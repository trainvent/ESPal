mod audio;

use std::{io::Read, time::Duration};

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    // Audio is allocated only after an explicit command. No networking at boot.
    log::info!("ESPal idle. Commands: help, stats, tone, record");
    let mut audio = None;
    let mut stdin = std::io::stdin();
    let mut line = Vec::with_capacity(64);
    let mut overflow = false;
    loop {
        let mut byte = [0];
        match stdin.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' || byte[0] == b'\r' => {
                if overflow {
                    log::warn!("Command too long");
                } else if !line.is_empty() {
                    let command = std::str::from_utf8(&line).unwrap_or("").trim();
                    match command {
                        "help" => log::info!(
                            "stats: 1s mic levels; tone: quiet 440Hz; record: 5s PCM24 USB export"
                        ),
                        "stats" | "tone" | "record" => {
                            if audio.is_none() {
                                match audio::Diagnostics::new() {
                                    Ok(driver) => audio = Some(driver),
                                    Err(e) => log::error!("Audio init failed: {e}"),
                                }
                            }
                            if let Some(driver) = audio.as_mut() {
                                if command == "record" {
                                    log::info!("Recording 5 seconds now; speaker silent");
                                    match driver.record() {
                                        Ok(pcm) => {
                                            println!("ESPAL_RECORD 48000 240000 {}", pcm.len());
                                            let mut checksum = 2166136261u32;
                                            for (index, chunk) in pcm.chunks(48).enumerate() {
                                                let mut hex = String::with_capacity(100);
                                                hex.push_str("PCM ");
                                                for byte in chunk {
                                                    use std::fmt::Write;
                                                    write!(&mut hex, "{byte:02x}").unwrap();
                                                    checksum = (checksum ^ u32::from(*byte))
                                                        .wrapping_mul(16777619);
                                                }
                                                println!("{hex}");
                                                // USB's polled VFS can spin while the host drains
                                                // data. Let the FreeRTOS idle task service WDT.
                                                if index % 32 == 31 {
                                                    std::thread::sleep(Duration::from_millis(10));
                                                }
                                            }
                                            println!("ESPAL_RECORD_END {checksum:08x}");
                                        }
                                        Err(e) => log::error!("Recording failed: {e}"),
                                    }
                                } else if let Err(e) = driver.run(command == "tone") {
                                    log::error!("Audio diagnostic failed: {e}");
                                }
                            }
                        }
                        _ => log::warn!("Unknown command; use help"),
                    }
                }
                line.clear();
                overflow = false;
            }
            Ok(1) if line.len() < 64 && !overflow => line.push(byte[0]),
            Ok(1) => overflow = true,
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
