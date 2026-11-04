use espal_audio::{audio, interruption, wake};
mod config;
mod conversation;
mod network;
mod standalone;
mod websocket;

use esp_idf_svc::{hal::peripherals::Peripherals, nvs::EspDefaultNvsPartition};
use std::{
    io::Read,
    time::{Duration, Instant},
};

fn export_recording(pcm: &[u8]) {
    export_pcm(pcm, "ESPAL_RECORD", 48000, 3);
}
fn export_pcm(pcm: &[u8], marker: &str, rate: usize, width: usize) {
    println!("{marker} {rate} {} {}", pcm.len() / width, pcm.len());
    let mut checksum = 2166136261u32;
    for (index, chunk) in pcm.chunks(48).enumerate() {
        let mut hex = String::with_capacity(100);
        hex.push_str("PCM ");
        for byte in chunk {
            use std::fmt::Write;
            write!(&mut hex, "{byte:02x}").unwrap();
            checksum = (checksum ^ u32::from(*byte)).wrapping_mul(16777619);
        }
        println!("{hex}");
        if index % 32 == 31 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    println!("{marker}_END {checksum:08x}");
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    let p = Peripherals::take().expect("Peripheral initialization failed");
    let mut hardware = Some((p.i2s0, p.pins));
    let mut modem = Some(p.modem);
    let partition = EspDefaultNvsPartition::take().expect("NVS partition initialization failed");
    let store = config::Store::new(partition.clone()).expect("Config store initialization failed");
    let mut config = match store.load() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("{e}");
            None
        }
    };
    let mut audio = None;
    let mut network = None;
    log::info!("ESPal idle. Commands: help, stats, tone, record, provision, connect, check, turn, pipeline, forget");
    log::info!("Credentials configured: {}. Standalone boot: {}; USB commands available for five seconds before wake mode", config.is_some(), config.as_ref().is_some_and(|c| c.standalone));
    // Nonblocking USB input permits autonomous startup with no host attached.
    unsafe {
        let mut usb = esp_idf_svc::sys::usb_serial_jtag_driver_config_t {
            tx_buffer_size: 1024,
            rx_buffer_size: 2048,
        };
        if esp_idf_svc::sys::usb_serial_jtag_driver_install(&mut usb) != esp_idf_svc::sys::ESP_OK {
            log::error!("USB console driver initialization failed");
            return;
        }
        esp_idf_svc::sys::usb_serial_jtag_vfs_use_driver();
        let flags = esp_idf_svc::sys::fcntl(0, esp_idf_svc::sys::F_GETFL as i32);
        if flags < 0
            || esp_idf_svc::sys::fcntl(
                0,
                esp_idf_svc::sys::F_SETFL as i32,
                flags | esp_idf_svc::sys::O_NONBLOCK as i32,
            ) < 0
        {
            log::error!("Cannot enable nonblocking USB input; standalone startup unavailable");
            return;
        }
    }
    let boot = Instant::now();
    let mut stdin = std::io::stdin();
    let mut line = Vec::with_capacity(2048);
    let mut overflow = false;
    loop {
        if boot.elapsed() >= Duration::from_secs(5) && config.as_ref().is_some_and(|c| c.standalone)
        {
            let result = (|| -> Result<(), &'static str> {
                if audio.is_none() {
                    let (i2s, pins) = hardware
                        .take()
                        .ok_or("Audio unavailable for standalone mode")?;
                    audio = Some(
                        audio::Diagnostics::new(i2s, pins)
                            .map_err(|_| "Audio initialization failed")?,
                    );
                }
                if network.is_none() {
                    network = Some(network::Network::new(
                        modem
                            .take()
                            .ok_or("Network unavailable for standalone mode")?,
                        partition.clone(),
                    )?);
                }
                standalone::run(
                    audio.as_mut().unwrap(),
                    network.as_mut().unwrap(),
                    config.as_ref().unwrap(),
                )
            })();
            if let Err(e) = result {
                log::error!("Standalone startup failed: {e}");
                // Keep USB recovery available rather than spinning on a missing model.
                if let Some(c) = config.as_mut() {
                    c.standalone = false;
                }
            }
        }
        let mut byte = [0];
        match stdin.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' || byte[0] == b'\r' => {
                if overflow {
                    log::warn!("Command too long");
                } else if !line.is_empty() {
                    let command = std::str::from_utf8(&line).unwrap_or("").trim();
                    if let Some(json) = command.strip_prefix("provision ") {
                        match store.provision(json) {
                            Ok(c) => {
                                if let Some(n) = network.as_mut() {
                                    network::Network::stop(n);
                                }
                                config = Some(c);
                                println!("ESPAL_PROVISIONED");
                            }
                            Err(e) => log::error!("Provisioning failed: {e}"),
                        }
                    } else if let Some(json) = command.strip_prefix("wifi ") {
                        match store.set_wifi(json) {
                            Ok(c) => {
                                if let Some(n) = network.as_mut() {
                                    network::Network::stop(n);
                                }
                                config = Some(c);
                                println!("ESPAL_WIFI_SAVED");
                            }
                            Err(e) => log::error!("Wi-Fi setting failed: {e}"),
                        }
                    } else if let Some(mode) = command.strip_prefix("standalone ") {
                        match mode {
                            "on" | "off" => match store.set_standalone(mode == "on") {
                                Ok(c) => {
                                    config = Some(c);
                                    println!("ESPAL_STANDALONE {mode}");
                                }
                                Err(e) => log::error!("Standalone setting failed: {e}"),
                            },
                            _ => log::error!("Use standalone on or standalone off"),
                        }
                    } else if let Some(voice) = command.strip_prefix("voice ") {
                        match store.set_voice(voice) {
                            Ok(c) => {
                                println!("ESPAL_VOICE {}", c.voice);
                                config = Some(c);
                            }
                            Err(e) => log::error!("Voice change failed: {e}"),
                        }
                    } else {
                        let result = (|| -> Result<(), &'static str> {
                            match command {
                                "help"=>log::info!("stats, tone, record (5s); connect (Wi-Fi/time); check (OpenAI session); pipeline (filtered tone); turn (speech-ended request); standalone on/off; wake-check; wake-test; forget"),
                                "forget"=>{store.forget()?;config=None;if let Some(n)=network.as_mut(){network::Network::stop(n);}log::info!("Credentials removed");},
                                "connect" | "check" | "turn" | "turn-capture" | "interrupt-network-check"=>{
                                    let c=config.as_ref().ok_or("Run tools/provision.py first")?;
                                    if network.is_none() {
                                        network=Some(network::Network::new(modem.take().ok_or("Network initialization requires restart")?,partition.clone())?);
                                    }
                                    network.as_mut().unwrap().connect(c)?;
                                    if command=="check" { conversation::check(c)?; }
                                    if command=="turn" || command=="turn-capture" || command=="interrupt-network-check" {
                                        if audio.is_none() {
                                            let (i2s,pins)=hardware.take().ok_or("Audio initialization requires restart")?;
                                            audio=Some(audio::Diagnostics::new(i2s,pins).map_err(|_|"Audio initialization failed")?);
                                        }
                                        if command=="interrupt-network-check" {
                                            conversation::interruption_network_test(audio.as_mut().unwrap(),c)?;
                                        } else if command=="turn-capture" {
                                            let pcm=conversation::capture_reply(audio.as_mut().unwrap(),c)?;
                                            export_pcm(&pcm,"ESPAL_REPLY",24000,2);
                                        } else { conversation::turn(audio.as_mut().unwrap(),c)?; }
                                    }
                                },
                                "tone16" | "tone-pico"=>{
                                    let (i2s,pins)=hardware.take().ok_or("Reset before the isolated tone test")?;
                                    audio::Diagnostics::tone_tx(i2s,pins,command=="tone-pico").map_err(|e|{log::error!("TX-only tone driver error: {e}");"TX-only tone failed; reset before further audio use"})?;
                                },
                                "stats" | "tone" | "failure-sound" | "waiting-sound" | "record" | "pipeline" | "loopback" | "input-test" | "wake-check" | "wake-test" | "aec-check" | "interrupt-check"=>{
                                    if audio.is_none() {
                                        let (i2s,pins)=hardware.take().ok_or("Audio initialization requires restart")?;
                                        audio=Some(audio::Diagnostics::new(i2s,pins).map_err(|_|"Audio initialization failed")?);
                                    }
                                    let driver=audio.as_mut().unwrap();
                                    if command=="waiting-sound" {
                                        conversation::retry_waiting_chime(driver)?;
                                        log::info!("Retry waiting sound complete");
                                    } else if command=="failure-sound" {
                                        conversation::connection_failure_chime(driver)?;
                                        log::info!("Connection failure sound complete");
                                    } else if command=="wake-check" || command=="wake-test" {
                                        let mut detector=wake::Detector::new()?;
                                        if command=="wake-check" {
                                            for _ in 0..2 {
                                                for _ in 0..2048 {
                                                    if detector.push(0)? { return Err("Wake self-check triggered on silence"); }
                                                }
                                                detector.reset()?;
                                            }
                                        }
                                        if command=="wake-test" {
                                            if !driver.wait_for_wake(&mut detector,Instant::now()+Duration::from_secs(30))? { return Err("Wake test timed out"); }
                                            conversation::ready_chime(driver)?;
                                        }
                                        log::info!("Wake diagnostic complete");
                                    } else if command=="interrupt-check" {
                                        conversation::interruption_test(driver)?;
                                    } else if command=="aec-check" {
                                        interruption::check()?;
                                    } else if command=="input-test" {
                                        conversation::input_test(driver)?;
                                    } else if command=="loopback" {
                                        driver.loopback()?;
                                    } else if command=="pipeline" {
                                        conversation::pipeline_test(driver)?;
                                    } else if command=="record" {
                                        log::info!("Recording 5 seconds now; speaker silent");
                                        export_recording(&driver.record().map_err(|_|"Recording failed")?);
                                    } else {driver.run(command=="tone").map_err(|_|"Audio diagnostic failed")?;}
                                },
                                _=>log::warn!("Unknown command; use help"),
                            }
                            Ok(())
                        })();
                        if let Err(e) = result {
                            log::error!("{e}");
                        }
                    }
                }
                // Wipe provisioning input before reusing the bounded buffer.
                line.fill(0);
                line.clear();
                overflow = false;
            }
            Ok(1) if line.len() < 2048 && !overflow => line.push(byte[0]),
            Ok(1) => overflow = true,
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
