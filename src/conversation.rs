use crate::{
    audio::{Diagnostics, Playback},
    config::Config,
    websocket::Socket,
};
use espal_core::{dsp::Downsampler, realtime};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

fn check_error(event: &serde_json::Value) -> Result<(), &'static str> {
    // Do not log server error text: it can include user data or credentials.
    if event["type"] == "error" {
        return Err("OpenAI returned an error; check model access, key and limits");
    }
    Ok(())
}
/// Quiet local check of the same filters, duplex and playback queue used by
/// Realtime, without networking or microphone upload.
pub fn pipeline_test(audio: &mut Diagnostics) -> Result<(), &'static str> {
    let mut down = Downsampler::new(1);
    let mut pcm = Vec::with_capacity(57600);
    for index in 0..57600 {
        let t = index as f32 / 48000.0;
        let envelope = (t / 0.01).min((1.2 - t) / 0.01).clamp(0.0, 1.0);
        let sample = ((t * 440.0 * std::f32::consts::TAU).sin() * envelope * 327.0) as i32;
        if let Some(v) = down.push_word(sample << 16) {
            pcm.extend_from_slice(&v.to_le_bytes());
        }
    }
    let (sender, receiver) = mpsc::sync_channel(2);
    let queued = AtomicUsize::new(pcm.len());
    let cancel = AtomicBool::new(false);
    sender
        .send(Playback::Audio(pcm))
        .map_err(|_| "Pipeline queue failed")?;
    sender
        .send(Playback::End)
        .map_err(|_| "Pipeline queue failed")?;
    audio.playback(
        receiver,
        &queued,
        &cancel,
        Instant::now() + Duration::from_secs(5),
    )?;
    if queued.load(Ordering::Acquire) != 0 {
        return Err("Pipeline did not drain");
    }
    log::info!("Filtered pipeline test complete: 48→24→48kHz, queue drained");
    Ok(())
}

fn open_session(config: &Config, deadline: Instant) -> Result<Socket, &'static str> {
    log::info!("Connecting OpenAI with certificate validation");
    let mut socket = Socket::connect(config)?;
    socket.send(&realtime::session_update(
        &config.voice,
        &config.instructions,
    ))?;
    loop {
        let event = socket.receive(deadline)?;
        check_error(&event)?;
        if event["type"] == "session.updated" {
            break;
        }
    }
    Ok(socket)
}

pub fn check(config: &Config) -> Result<(), &'static str> {
    let _socket = open_session(config, Instant::now() + Duration::from_secs(25))?;
    log::info!("OpenAI session ready; authentication and audio configuration verified");
    Ok(())
}

