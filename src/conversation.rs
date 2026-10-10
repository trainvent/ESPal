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
    let mut pcm = Vec::with_capacity(9600);
    for index in 0..9600 {
        let t = index as f32 / 48000.0;
        let envelope = (t / 0.01).min((0.2 - t) / 0.01).clamp(0.0, 1.0);
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
        &receiver,
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

pub fn turn(audio: &mut Diagnostics, config: &Config) -> Result<(), &'static str> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut socket = open_session(config, deadline)?;
    log::info!("LISTENING: speak now for five seconds");
    let raw = audio.record().map_err(|_| "Microphone capture failed")?;
    let mut down = Downsampler::new(config.mic_gain);
    let mut chunk = Vec::with_capacity(4800);
    log::info!("Uploading filtered 24kHz PCM16");
    for frame in raw.chunks_exact(3) {
        let word = i32::from_le_bytes([0, frame[0], frame[1], frame[2]]);
        if let Some(sample) = down.push_word(word) {
            chunk.extend_from_slice(&sample.to_le_bytes());
        }
        if chunk.len() == 4800 {
            if Instant::now() >= deadline {
                return Err("Turn deadline exceeded while uploading");
            }
            socket.send(&realtime::append(&chunk))?;
            chunk.clear();
            // At 100Hz a 1ms sleep can round to zero RTOS ticks.
            // Block for a full tick so Wi-Fi/idle tasks can run during upload.
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    log::info!("Input conversion clipped samples={}", down.clipped);
    drop(raw);
    socket.send(&json!({"type":"input_audio_buffer.commit"}))?;
    socket.send(&json!({"type":"response.create"}))?;
    log::info!("Waiting for spoken response; microphone upload gated");
    let (sender, receiver) = mpsc::sync_channel(256);
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
                audio.playback(&receiver, queued_ref, cancel_ref, deadline)
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
                    // Ten seconds of PCM (480KB) remains bounded and fits PSRAM.
                    received_bytes += bytes.len();
                    let previous = queued.fetch_add(bytes.len(), Ordering::AcqRel);
                    max_queued = max_queued.max(previous + bytes.len());
                    if previous + bytes.len() > 480000 {
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
