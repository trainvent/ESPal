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

struct AudioPriority(esp_idf_svc::sys::UBaseType_t);
struct SpeakerScheduling(esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration);
impl SpeakerScheduling {
    fn acquire() -> Result<Self, &'static str> {
        use esp_idf_svc::hal::{cpu::Core, task::thread::ThreadSpawnConfiguration};
        let previous = ThreadSpawnConfiguration::get().unwrap_or_default();
        ThreadSpawnConfiguration {
            pin_to_core: Some(Core::Core0),
            priority: 5,
            ..ThreadSpawnConfiguration::default()
        }.set().map_err(|_| "Speaker scheduling configuration failed")?;
        Ok(Self(previous))
    }
}
impl Drop for SpeakerScheduling {
    fn drop(&mut self) { let _ = self.0.set(); }
}
impl AudioPriority {
    fn acquire() -> Self {
        Self::with_priority(5)
    }
    fn with_priority(priority: esp_idf_svc::sys::UBaseType_t) -> Self {
        let original = unsafe { esp_idf_svc::sys::uxTaskPriorityGet(std::ptr::null_mut()) };
        unsafe { esp_idf_svc::sys::vTaskPrioritySet(std::ptr::null_mut(), original.max(priority)) };
        Self(original)
    }
}
impl Drop for AudioPriority {
    fn drop(&mut self) {
        unsafe { esp_idf_svc::sys::vTaskPrioritySet(std::ptr::null_mut(), self.0) };
    }
}

