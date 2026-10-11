//! Local echo-cancelled wake detection, isolated from the DMA servicing task.
use esp_idf_svc::sys::*;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::Receiver,
};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Signals {
    pub detected: AtomicBool,
    pub played_ms: AtomicUsize,
}

pub struct Frame {
    pub microphone: Vec<i32>,
    pub reference: Vec<i32>,
    pub captured_at: Instant,
}

struct Aligned {
    ptr: *mut i16,
    len: usize,
}
impl Aligned {
    fn new(len: usize) -> Result<Self, &'static str> {
        let ptr =
            unsafe { heap_caps_aligned_alloc(16, len * 2, MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT) }
                .cast::<i16>();
        if ptr.is_null() {
            return Err("AEC aligned allocation failed");
        }
        unsafe {
            std::ptr::write_bytes(ptr, 0, len);
        }
        Ok(Self { ptr, len })
    }
    fn samples(&self) -> &[i16] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}
impl Drop for Aligned {
    fn drop(&mut self) {
        unsafe {
            heap_caps_free(self.ptr.cast());
        }
    }
}

struct Echo {
    handle: *mut aec_handle_t,
    microphone: Aligned,
    reference: Aligned,
    output: Aligned,
    used: usize,
}
impl Echo {
    fn new() -> Result<Self, &'static str> {
        // Allocate buffers first, so an allocation failure cannot leak the handle.
        let microphone = Aligned::new(512)?;
        let reference = Aligned::new(512)?;
        let output = Aligned::new(512)?;
        let handle = unsafe { aec_create(16000, 4, 1, aec_mode_t_AEC_MODE_SR_LOW_COST) };
        if handle.is_null() {
            return Err("AEC creation failed");
        }
        let echo = Self {
            handle,
            microphone,
            reference,
            output,
            used: 0,
        };
        if unsafe { aec_get_chunksize(handle) } != 512 {
            return Err("Unsupported AEC frame size");
        }
        Ok(echo)
    }
    fn push(&mut self, microphone: i16, reference: i16) -> Option<&[i16]> {
        unsafe {
            *self.microphone.ptr.add(self.used) = microphone;
            *self.reference.ptr.add(self.used) = reference;
        }
        self.used += 1;
        if self.used < 512 {
            return None;
        }
        unsafe {
            aec_process(
                self.handle,
                self.microphone.ptr,
                self.reference.ptr,
                self.output.ptr,
            );
        }
        self.used = 0;
        Some(self.output.samples())
    }
}
impl Drop for Echo {
    fn drop(&mut self) {
        unsafe {
            aec_destroy(self.handle);
        }
    }
}

pub fn run(
    receiver: Receiver<Frame>,
    signals: &Signals,
    cancel: &AtomicBool,
    ready: std::sync::mpsc::SyncSender<Result<(), &'static str>>,
) -> Result<(), &'static str> {
    let initialized = (|| Ok((Echo::new()?, crate::wake::Detector::new()?)))();
    let (mut echo, mut detector) = match initialized {
        Ok(state) => {
            let _ = ready.send(Ok(()));
            state
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return Err(e);
        }
    };
    let mut microphone = espal_core::dsp::WakeDownsampler::default();
    let mut reference = espal_core::dsp::WakeDownsampler::default();
    let mut frames = 0usize;
    let mut max_us = 0u128;
    let mut max_lag_ms = 0u128;
    log::info!("Interruption ready: AEC 16kHz/512, say Jarvis during playback");
    while !cancel.load(Ordering::Acquire) {
        let frame = match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(frame) => frame,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        };
        let started = Instant::now();
        max_lag_ms = max_lag_ms.max(frame.captured_at.elapsed().as_millis());
        for (mic, speaker) in frame.microphone.into_iter().zip(frame.reference) {
            if let (Some(mic), Some(speaker)) =
                (microphone.push_word(mic), reference.push_word(speaker))
            {
                if let Some(clean) = echo.push(mic, speaker) {
                    for &sample in clean {
                        if detector.push(sample)? {
                            signals.detected.store(true, Ordering::Release);
                            cancel.store(true, Ordering::Release);
                            log::info!("INTERRUPTED: Jarvis detected after echo cancellation; max_lag_ms={max_lag_ms}");
                            return Ok(());
                        }
                    }
                }
            }
        }
        frames += 1;
        max_us = max_us.max(started.elapsed().as_micros());
        // Cooperate with Wi-Fi and the reply converter.
        if frames % 5 == 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    log::info!("Interruption processing: frames20ms={frames} max_frame_us={max_us} max_lag_ms={max_lag_ms}");
    Ok(())
}

pub fn check() -> Result<(), &'static str> {
    let mut echo = Echo::new()?;
    let mut seed = 7u32;
    // DMA preload is aligned by the playback task. Exercise the remaining
    // acoustic propagation delay with a deterministic, nontrivial signal.
    let mut reference = std::collections::VecDeque::from(vec![0i16; 160]); // 10ms echo.
    let mut input_energy = 0u64;
    let mut output_energy = 0u64;
    let started = Instant::now();
    for index in 0..16000 * 6 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let speaker = ((seed >> 16) as i16) / 4;
        reference.push_back(speaker);
        let mic = reference.pop_front().unwrap() / 2;
        if index >= 16000 * 4 {
            input_energy += i64::from(mic).pow(2) as u64;
        }
        if let Some(clean) = echo.push(mic, speaker) {
            if index >= 16000 * 4 {
                output_energy += clean
                    .iter()
                    .map(|&s| i64::from(s).pow(2) as u64)
                    .sum::<u64>();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let reduction = 10.0 * ((input_energy as f64 + 1.0) / (output_energy as f64 + 1.0)).log10();
    log::info!(
        "AEC diagnostic: synthetic_echo_reduction_db={reduction:.1} elapsed_ms={}",
        started.elapsed().as_millis()
    );
    if reduction < 10.0 {
        return Err("AEC synthetic echo suppression failed");
    }
    log::info!("AEC diagnostic complete");
    Ok(())
}
