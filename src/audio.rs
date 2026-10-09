use esp_idf_svc::hal::{
    delay::TickType,
    gpio::AnyIOPin,
    i2s::{config::*, I2sBiDir, I2sDriver},
};
use esp_idf_svc::sys::EspError;
use std::time::{Duration, Instant};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
    },
};

pub enum Playback {
    Audio(Vec<u8>),
    End,
}

const RATE: u32 = 48_000;
const FRAMES: usize = 240;

pub struct Diagnostics {
    driver: I2sDriver<'static, I2sBiDir>,
}

impl Diagnostics {
    fn preload_silence(&mut self) -> Result<(), EspError> {
        // Clear every DMA descriptor, including unsent data after cancellation.
        let silence = vec![0; FRAMES * 8 * 6];
        let loaded = self.driver.preload_data(&silence)?;
        if loaded != silence.len() {
            return Err(EspError::from_infallible::<{ esp_idf_svc::sys::ESP_FAIL }>());
        }
        Ok(())
    }
    pub fn playback(
        &mut self,
        receiver: &Receiver<Playback>,
        queued: &AtomicUsize,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<(), &'static str> {
        let mut pending = VecDeque::new();
        let mut ended = false;
        // Prime with 100ms. Even a very short response plays after End.
        while pending.len() < 4800 && !ended {
            if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err("Playback cancelled");
            }
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(Playback::Audio(bytes)) => pending.extend(bytes),
                Ok(Playback::End) => ended = true,
                Err(RecvTimeoutError::Disconnected) => return Err("Playback queue disconnected"),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        self.preload_silence()
            .map_err(|_| "Playback preload failed")?;
        self.driver
            .rx_enable()
            .map_err(|_| "Playback capture enable failed")?;
        if self.driver.tx_enable().is_err() {
            let _ = self.driver.rx_disable();
            return Err("Playback enable failed");
        }
        let result = (|| {
            let mut filter = espal_core::dsp::Upsampler::new();
            let mut output = vec![0u8; 480 * 2 * 8];
            let mut input = vec![0u8; output.len()];
            let timeout = TickType::new_millis(100).ticks();
            loop {
                if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err("Playback cancelled");
                }
                while pending.len() < 960 && !ended {
                    match receiver.recv_timeout(Duration::from_millis(10)) {
                        Ok(Playback::Audio(bytes)) => pending.extend(bytes),
                        Ok(Playback::End) => ended = true,
                        Err(RecvTimeoutError::Disconnected) => {
                            return Err("Playback queue disconnected")
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            // Maintain the duplex bus with silence during network gaps.
                            self.driver
                                .write_all(&[0; FRAMES * 8], timeout)
                                .map_err(|_| "Silence write timeout")?;
                            self.driver
                                .read(&mut input[..FRAMES * 8], timeout)
                                .map_err(|_| "Gated capture timeout")?;
                        }
                    }
                    if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                        return Err("Playback cancelled");
                    }
                }
                if pending.is_empty() && ended {
                    break;
                }
                output.fill(0);
                let samples = (pending.len() / 2).min(480);
                for index in 0..samples {
                    let sample = i16::from_le_bytes([
                        pending.pop_front().unwrap(),
                        pending.pop_front().unwrap(),
                    ]);
                    for (phase, value) in filter.push(sample).into_iter().enumerate() {
                        let word = (i32::from(value) << 16).to_le_bytes();
                        let frame =
                            &mut output[(index * 2 + phase) * 8..(index * 2 + phase + 1) * 8];
                        frame[..4].copy_from_slice(&word);
                        frame[4..].copy_from_slice(&word);
                    }
                }
                queued.fetch_sub(samples * 2, Ordering::AcqRel);
                self.driver
                    .write_all(&output, timeout)
                    .map_err(|_| "Speaker write timeout")?;
                let mut captured = 0;
                while captured < input.len() {
                    let n = self
                        .driver
                        .read(&mut input[captured..], timeout)
                        .map_err(|_| "Gated capture timeout")?;
                    if n == 0 {
                        return Err("Empty gated capture");
                    }
                    captured += n;
                }
            }
            // Flush filter and pending DMA with silence, then acoustic tail.
            for _ in 0..4 {
                for frame in output.chunks_exact_mut(16) {
                    for (phase, value) in filter.push(0).into_iter().enumerate() {
                        let word = (i32::from(value) << 16).to_le_bytes();
                        frame[phase * 8..phase * 8 + 4].copy_from_slice(&word);
                        frame[phase * 8 + 4..phase * 8 + 8].copy_from_slice(&word);
                    }
                }
                self.driver
                    .write_all(&output, timeout)
                    .map_err(|_| "Playback drain failed")?;
                self.driver
                    .read(&mut input, timeout)
                    .map_err(|_| "Playback drain capture failed")?;
            }
            for _ in 0..25 {
                if Instant::now() >= deadline {
                    return Err("Playback deadline exceeded");
                }
                output.fill(0);
                self.driver
                    .write_all(&output, timeout)
                    .map_err(|_| "Acoustic-tail silence failed")?;
                self.driver
                    .read(&mut input, timeout)
                    .map_err(|_| "Acoustic-tail capture failed")?;
            }
            Ok(())
        })();
        let tx = self.driver.tx_disable();
        let rx = self.driver.rx_disable();
        if tx.is_err() || rx.is_err() {
            return Err("Playback shutdown failed");
        }
        result
    }

    pub fn record(&mut self) -> Result<Vec<u8>, EspError> {
        // 5 seconds of native 48kHz PCM24. Export only after DMA is stopped.
        let mut pcm = Vec::new();
        pcm.try_reserve_exact(240_000 * 3)
            .map_err(|_| EspError::from_infallible::<{ esp_idf_svc::sys::ESP_ERR_NO_MEM }>())?;
        self.preload_silence()?;
        self.driver.rx_enable()?;
        if let Err(e) = self.driver.tx_enable() {
            let _ = self.driver.rx_disable();
            return Err(e);
        }
        let result = (|| {
            let mut input = [0u8; FRAMES * 8];
            let timeout = TickType::new_millis(100).ticks();
            while pcm.len() < 240_000 * 3 {
                let n = self.driver.read(&mut input, timeout)?;
                if n == 0 || n % 8 != 0 {
                    return Err(EspError::from_infallible::<{ esp_idf_svc::sys::ESP_FAIL }>());
                }
                for frame in input[..n].chunks_exact(8) {
                    if pcm.len() == 240_000 * 3 {
                        break;
                    }
                    // Preserve the upper 24 bits; no gain or resampling.
                    pcm.extend_from_slice(&frame[1..4]);
                }
            }
            Ok(())
        })();
        let tx = self.driver.tx_disable();
        let rx = self.driver.rx_disable();
        result.and(tx).and(rx)?;
        Ok(pcm)
    }

    pub fn new(
        i2s: esp_idf_svc::hal::i2s::I2S0<'static>,
        pins: esp_idf_svc::hal::gpio::Pins,
    ) -> Result<Self, EspError> {
        let config = StdConfig::new(
            Config::default()
                .auto_clear(true)
                .frames_per_buffer(FRAMES as u32),
            StdClkConfig::from_sample_rate_hz(RATE),
            StdSlotConfig::philips_slot_default(DataBitWidth::Bits32, SlotMode::Stereo),
            StdGpioConfig::default(),
        );
        let driver = I2sDriver::new_std_bidir(
            i2s,
            &config,
            pins.gpio5,
            pins.gpio7,
            pins.gpio8,
            None::<AnyIOPin>,
            pins.gpio6,
        )?;
        Ok(Self { driver })
    }

    pub fn run(&mut self, tone: bool) -> Result<(), EspError> {
        // Preload silence and clear DMA after transmission to avoid repeating old audio.
        self.preload_silence()?;
        self.driver.rx_enable()?;
        if let Err(e) = self.driver.tx_enable() {
            let _ = self.driver.rx_disable();
            return Err(e);
        }
        let result = self.measure(tone);
        // Always stop both channels, including on timeout/error.
        let tx = self.driver.tx_disable();
        let rx = self.driver.rx_disable();
        result.and(tx).and(rx)
    }

    fn measure(&mut self, tone: bool) -> Result<(), EspError> {
        let mut input = [0u8; FRAMES * 8];
        let mut output = [0u8; FRAMES * 8];
        let timeout = TickType::new_millis(100).ticks();
        let started = Instant::now();
        let mut samples = 0u64;
        let mut squares = 0f64;
        let mut peak = 0i64;
        let mut clipping = 0u64;
        let mut played = 0usize;
        let mut read_timeouts = 0u32;
        while started.elapsed() < Duration::from_secs(1) {
            // 440 Hz at 1% full scale, with a 10 ms envelope; only first 200 ms.
            for frame in output.chunks_exact_mut(8) {
                let t = played as f32 / RATE as f32;
                let envelope = (t / 0.01).min((0.2 - t) / 0.01).clamp(0.0, 1.0);
                let value = if tone {
                    ((t * 440.0 * std::f32::consts::TAU).sin() * envelope * 0.01 * i32::MAX as f32)
                        as i32
                } else {
                    0
                };
                frame[..4].copy_from_slice(&value.to_le_bytes());
                frame[4..].copy_from_slice(&value.to_le_bytes());
                played += 1;
            }
            self.driver.write_all(&output, timeout)?;
            match self.driver.read(&mut input, timeout) {
                Ok(n) => {
                    for frame in input[..n].chunks_exact(8) {
                        // ICS-43434 left slot: signed 24-bit word in the upper bits.
                        // This alignment remains subject to recorded-speech validation.
                        let value = i32::from_le_bytes(frame[..4].try_into().unwrap()) >> 8;
                        peak = peak.max(i64::from(value).abs());
                        squares += f64::from(value).powi(2);
                        clipping += u64::from(i64::from(value).abs() >= 8_300_000);
                        samples += 1;
                    }
                }
                Err(e) if e.code() == esp_idf_svc::sys::ESP_ERR_TIMEOUT => read_timeouts += 1,
                Err(e) => return Err(e),
            }
        }
        log::info!("mic: samples={samples} peak24={peak} rms24={:.1} near_clip={clipping} read_timeouts={read_timeouts}",
            if samples > 0 { (squares / samples as f64).sqrt() } else { 0.0 });
        Ok(())
    }
}
