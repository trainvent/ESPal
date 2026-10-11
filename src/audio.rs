use esp_idf_svc::hal::{
    delay::TickType,
    gpio::AnyIOPin,
    i2s::{config::*, I2sDriver},
};
use esp_idf_svc::sys::EspError;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{Receiver, RecvTimeoutError},
};
use std::time::{Duration, Instant};

pub enum Playback {
    Audio(Vec<u8>),
    End,
}

const RATE: u32 = 48_000;
const FRAMES: usize = 240;
// 80ms of hardware buffering covers the measured Wi-Fi scheduling stalls.
const DMA_BUFFERS: usize = 16;
// User-selected speaker level: -12dB relative to received PCM.
const SPEAKER_DIVISOR: i32 = 4;

pub struct Diagnostics {
    driver: crate::duplex::Duplex,
}

impl Diagnostics {
    fn preload_silence(&mut self) -> Result<(), EspError> {
        // Clear every DMA descriptor, including unsent data after cancellation.
        let silence = vec![0; FRAMES * 8 * DMA_BUFFERS];
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
        // Bench isolation: complete response + FIR conversion before clocks
        // start. During playback only copy precomputed samples and service DMA.
        let mut pcm = Vec::new();
        loop {
            if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err("Playback preparation cancelled");
            }
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(Playback::Audio(bytes)) => {
                    if pcm.len() + bytes.len() > 960000 {
                        return Err("Prepared reply exceeds twenty seconds");
                    }
                    pcm.try_reserve(bytes.len())
                        .map_err(|_| "Reply preparation allocation failed")?;
                    pcm.extend(bytes);
                }
                Ok(Playback::End) => break,
                Err(RecvTimeoutError::Disconnected) => return Err("Playback queue disconnected"),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        if pcm.is_empty() {
            return Err("No response audio received");
        }
        let source_samples = pcm.len() / 2;
        let preparation = Instant::now();
        let mut prepared = Vec::new();
        prepared
            .try_reserve_exact(pcm.len() + 128)
            .map_err(|_| "Filtered reply allocation failed")?;
        let mut filter = espal_core::dsp::Upsampler::new();
        let mut peak = 0i32;
        for (index, bytes) in pcm.chunks_exact(2).enumerate() {
            let sample = i16::from_le_bytes([bytes[0], bytes[1]]);
            peak = peak.max(i32::from(sample).abs());
            prepared.extend(filter.push(sample));
            if index % 480 == 479 {
                if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err("Playback conversion cancelled");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        for _ in 0..63 {
            prepared.extend(filter.push(0));
        }
        queued.fetch_sub(pcm.len(), Ordering::AcqRel);
        drop(pcm);
        log::info!(
            "Reply prepared: pcm24_samples={source_samples} conversion_ms={} peak16={peak}",
            preparation.elapsed().as_millis()
        );
        let mut output = vec![0u8; 960 * 8];
        let mut input = vec![0u8; output.len()];
        self.preload_silence()
            .map_err(|_| "Playback preload failed")?;
        self.driver
            .rx_enable()
            .map_err(|_| "Playback capture enable failed")?;
        if self.driver.tx_enable().is_err() {
            let _ = self.driver.rx_disable();
            return Err("Playback enable failed");
        }
        let started = Instant::now();
        let mut max_pack_us = 0u128;
        let result = (|| {
            let timeout = TickType::new_millis(100).ticks();
            for samples in prepared.chunks(960) {
                if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err("Playback cancelled");
                }
                let packing = Instant::now();
                output.fill(0);
                for (sample, frame) in samples.iter().zip(output.chunks_exact_mut(8)) {
                    let word = ((i32::from(*sample) / SPEAKER_DIVISOR) << 16).to_le_bytes();
                    frame[..4].copy_from_slice(&word);
                    frame[4..].copy_from_slice(&word);
                }
                max_pack_us = max_pack_us.max(packing.elapsed().as_micros());
                self.driver
                    .write_all(&output, timeout)
                    .map_err(|_| "Speaker write failed")?;
                let mut captured = 0;
                while captured < input.len() {
                    let n = self
                        .driver
                        .read(&mut input[captured..], timeout)
                        .map_err(|_| "Gated capture failed")?;
                    if n == 0 {
                        return Err("Empty gated capture");
                    }
                    captured += n;
                }
            }
            output.fill(0);
            for _ in 0..25 {
                if Instant::now() >= deadline {
                    return Err("Playback tail deadline exceeded");
                }
                self.driver
                    .write_all(&output, timeout)
                    .map_err(|_| "Playback silence failed")?;
                self.driver
                    .read(&mut input, timeout)
                    .map_err(|_| "Playback tail capture failed")?;
            }
            Ok(())
        })();
        let tx = self.driver.tx_disable();
        let rx = self.driver.rx_disable();
        log::info!("Playback: pcm24_samples={source_samples} elapsed_ms={} max_20ms_pack_us={max_pack_us} speaker_divisor={SPEAKER_DIVISOR}", started.elapsed().as_millis());
        if tx.is_err() || rx.is_err() {
            return Err("Playback shutdown failed");
        }
        result
    }

    pub fn loopback(&mut self) -> Result<(), &'static str> {
        // Observe D5 internally; microphone D4 is never driven or rewired.
        // The amplifier sees only a very small balanced test pattern.
        use esp_idf_svc::sys::{
            esp_rom_gpio_connect_in_signal, gpio_input_enable, gpio_num_t_GPIO_NUM_8,
            I2S0I_SD_IN_IDX,
        };
        self.preload_silence()
            .map_err(|_| "Loopback preload failed")?;
        unsafe {
            if gpio_input_enable(gpio_num_t_GPIO_NUM_8) != 0 {
                return Err("Loopback input enable failed");
            }
            esp_rom_gpio_connect_in_signal(8, I2S0I_SD_IN_IDX, false);
        }
        let result = (|| {
            self.driver
                .rx_enable()
                .map_err(|_| "Loopback RX enable failed")?;
            self.driver
                .tx_enable()
                .map_err(|_| "Loopback TX enable failed")?;
            let values = [-16i32, -8, 8, 16];
            let mut output = [0u8; FRAMES * 8];
            for (index, frame) in output.chunks_exact_mut(8).enumerate() {
                let v = values[index % 4] << 16;
                frame[..4].copy_from_slice(&v.to_le_bytes());
                frame[4..].copy_from_slice(&(-v).to_le_bytes());
            }
            let mut input = [0u8; FRAMES * 8];
            let timeout = TickType::new_millis(100).ticks();
            let (mut matched, mut invalid, mut zero_after_data) = (0usize, 0usize, 0usize);
            let mut phase = None;
            for _ in 0..100 {
                self.driver
                    .write_all(&output, timeout)
                    .map_err(|_| "Loopback write failed")?;
                let n = self
                    .driver
                    .read(&mut input, timeout)
                    .map_err(|_| "Loopback read failed")?;
                if n % 8 != 0 {
                    return Err("Loopback unaligned read");
                }
                for frame in input[..n].chunks_exact(8) {
                    let left = i32::from_le_bytes(frame[..4].try_into().unwrap());
                    let right = i32::from_le_bytes(frame[4..].try_into().unwrap());
                    if left == 0 && right == 0 {
                        zero_after_data += usize::from(phase.is_some());
                        continue;
                    }
                    let index = *phase.get_or_insert_with(|| {
                        values.iter().position(|v| (*v << 16) == left).unwrap_or(0)
                    });
                    let expected = values[index] << 16;
                    if left == expected && right == -expected {
                        matched += 1;
                    } else {
                        invalid += 1;
                    }
                    phase = Some((index + 1) % 4);
                }
            }
            log::info!("I2S loopback: matched_frames={matched} invalid_frames={invalid} zero_after_data={zero_after_data}");
            if matched < 10000 || invalid != 0 || zero_after_data != 0 {
                return Err("I2S loopback pattern mismatch or underrun");
            }
            Ok(())
        })();
        let _ = self.driver.tx_disable();
        let _ = self.driver.rx_disable();
        unsafe {
            esp_rom_gpio_connect_in_signal(7, I2S0I_SD_IN_IDX, false);
        }
        result?;
        log::info!(
            "I2S loopback complete; signed stereo words verified, microphone routing restored"
        );
        Ok(())
    }

