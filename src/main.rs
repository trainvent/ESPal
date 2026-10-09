mod audio;

use std::{io::Read, time::Duration};

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    // Audio is allocated only after an explicit command. No networking at boot.
    log::info!("ESPal idle. Commands: help, stats, tone");
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
                            "stats: 1s mic levels with silent output; tone: 200ms quiet 440Hz tone"
                        ),
                        "stats" | "tone" => {
                            if audio.is_none() {
                                match audio::Diagnostics::new() {
                                    Ok(driver) => audio = Some(driver),
                                    Err(e) => log::error!("Audio init failed: {e}"),
                                }
                            }
                            if let Some(driver) = audio.as_mut() {
                                if let Err(e) = driver.run(command == "tone") {
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