/// Keep capture, filtering and upload independent. Both queues hold at most
/// two seconds; overflow cancels the input instead of dropping speech.
fn stream_input(
    audio: &mut Diagnostics,
    gain: u8,
    deadline: Instant,
    mut upload: impl FnMut(&[u8]) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    // app_main runs at priority 1, whereas Rust worker threads default to 5.
    // Give the uploader equal scheduling priority while this pipeline runs;
    // restore its original priority on every exit path.
    struct UploadPriority(esp_idf_svc::sys::UBaseType_t);
    impl Drop for UploadPriority {
        fn drop(&mut self) {
            unsafe { esp_idf_svc::sys::vTaskPrioritySet(std::ptr::null_mut(), self.0) };
        }
    }
    let original_priority = unsafe { esp_idf_svc::sys::uxTaskPriorityGet(std::ptr::null_mut()) };
    let _priority = UploadPriority(original_priority);
    unsafe { esp_idf_svc::sys::vTaskPrioritySet(std::ptr::null_mut(), original_priority.max(5)) };
    let (raw_sender, raw_receiver) = mpsc::sync_channel::<Vec<i32>>(40);
    let (pcm_sender, pcm_receiver) = mpsc::sync_channel::<Vec<u8>>(20);
    let raw_queued = AtomicUsize::new(0);
    let pcm_queued = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    std::thread::scope(|scope| {
        let raw_ref = &raw_queued;
        let pcm_ref = &pcm_queued;
        let cancel_ref = &cancel;
        let capture = std::thread::Builder::new()
            .name("microphone".into())
            .stack_size(8192)
            .spawn_scoped(scope, move || {
                let result = audio.stream_capture(raw_sender, raw_ref, cancel_ref, deadline);
                if result.is_err() {
                    cancel_ref.store(true, Ordering::Release);
                }
                result
            })
            .map_err(|_| "Microphone task failed")?;
        let filter = std::thread::Builder::new()
            .name("input-filter".into()).stack_size(8192)
            .spawn_scoped(scope, move || {
                let result = (|| {
                    let mut down = Downsampler::new(gain);
                    let mut chunk = Vec::with_capacity(4800);
                    let mut samples = 0usize;
                    let mut max_queued = 0usize;
                    loop {
                        if cancel_ref.load(Ordering::Acquire) || Instant::now() >= deadline {
                            return Err("Input filter cancelled");
                        }
                        let block = match raw_receiver.recv_timeout(Duration::from_millis(50)) {
                            Ok(block) => block,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        };
                        let len = block.len();
                        for word in block {
                            samples += 1;
                            if let Some(sample) = down.push_word(word) {
                                chunk.extend_from_slice(&sample.to_le_bytes());
                            }
                            if chunk.len() == 4800 {
                                let pending = pcm_ref.fetch_add(chunk.len(), Ordering::AcqRel) + chunk.len();
                                max_queued = max_queued.max(pending);
                                if pcm_sender.try_send(std::mem::replace(&mut chunk, Vec::with_capacity(4800))).is_err() {
                                    pcm_ref.fetch_sub(4800, Ordering::AcqRel);
                                    return Err("Input upload buffer overflow or uploader stopped; input cancelled");
                                }
                                // A full RTOS tick lets Wi-Fi and idle tasks run.
                                std::thread::sleep(Duration::from_millis(10));
                            }
                        }
                        raw_ref.fetch_sub(len, Ordering::AcqRel);
                    }
                    if samples != 240_000 || !chunk.is_empty() {
                        return Err("Input stream incomplete; turn not committed");
                    }
                    Ok((down.clipped, max_queued))
                })();
                if result.is_err() { cancel_ref.store(true, Ordering::Release); }
                result
            });
        let filter = match filter {
            Ok(filter) => filter,
            Err(_) => {
                cancel.store(true, Ordering::Release);
                let _ = capture.join();
                return Err("Input filter task failed");
            }
        };
        let mut uploaded = 0usize;
        let sent = (|| {
            loop {
                if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err("Input upload cancelled");
                }
                match pcm_receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(chunk) => {
                        upload(&chunk)?;
                        uploaded += chunk.len();
                        pcm_queued.fetch_sub(chunk.len(), Ordering::AcqRel);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            Ok(())
        })();
        if sent.is_err() {
            cancel.store(true, Ordering::Release);
        }
        log::info!("Input upload: uploaded_bytes={uploaded} elapsed_ms={} raw_pending_samples={} pcm_pending_bytes={}", started.elapsed().as_millis(), raw_queued.load(Ordering::Acquire), pcm_queued.load(Ordering::Acquire));
        drop(pcm_receiver);
        let captured = capture.join().map_err(|_| "Microphone task panicked")?;
        let converted = filter.join().map_err(|_| "Input filter task panicked")?;
        if let Err(e) = sent {
            if e != "Input upload cancelled" {
                return Err(e);
            }
        }
        // Preserve a concrete worker error rather than its cancellation fallout.
        if let Err(e) = captured {
            if e != "Microphone streaming cancelled" {
                return Err(e);
            }
        }
        if let Err(e) = converted {
            if e != "Input filter cancelled" {
                return Err(e);
            }
        }
        sent?;
        let (samples, capture_ms, raw_max) = captured?;
        let (clipped, pcm_max) = converted?;
        if uploaded != 240_000
            || raw_queued.load(Ordering::Acquire) != 0
            || pcm_queued.load(Ordering::Acquire) != 0
        {
            return Err("Input upload incomplete; turn not committed");
        }
        let elapsed = started.elapsed().as_millis();
        log::info!("Input streaming: capture_samples={samples} capture_ms={capture_ms} uploaded_bytes={uploaded} total_ms={elapsed} drain_ms={} max_raw_buffer_ms={} max_pcm_buffer_ms={} clipped_samples={clipped}", elapsed.saturating_sub(capture_ms), raw_max / 48, pcm_max / 48);
        Ok(())
    })
}

/// Exercise the real capture/filter queues with a deliberately slower consumer,
/// without sending audio over the network or playing it through the speaker.
pub fn input_test(audio: &mut Diagnostics) -> Result<(), &'static str> {
    stream_input(audio, 1, Instant::now() + Duration::from_secs(20), |_| {
        std::thread::sleep(Duration::from_millis(120));
        Ok(())
    })?;
    let failure = stream_input(audio, 1, Instant::now() + Duration::from_secs(5), |_| {
        Err("Injected upload failure")
    });
    if failure != Err("Injected upload failure") {
        return Err("Input cancellation test failed");
    }
    audio
        .run(false)
        .map_err(|_| "Capture driver unavailable after cancellation")?;
    log::info!("Input streaming test complete; five seconds captured and queues drained, no network upload");
    Ok(())
}