    pub fn tone_tx(
        i2s: esp_idf_svc::hal::i2s::I2S0<'static>,
        pins: esp_idf_svc::hal::gpio::Pins,
        pico: bool,
    ) -> Result<(), EspError> {
        let bits = if pico {
            DataBitWidth::Bits32
        } else {
            DataBitWidth::Bits16
        };
        let frame_bytes = if pico { 8 } else { 4 };
        let config = StdConfig::new(
            Config::default()
                .dma_buffer_count(DMA_BUFFERS as u32)
                .auto_clear(true)
                .frames_per_buffer(FRAMES as u32),
            StdClkConfig::from_sample_rate_hz(RATE),
            StdSlotConfig::philips_slot_default(bits, SlotMode::Stereo),
            StdGpioConfig::default(),
        );
        let mut driver = I2sDriver::new_std_tx(
            i2s,
            &config,
            pins.gpio5,
            pins.gpio8,
            None::<AnyIOPin>,
            pins.gpio6,
        )?;
        let silence = vec![0; FRAMES * frame_bytes * DMA_BUFFERS];
        // Prepare once, before DMA starts. FRAMES is divisible by the 48-sample
        // period, so replaying this buffer preserves waveform continuity.
        let mut tone = vec![0; FRAMES * frame_bytes];
        for (index, frame) in tone.chunks_exact_mut(frame_bytes).enumerate() {
            let phase = (index % 48) as i32;
            let ramp = if phase < 24 { phase } else { 48 - phase };
            let sample = ((ramp - 12) * 512 / 12) as i16;
            if pico {
                let slot = (i32::from(sample) << 16).to_le_bytes();
                frame[..4].copy_from_slice(&slot);
                frame[4..].copy_from_slice(&slot);
            } else {
                frame[..2].copy_from_slice(&sample.to_le_bytes());
                frame[2..].copy_from_slice(&sample.to_le_bytes());
            }
        }
        driver.preload_data(&silence)?;
        driver.tx_enable()?;
        let started = Instant::now();
        let result = (|| {
            let timeout = TickType::new_millis(100).ticks();
            for block in 0..340 {
                let output = if (40..240).contains(&block) {
                    &tone[..]
                } else {
                    &silence[..FRAMES * frame_bytes]
                };
                driver.write_all(output, timeout)?;
            }
            Ok(())
        })();
        let stop = driver.tx_disable();
        result.and(stop)?;
        if pico {
            log::info!("Pico-equivalent 32-bit TX tone complete; elapsed_ms={}; reset before another audio command", started.elapsed().as_millis());
        } else {
            log::info!(
                "16-bit TX-only tone complete; elapsed_ms={}; reset before another audio command",
                started.elapsed().as_millis()
            );
        }
        Ok(())
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
        let driver = crate::duplex::Duplex::new(i2s, pins, FRAMES, DMA_BUFFERS)?;
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
