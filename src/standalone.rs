use espal_audio::wake::Detector;
use crate::{audio::Diagnostics, config::Config, conversation, network::Network};
use std::time::{Duration, Instant};

pub fn run(
    audio: &mut Diagnostics,
    network: &mut Network,
    config: &Config,
) -> Result<(), &'static str> {
    let mut detector = Detector::new()?;
    let mut backoff = 1u64;
    loop {
        match audio.wait_for_wake(&mut detector, Instant::now() + Duration::from_secs(86400)) {
            Ok(false) => continue,
            Ok(true) => {
                let result = network
                    .connect(config)
                    .and_then(|_| conversation::session(audio, config));
                match result {
                    Ok(()) => backoff = 1,
                    Err(e) => {
                        log::warn!(
                            "Conversation stopped: {e}; returning to wake mode after {backoff}s"
                        );
                        std::thread::sleep(Duration::from_secs(backoff));
                        backoff = (backoff * 2).min(30);
                    }
                }
            }
            Err(e) => {
                log::warn!("Wake capture stopped: {e}; retry in {backoff}s");
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(30);
            }
        }
    }
}
