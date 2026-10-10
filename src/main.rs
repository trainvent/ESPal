mod audio;
mod config;
mod conversation;
mod network;
mod websocket;

use esp_idf_svc::{hal::peripherals::Peripherals, nvs::EspDefaultNvsPartition};
use std::{io::Read, time::Duration};

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
    log::info!(
        "Credentials configured: {}. No audio or networking at boot",
        config.is_some()
    );
    let mut stdin = std::io::stdin();
    let mut line = Vec::with_capacity(2048);
    let mut overflow = false;
    loop {
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
                    } else {
                        let result = (|| -> Result<(), &'static str> {
                            match command {
                                "help"=>log::info!("stats, tone, record (5s); connect (Wi-Fi/time); check (OpenAI session); pipeline (filtered tone); turn (5s spoken request); forget"),
                                "forget"=>{store.forget()?;config=None;if let Some(n)=network.as_mut(){network::Network::stop(n);}log::info!("Credentials removed");},
                                "connect" | "check" | "turn" | "turn-capture"=>{
                                    let c=config.as_ref().ok_or("Run tools/provision.py first")?;
                                    if network.is_none() {
                                        network=Some(network::Network::new(modem.take().ok_or("Network initialization requires restart")?,partition.clone())?);
                                    }
                                    network.as_mut().unwrap().connect(c)?;
                                    if command=="check" { conversation::check(c)?; }
                                    if command=="turn" || command=="turn-capture" {
                                        if audio.is_none() {
                                            let (i2s,pins)=hardware.take().ok_or("Audio initialization requires restart")?;
                                            audio=Some(audio::Diagnostics::new(i2s,pins).map_err(|_|"Audio initialization failed")?);
                                        }
                                        if command=="turn-capture" {
                                            let pcm=conversation::capture_reply(audio.as_mut().unwrap(),c)?;
                                            export_pcm(&pcm,"ESPAL_REPLY",24000,2);
                                        } else { conversation::turn(audio.as_mut().unwrap(),c)?; }
                                    }
                                },
                                "stats" | "tone" | "record" | "pipeline"=>{
                                    if audio.is_none() {
                                        let (i2s,pins)=hardware.take().ok_or("Audio initialization requires restart")?;
                                        audio=Some(audio::Diagnostics::new(i2s,pins).map_err(|_|"Audio initialization failed")?);
                                    }
                                    let driver=audio.as_mut().unwrap();
                                    if command=="pipeline" {
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