fn request(audio: &mut Diagnostics, config: &Config) -> Result<(Socket, Instant), &'static str> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut socket = open_session(config, deadline)?;
    stream_input(audio, config.mic_gain, deadline, |chunk| {
        socket.send(&realtime::append(chunk))
    })?;
    socket.send(&json!({"type":"input_audio_buffer.commit"}))?;
    socket.send(&json!({"type":"response.create"}))?;
    Ok((socket, deadline))
}

pub fn capture_reply(audio: &mut Diagnostics, config: &Config) -> Result<Vec<u8>, &'static str> {
    let (mut socket, deadline) = request(audio, config)?;
    let mut pcm = Vec::new();
    loop {
        let event = socket.receive(deadline)?;
        check_error(&event)?;
        if let Some(bytes) = realtime::audio_delta(&event)? {
            if pcm.len() + bytes.len() > 960000 {
                return Err("Reply capture exceeds twenty seconds");
            }
            pcm.try_reserve(bytes.len())
                .map_err(|_| "Reply capture allocation failed")?;
            pcm.extend(bytes);
        }
        if event["type"] == "response.done" {
            if event["response"]["status"] != "completed" || pcm.is_empty() {
                return Err("Reply capture did not complete");
            }
            return Ok(pcm);
        }
    }
}

pub fn turn(audio: &mut Diagnostics, config: &Config) -> Result<(), &'static str> {
    let (mut socket, deadline) = request(audio, config)?;
    log::info!("Waiting for spoken response; microphone upload gated");
    let (sender, receiver) = mpsc::sync_channel(512);
    let queued = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);
    let mut received_bytes = 0usize;
    let mut max_queued = 0usize;
    let result = std::thread::scope(|scope| {
        let queued_ref = &queued;
        let cancel_ref = &cancel;
        let worker = std::thread::Builder::new()
            .name("speaker".into())
            .stack_size(8192)
            .spawn_scoped(scope, move || {
                audio.playback(receiver, queued_ref, cancel_ref, deadline)
            })
            .map_err(|_| "Speaker task failed")?;
        let stream = (|| {
            loop {
                let event = socket.receive(deadline)?;
                check_error(&event)?;
                if let Some(bytes) = realtime::audio_delta(&event)? {
                    if worker.is_finished() {
                        return Err("Speaker stopped before response completion");
                    }
                    // Allow response-generation bursts ahead of real-time playback.
                    // Twenty seconds of PCM (960KB) remains bounded and fits PSRAM.
                    received_bytes += bytes.len();
                    let previous = queued.fetch_add(bytes.len(), Ordering::AcqRel);
                    max_queued = max_queued.max(previous + bytes.len());
                    if previous + bytes.len() > 960000 {
                        return Err("Playback overflow; response cancelled");
                    }
                    sender
                        .try_send(Playback::Audio(bytes))
                        .map_err(|_| "Playback queue overflow; response cancelled")?;
                }
                if event["type"] == "response.done" {
                    if event["response"]["status"] != "completed" {
                        return Err("OpenAI response did not complete");
                    }
                    sender
                        .try_send(Playback::End)
                        .map_err(|_| "Playback completion queue overflow")?;
                    return Ok(());
                }
            }
        })();
        if stream.is_err() {
            cancel.store(true, Ordering::Release);
            let _ = socket.send(&json!({"type":"response.cancel"}));
        }
        drop(sender);
        let played = worker.join().map_err(|_| "Speaker task panicked")?;
        stream.and(played)
    });
    log::info!("Response audio: received_bytes={received_bytes} max_queued_bytes={max_queued}");
    if result.is_ok() {
        log::info!("Turn finished; audio stopped");
    }
    result
}
