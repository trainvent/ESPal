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
        // FFT workspace is latency-sensitive. Prefer internal RAM for its
        // small allocations, then restore PSRAM preference for audio queues.
        unsafe { heap_caps_malloc_extmem_enable(16 * 1024) };
        let handle = unsafe { aec_create(16000, 4, 1, aec_mode_t_AEC_MODE_SR_LOW_COST) };
        unsafe { heap_caps_malloc_extmem_enable(1024) };
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
        if !unsafe { heap_caps_check_integrity_all(true) } {
            log::error!("Heap corruption before echo cleanup");
        }
        unsafe {
            aec_destroy(self.handle);
        }
        if !unsafe { heap_caps_check_integrity_all(true) } {
            log::error!("Heap corruption after echo cleanup");
        }
    }
}

pub fn run(
    receiver: Receiver<Frame>,
    signals: &Signals,
    cancel: &AtomicBool,
    ready: std::sync::mpsc::SyncSender<Result<(), &'static str>>,
) -> Result<(), &'static str> {
    let result = run_echo(receiver, signals, cancel, &ready);
    if let Err(error) = result {
        let _ = ready.send(Err(error));
        log::warn!("Interruption detector stopped: {error}; speaker playback remains active");
    }
    result
}

fn run_echo(
    receiver: Receiver<Frame>,
    signals: &Signals,
    cancel: &AtomicBool,
    ready: &std::sync::mpsc::SyncSender<Result<(), &'static str>>,
) -> Result<(), &'static str> {
    use esp_idf_svc::hal::{cpu::Core, task::thread::ThreadSpawnConfiguration};
    struct RestoreSpawn(ThreadSpawnConfiguration);
    impl Drop for RestoreSpawn {
        fn drop(&mut self) {
            let _ = self.0.set();
        }
    }
    let _restore = RestoreSpawn(ThreadSpawnConfiguration::get().unwrap_or_default());
    ThreadSpawnConfiguration {
        pin_to_core: Some(Core::Core0),
        ..ThreadSpawnConfiguration::default()
    }
    .set()
    .map_err(|_| "Wake scheduling configuration failed")?;
    let mut echo = Echo::new()?;
    let mut microphone_filter = espal_core::dsp::WakeDownsampler::default();
    let mut reference_filter = espal_core::dsp::WakeDownsampler::default();
    std::thread::scope(|scope| {
        let (sender, cleaned) = std::sync::mpsc::sync_channel::<(Vec<i16>, Instant)>(64);
        let (wake_ready_sender, wake_ready) = std::sync::mpsc::sync_channel(1);
        let wake = std::thread::Builder::new().name("playback-wake".into()).stack_size(8192)
            .spawn_scoped(scope, move || {
                let mut detector = match crate::wake::Detector::new() {
                    Ok(detector) => { let _ = wake_ready_sender.send(Ok(())); detector },
                    Err(error) => { let _ = wake_ready_sender.send(Err(error)); return Err(error); }
                };
                let mut frames = 0usize;
                let mut max_lag_ms = 0u128;
                while !cancel.load(Ordering::Acquire) {
                    let (samples, captured_at) = match cleaned.recv_timeout(Duration::from_millis(50)) {
                        Ok(frame) => frame,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    };
                    max_lag_ms = max_lag_ms.max(captured_at.elapsed().as_millis());
                    for sample in samples {
                        if detector.push(sample)? {
                            signals.detected.store(true, Ordering::Release);
                            cancel.store(true, Ordering::Release);
                            log::info!("INTERRUPTED: Jarvis detected after echo cancellation; wake_max_lag_ms={max_lag_ms}");
                            return Ok(());
                        }
                    }
                    frames += 1;
                    if frames % 3 == 0 { std::thread::sleep(Duration::from_millis(10)); }
                }
                log::info!("Playback wake processing: frames32ms={frames} max_lag_ms={max_lag_ms}");
                Ok(())
            }).map_err(|_| "Playback wake task failed")?;
        wake_ready
            .recv()
            .map_err(|_| "Playback wake initialization stopped")??;
        let internal = unsafe { heap_caps_get_free_size(MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT) };
        let external = unsafe { heap_caps_get_free_size(MALLOC_CAP_SPIRAM) };
        log::info!(
            "Interruption memory: internal_free_bytes={internal} psram_free_bytes={external}"
        );
        if internal < 32 * 1024 {
            return Err("Insufficient internal memory for safe interruption");
        }
        let _ = ready.send(Ok(()));
        log::info!("Interruption ready: AEC core1/WakeNet core0, say Jarvis during playback");
        let mut frames = 0usize;
        let mut max_us = 0u128;
        let mut max_lag_ms = 0u128;
        let mut mic_energy = 0u64;
        let mut clean_energy = 0u64;
        let mut mic_samples = 0usize;
        let mut clean_samples = 0usize;
        let processed = (|| {
            while !cancel.load(Ordering::Acquire) {
                let frame = match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(frame) => frame,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                };
                let started = Instant::now();
                max_lag_ms = max_lag_ms.max(frame.captured_at.elapsed().as_millis());
                for (mic, speaker) in frame.microphone.into_iter().zip(frame.reference) {
                    let mic = microphone_filter.push_word(mic);
                    let speaker = reference_filter.push_word(speaker);
                    if let (Some(mic), Some(speaker)) = (mic, speaker) {
                      mic_energy += (i64::from(mic) * i64::from(mic)) as u64;
                      mic_samples += 1;
                      if let Some(clean) = echo.push(mic, speaker) {
                        for &sample in clean {
                            clean_energy += (i64::from(sample) * i64::from(sample)) as u64;
                        }
                        clean_samples += clean.len();
                        if sender.try_send((clean.to_vec(), Instant::now())).is_err() {
                            if signals.detected.load(Ordering::Acquire) {
                                return Ok(());
                            }
                            return Err("Wake detection fell behind echo cancellation");
                        }
                      }
                    }
                }
                frames += 1;
                if frames % 50 == 0 {
                    log::info!("Interruption signal: mic_rms={} clean_rms={}",
                        ((mic_energy / mic_samples.max(1) as u64) as f32).sqrt() as u32,
                        ((clean_energy / clean_samples.max(1) as u64) as f32).sqrt() as u32);
                    mic_energy = 0; clean_energy = 0; mic_samples = 0; clean_samples = 0;
                }
                max_us = max_us.max(started.elapsed().as_micros());
                if frames % 5 == 0 {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            Ok(())
        })();
        drop(sender);
        let detected = wake.join().map_err(|_| "Playback wake task panicked")?;
        log::info!("Interruption processing: frames20ms={frames} max_frame_us={max_us} max_lag_ms={max_lag_ms}");
        processed.and(detected)
    })
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
