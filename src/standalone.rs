use crate::{audio::Diagnostics, config::Config, conversation, network::Network};
use espal_audio::wake::Detector;
use espal_core::recovery::Recovery;
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
                backoff = 1;
                let mut recovery = Recovery::default();
                if let Err(error) = conversation::retry_waiting_chime(audio) {
                    log::warn!("Initial waiting sound unavailable: {error}");
                }
                loop {
                    // Each attempt owns a fresh socket and audio queues. Failed
                    // microphone input and unfinished replies are never replayed.
                    let connected = network.connect_with_progress(config, || {
                        if let Err(error) = conversation::retry_waiting_chime(audio) {
                            log::warn!("Retry waiting sound unavailable: {error}");
                        }
                    });
                    let result = connected
                        .and_then(|_| conversation::session(audio, config, || recovery.reset()));
                    match result {
                        Ok(()) => break,
                        Err(error) => {
                            log::warn!("Conversation stopped: {error}");
                            let decision = recovery.failed(error);
                            if decision.error_sound {
                                log::info!("Recovery feedback: final error notification");
                                if let Err(chime_error) =
                                    conversation::connection_failure_chime(audio)
                                {
                                    log::warn!(
                                        "Connection failure sound unavailable: {chime_error}"
                                    );
                                }
                            }
                            let Some(delay) = decision.retry_after else {
                                log::info!("Recovery stopped; returning to Jarvis wake listening");
                                break;
                            };
                            log::info!(
                                "Reconnecting in {}s; wait for the ready beep",
                                delay.as_secs()
                            );
                            let deadline = Instant::now() + delay;
                            while Instant::now() < deadline {
                                let next_tick =
                                    (Instant::now() + Duration::from_secs(1)).min(deadline);
                                if let Err(chime_error) = conversation::retry_waiting_chime(audio) {
                                    log::warn!("Retry waiting sound unavailable: {chime_error}");
                                }
                                std::thread::sleep(
                                    next_tick.saturating_duration_since(Instant::now()),
                                );
                            }
                        }
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