fn check_error(event: &serde_json::Value) -> Result<(), &'static str> {
    // Do not log server error text: it can include user data or credentials.
    if event["type"] == "error" {
        return Err(espal_core::recovery::server_error(&event["error"]));
    }
    Ok(())
}
/// Quiet local check of the same filters, duplex and playback queue used by
/// Realtime, without networking or microphone upload.
pub fn pipeline_test(audio: &mut Diagnostics) -> Result<(), &'static str> {
    pipeline_duration(audio, 1.2)
}
pub fn interruption_test(audio: &mut Diagnostics) -> Result<(), &'static str> {
    pipeline_duration(audio, 12.0)?;
    log::info!("Interruption diagnostic complete");
    Ok(())
}
pub fn interruption_network_test(
    audio: &mut Diagnostics,
    config: &Config,
) -> Result<(), &'static str> {
    let mut socket = open_session(config, Instant::now() + Duration::from_secs(60))?;
    // Cover the capture/filter/upload closure used after the standalone chime.
    // Discard this diagnostic input before requesting the fixed spoken reply.
    stream_input(audio, config.mic_gain, false, Instant::now() + Duration::from_secs(20), &mut |chunk| {
        socket.append_audio(chunk)
    })?;
    socket.send(&json!({"type":"input_audio_buffer.clear"}))?;
    log::info!("Network microphone upload diagnostic complete");
    interruption_test(audio)?;
    // Prove the live TLS connection remains usable after concurrent processing.
    socket.send(&realtime::session_update(
        &config.voice,
        &config.instructions,
    ))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let event = socket.receive(deadline)?;
        check_error(&event)?;
        if event["type"] == "session.updated" {
            break;
        }
    }
    // Exercise real incoming audio and a second AEC initialization on the same
    // TLS connection. Send a fixed text prompt; no microphone upload occurs.
    let _idle_detector = espal_audio::wake::Detector::new()?;
    socket.send(&json!({"type":"conversation.item.create", "item":{
        "type":"message", "role":"user", "content":[{
            "type":"input_text", "text":"Count aloud from one to twenty at a natural pace, without any introduction."
        }]
    }}))?;
    socket.send(&json!({"type":"response.create"}))?;
    if play_response(audio, &mut socket, Instant::now() + Duration::from_secs(45))? {
        return Err("Network diagnostic reply was interrupted");
    }
    log::info!("Network interruption diagnostic complete");
    Ok(())
}
fn pipeline_duration(audio: &mut Diagnostics, seconds: f32) -> Result<(), &'static str> {
    // Retain an idle detector while the playback detector comes and goes,
    // exercising the shared ESP-SR model registry used in standalone mode.
    let mut idle_detector = espal_audio::wake::Detector::new()?;
    let interruption = espal_audio::interruption::Signals::default();
    let mut down = Downsampler::new(1);
    let count = (seconds * 48000.0) as usize;
    let mut pcm = Vec::with_capacity(count);
    for index in 0..count {
        let t = index as f32 / 48000.0;
        let envelope = (t / 0.01).min((seconds - t) / 0.01).clamp(0.0, 1.0);
        let sample = ((t * 440.0 * std::f32::consts::TAU).sin() * envelope * 327.0) as i32;
        if let Some(v) = down.push_word(sample << 16) {
            pcm.extend_from_slice(&v.to_le_bytes());
        }
        if index % 4800 == 0 {
            std::thread::sleep(Duration::from_millis(10));
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
    // Match the real response path: DMA servicing must run at audio-task
    // priority, rather than the low-priority USB command/main task.
    let _speaker_scheduling = SpeakerScheduling::acquire()?;
    std::thread::scope(|scope| {
        let queued_ref = &queued;
        let cancel_ref = &cancel;
        let interruption_ref = &interruption;
        std::thread::Builder::new()
            .name("speaker-test".into())
            .stack_size(8192)
            .spawn_scoped(scope, move || {
                audio.playback_interruptible(
                    receiver,
                    queued_ref,
                    cancel_ref,
                    Instant::now() + Duration::from_secs(30),
                    Some(interruption_ref),
                )
            })
            .map_err(|_| "Diagnostic speaker task failed")?
            .join()
            .map_err(|_| "Diagnostic speaker task panicked")?
    })?;
    if queued.load(Ordering::Acquire) != 0 {
        return Err("Pipeline did not drain");
    }
    if interruption.detected.load(Ordering::Acquire) {
        return Err("Playback tone falsely triggered Jarvis");
    }
    for _ in 0..2048 {
        if idle_detector.push(0)? {
            return Err("Idle wake detector falsely triggered after playback");
        }
    }
    idle_detector.reset()?;
    log::info!("Filtered pipeline test complete: 48→24→48kHz, queue drained");
    Ok(())
}

fn open_session(config: &Config, deadline: Instant) -> Result<Box<Socket>, &'static str> {
    log::info!("Connecting OpenAI with certificate validation");
    let mut socket = Box::new(Socket::connect(&config.model, &config.api_key)?);
    let instructions = format!("{}\nYour name is Jarvis. When asked your name, identify yourself as Jarvis. Cedar is your voice setting, not your name.", config.instructions);
    socket.send(&realtime::session_update(
        &config.voice,
        &instructions,
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
#[inline(never)]
fn stream_input(
    audio: &mut Diagnostics,
    gain: u8,
    speech: bool,
    deadline: Instant,
    upload: &mut dyn FnMut(&[u8]) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    // app_main runs at priority 1, whereas Rust worker threads default to 5.
    // Give the uploader equal scheduling priority while this pipeline runs;
    // restore its original priority on every exit path.
    let _priority = AudioPriority::acquire();
    // Keep the continuous capture/filter workers off Wi-Fi's core. Long
    // utterances cannot rely on accumulating a growing processing backlog.
    use esp_idf_svc::hal::{cpu::Core, task::thread::ThreadSpawnConfiguration};
    struct RestoreSpawnConfig(ThreadSpawnConfiguration);
    impl Drop for RestoreSpawnConfig {
        fn drop(&mut self) {
            let _ = self.0.set();
        }
    }
    let _spawn_config = RestoreSpawnConfig(ThreadSpawnConfiguration::get().unwrap_or_default());
    ThreadSpawnConfiguration {
        pin_to_core: Some(Core::Core1),
        ..ThreadSpawnConfiguration::default()
    }
    .set()
    .map_err(|_| "Input scheduling configuration failed")?;
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
                let result =
                    audio.stream_capture(raw_sender, raw_ref, cancel_ref, deadline, speech);
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
                    if !chunk.is_empty() {
                        let len = chunk.len();
                        let pending = pcm_ref.fetch_add(len, Ordering::AcqRel) + len;
                        max_queued = max_queued.max(pending);
                        if pcm_sender.try_send(chunk).is_err() {
                            pcm_ref.fetch_sub(len, Ordering::AcqRel);
                            return Err("Final input upload buffer overflow");
                        }
                    }
                    Ok((down.clipped, max_queued, samples))
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
        let (clipped, pcm_max, filtered_samples) = converted?;
        if uploaded != samples
            || filtered_samples != samples
            || samples == 0
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
    stream_input(
        audio,
        1,
        false,
        Instant::now() + Duration::from_secs(20),
        &mut |_| {
            std::thread::sleep(Duration::from_millis(120));
            Ok(())
        },
    )?;
    let failure = stream_input(
        audio,
        1,
        false,
        Instant::now() + Duration::from_secs(5),
        &mut |_| Err("Injected upload failure"),
    );
    if failure != Err("Injected upload failure") {
        return Err("Input cancellation test failed");
    }
    audio
        .run(false)
        .map_err(|_| "Capture driver unavailable after cancellation")?;
    log::info!("Input streaming test complete; five seconds captured and queues drained, no network upload");
    Ok(())
}

fn request(audio: &mut Diagnostics, config: &Config) -> Result<(Box<Socket>, Instant), &'static str> {
    let deadline = Instant::now() + Duration::from_secs(660);
    let mut socket = open_session(config, deadline)?;
    stream_input(audio, config.mic_gain, true, deadline, &mut |chunk| {
        socket.append_audio(chunk)
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
    play_response(audio, &mut socket, deadline).map(|_| ())
}

pub fn ready_chime(audio: &mut Diagnostics) -> Result<(), &'static str> {
    let mut pcm = Vec::with_capacity(4800);
    for index in 0..2400 {
        let phase = index % 24;
        let ramp = if phase < 12 { phase } else { 24 - phase };
        let sample = ((ramp as i32 - 6) * 1600 / 6) as i16;
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    play_chime(audio, pcm)
}

pub fn connection_failure_chime(audio: &mut Diagnostics) -> Result<(), &'static str> {
    let mut pcm = include_bytes!("../assets/connection-error.pcm").to_vec();
    for sample in pcm.chunks_exact_mut(2) {
        let quieter = i16::from_le_bytes([sample[0], sample[1]]) / 2;
        sample.copy_from_slice(&quieter.to_le_bytes());
    }
    play_chime(audio, pcm)
}

pub fn retry_waiting_chime(audio: &mut Diagnostics) -> Result<(), &'static str> {
    // Two quiet, steady ticks distinguish retry waiting from the ready beep.
    let mut pcm = Vec::with_capacity(9600);
    for index in 0..4800 {
        let tick = index % 3360;
        let sample = if tick < 1440 {
            let envelope = (tick as f32 / 240.0)
                .min((1439 - tick) as f32 / 240.0)
                .min(1.0);
            let phase = std::f32::consts::TAU * 600.0 * tick as f32 / 24000.0;
            (phase.sin() * 1400.0 * envelope) as i16
        } else {
            0
        };
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    play_chime(audio, pcm)
}

fn play_chime(audio: &mut Diagnostics, pcm: Vec<u8>) -> Result<(), &'static str> {
    let (sender, receiver) = mpsc::sync_channel(2);
    let queued = AtomicUsize::new(pcm.len());
    let cancel = AtomicBool::new(false);
    sender
        .send(Playback::Audio(pcm))
        .map_err(|_| "Chime queue failed")?;
    sender
        .send(Playback::End)
        .map_err(|_| "Chime queue failed")?;
    audio.playback(
        receiver,
        &queued,
        &cancel,
        Instant::now() + Duration::from_secs(5),
    )
}

pub fn session(
    audio: &mut Diagnostics,
    config: &Config,
    on_ready: impl FnOnce(),
) -> Result<(), &'static str> {
    let deadline = Instant::now() + Duration::from_secs(660);
    let mut socket = open_session(config, Instant::now() + Duration::from_secs(25))?;
    ready_chime(audio)?;
    on_ready();
    log::info!("Conversation ready; speak after the beep");
    while Instant::now() < deadline {
        let input = stream_input(audio, config.mic_gain, true, deadline, &mut |chunk| {
            socket.append_audio(chunk)
        });
        if input == Err("No speech detected") {
            log::info!("Conversation idle; returning to offline wake detection");
            return Ok(());
        }
        input?;
        socket.send(&json!({"type":"input_audio_buffer.commit"}))?;
        socket.send(&json!({"type":"response.create"}))?;
        if play_response(audio, &mut socket, deadline)? {
            ready_chime(audio)?;
            log::info!("Interruption accepted; speak after the beep");
        }
    }
    Ok(())
}

fn play_response(
    audio: &mut Diagnostics,
    socket: &mut Socket,
    deadline: Instant,
) -> Result<bool, &'static str> {
    // Wake detection takes precedence over parsing buffered response events.
    let _priority = AudioPriority::with_priority(4);
    log::info!("Waiting for spoken response; microphone upload gated");
    // Generation can outrun the speaker for an entire long answer. Budget the
    // pending PCM against currently available PSRAM, after the wake model and
    // TLS session have been allocated. Keep 2MiB for prepared audio, WebSocket
    // messages and other allocations; never consume the whole heap.
    let free_psram =
        unsafe { esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_SPIRAM) };
    let queue_limit = (48_000 * 60).min(free_psram.saturating_sub(2 * 1024 * 1024));
    if queue_limit < 48_000 {
        return Err("Insufficient memory for response buffering");
    }
    log::info!(
        "Reply buffer: capacity_ms={} free_psram_bytes={free_psram}",
        queue_limit / 48
    );
    // Leave enough message slots for small deltas within the larger byte budget.
    let (sender, receiver) = mpsc::sync_channel(2048);
    let queued = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);
    let mut received_bytes = 0usize;
    let mut max_queued = 0usize;
    let interruption = espal_audio::interruption::Signals::default();
    let mut response_done = false;
    let mut item_id: Option<String> = None;
    let mut content_index = 0usize;
    let mut yield_bytes = 0usize;
    let _speaker_scheduling = SpeakerScheduling::acquire()?;
    let result = std::thread::scope(|scope| {
        let queued_ref = &queued;
        let cancel_ref = &cancel;
        let interruption_ref = &interruption;
        let worker = std::thread::Builder::new()
            .name("speaker".into())
            .stack_size(8192)
            .spawn_scoped(scope, move || {
                audio.playback_interruptible(
                    receiver,
                    queued_ref,
                    cancel_ref,
                    deadline,
                    Some(interruption_ref),
                )
            })
            .map_err(|_| "Speaker task failed")?;
        let stream = (|| loop {
            let event = socket.receive_interruptible(deadline, Some(&interruption.detected))?;
            check_error(&event)?;
            if let Some(bytes) = realtime::audio_delta(&event)? {
                item_id = Some(
                    event["item_id"]
                        .as_str()
                        .ok_or("Response audio item missing")?
                        .to_owned(),
                );
                content_index = event["content_index"]
                    .as_u64()
                    .ok_or("Response audio content index missing")?
                    as usize;
                if worker.is_finished() {
                    return Err("Speaker stopped before response completion");
                }
                received_bytes += bytes.len();
                let previous = queued.fetch_add(bytes.len(), Ordering::AcqRel);
                max_queued = max_queued.max(previous + bytes.len());
                if previous + bytes.len() > queue_limit {
                    queued.fetch_sub(bytes.len(), Ordering::AcqRel);
                    log::warn!("Reply buffer full: pending_bytes={previous} delta_bytes={} capacity_bytes={queue_limit}", bytes.len());
                    return Err("Playback overflow; response cancelled");
                }
                let len = bytes.len();
                if sender.try_send(Playback::Audio(bytes)).is_err() {
                    queued.fetch_sub(len, Ordering::AcqRel);
                    return Err("Playback queue overflow; response cancelled");
                }
                yield_bytes += len;
                if yield_bytes >= 48000 {
                    std::thread::sleep(Duration::from_millis(10));
                    yield_bytes = 0;
                }
            }
            if event["type"] == "response.done" {
                response_done = true;
                if event["response"]["status"] != "completed" {
                    if event["response"]["status"] == "failed" {
                        return Err(espal_core::recovery::server_error(
                            &event["response"]["status_details"]["error"],
                        ));
                    }
                    return Err("OpenAI response did not complete");
                }
                sender
                    .try_send(Playback::End)
                    .map_err(|_| "Playback completion queue overflow")?;
                return Ok(());
            }
        })();
        if stream.is_err() && !interruption.detected.load(Ordering::Acquire) {
            cancel.store(true, Ordering::Release);
            let _ = socket.send(&json!({"type":"response.cancel"}));
        }
        drop(sender);
        let played = worker.join().map_err(|_| "Speaker task panicked")?;
        if interruption.detected.load(Ordering::Acquire) {
            let item_id = item_id
                .as_deref()
                .ok_or("Interrupted response item unavailable")?;
            if !response_done {
                socket.send(
                    &json!({"type":"response.cancel", "event_id":"espal_interrupt_cancel"}),
                )?;
            }
            let played_ms = interruption
                .played_ms
                .load(Ordering::Acquire)
                .min(received_bytes / 48);
            socket.send(
                &json!({"type":"conversation.item.truncate", "item_id":item_id,
                               "content_index":content_index, "audio_end_ms":played_ms}),
            )?;
            // Drain cancelled audio and acknowledgement before the next turn.
            let cleanup_deadline = deadline.min(Instant::now() + Duration::from_secs(10));
            let mut truncated = false;
            while !response_done || !truncated {
                let event = socket.receive(cleanup_deadline)?;
                // Generation may finish between detecting the wake word and
                // cancelling it. A cancel error is harmless only when it is
                // attributable to our own event and a terminal response.done
                // still arrives; all other server errors remain fatal.
                if event["type"] == "error"
                    && event["error"]["event_id"] == "espal_interrupt_cancel"
                {
                    continue;
                }
                check_error(&event)?;
                if event["type"] == "response.done" {
                    response_done = true;
                }
                if event["type"] == "conversation.item.truncated" && event["item_id"] == item_id {
                    truncated = true;
                }
            }
            log::info!(
                "Reply interrupted: played_ms={played_ms}; unplayed audio removed from history"
            );
            Ok(true)
        } else {
            stream.and(played).map(|_| false)
        }
    });
    log::info!("Response audio: received_bytes={received_bytes} max_queued_bytes={max_queued}");
    if matches!(result, Ok(false)) {
        log::info!("Turn finished; audio stopped");
    }
    result
}
