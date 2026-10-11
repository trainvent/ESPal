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
const PLAYBACK_PRIME_BLOCKS: usize = 30; // 600ms of prepared audio.
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
        receiver: Receiver<Playback>,
        queued: &AtomicUsize,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<(), &'static str> {
        // Conversion runs independently of DMA servicing. Bound the prepared
        // queue to two seconds, and prime 600ms before starting the clocks.
        // Keep the FIR worker away from the Wi-Fi task pinned to core 0.
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
        .map_err(|_| "Reply filter scheduling configuration failed")?;
        std::thread::scope(|scope| {
            let (sender, prepared) = std::sync::mpsc::sync_channel(100);
            let converter = std::thread::Builder::new()
                .name("reply-filter".into())
                .stack_size(8192)
                .spawn_scoped(scope, move || {
                    log::info!("Reply filter: core={:?}", esp_idf_svc::hal::cpu::core());
                    let mut filter = espal_core::dsp::Upsampler::new();
                    let mut output = Vec::with_capacity(960 * 8);
                    let mut samples = 0usize;
                    let mut peak = 0i32;
                    let mut max_conversion_us = 0u128;
                    let mut ended = false;
                    while !ended {
                        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                            return Err("Reply conversion cancelled");
                        }
                        match receiver.recv_timeout(Duration::from_millis(50)) {
                            Ok(Playback::Audio(bytes)) => {
                                let mut converting = Instant::now();
                                for bytes in bytes.chunks_exact(2) {
                                    let sample = i16::from_le_bytes([bytes[0], bytes[1]]);
                                    samples += 1;
                                    peak = peak.max(i32::from(sample).abs());
                                    for value in filter.push(sample) {
                                        let word = ((i32::from(value) / SPEAKER_DIVISOR) << 16)
                                            .to_le_bytes();
                                        output.extend_from_slice(&word);
                                        output.extend_from_slice(&word);
                                    }
                                    if output.len() == 960 * 8 {
                                        max_conversion_us =
                                            max_conversion_us.max(converting.elapsed().as_micros());
                                        sender
                                            .send(Playback::Audio(std::mem::replace(
                                                &mut output,
                                                Vec::with_capacity(960 * 8),
                                            )))
                                            .map_err(|_| "Prepared audio queue disconnected")?;
                                        // Once per 100ms of audio, yield a full RTOS tick.
                                        if samples % 2400 == 0 {
                                            std::thread::sleep(Duration::from_millis(10));
                                        }
                                        if cancel.load(Ordering::Acquire)
                                            || Instant::now() >= deadline
                                        {
                                            return Err("Reply conversion cancelled");
                                        }
                                        converting = Instant::now();
                                    }
                                }
                                queued.fetch_sub(bytes.len(), Ordering::AcqRel);
                            }
                            Ok(Playback::End) => ended = true,
                            Err(RecvTimeoutError::Disconnected) => {
                                return Err("Playback queue disconnected")
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                        }
                    }
                    if samples == 0 {
                        return Err("No response audio received");
                    }
                    // Flush the reconstruction filter before padding the last frame.
                    for _ in 0..63 {
                        for value in filter.push(0) {
                            let word = ((i32::from(value) / SPEAKER_DIVISOR) << 16).to_le_bytes();
                            output.extend_from_slice(&word);
                            output.extend_from_slice(&word);
                            if output.len() == 960 * 8 {
                                sender
                                    .send(Playback::Audio(std::mem::replace(
                                        &mut output,
                                        Vec::with_capacity(960 * 8),
                                    )))
                                    .map_err(|_| "Prepared audio queue disconnected")?;
                            }
                        }
                    }
                    if !output.is_empty() {
                        output.resize(960 * 8, 0);
                        sender
                            .send(Playback::Audio(output))
                            .map_err(|_| "Prepared audio queue disconnected")?;
                    }
                    sender
                        .send(Playback::End)
                        .map_err(|_| "Prepared audio queue disconnected")?;
                    Ok((samples, peak, max_conversion_us))
                })
                .map_err(|_| "Reply filter task failed")?;
            let played = self.play_prepared(&prepared, cancel, deadline);
            if played.is_err() {
                cancel.store(true, Ordering::Release);
            }
            // Unblock a converter waiting on a full queue after DMA failure.
            drop(prepared);
            let converted = converter.join().map_err(|_| "Reply filter task panicked")?;
            match (played, converted) {
                (Err(e), _) => Err(e),
                (_, Err(e)) => Err(e),
                (Ok(()), Ok((samples, peak, max_us))) => {
                    log::info!("Response conversion: pcm24_samples={samples} peak16={peak} max_chunk_conversion_us={max_us}");
                    Ok(())
                }
            }
        })
    }

    fn play_prepared(
        &mut self,
        prepared: &Receiver<Playback>,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<(), &'static str> {
        let waiting = Instant::now();
        let mut primed = std::collections::VecDeque::new();
        let mut ended = false;
        while primed.len() < PLAYBACK_PRIME_BLOCKS && !ended {
            if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err("Playback priming cancelled");
            }
            match prepared.recv_timeout(Duration::from_millis(50)) {
                Ok(Playback::Audio(bytes)) => primed.push_back(bytes),
                Ok(Playback::End) => ended = true,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("Reply converter stopped during priming")
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        log::info!(
            "Streaming playback: primed_ms={} wait_ms={}",
            primed.len() * 20,
            waiting.elapsed().as_millis()
        );
        let silence = vec![0u8; 960 * 8];
        let mut input = vec![0u8; silence.len()];
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
        let mut gaps = 0usize;
        let mut frames = 0usize;
        let result = (|| {
            let timeout = TickType::new_millis(100).ticks();
            loop {
                if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err("Playback cancelled");
                }
                let next = if let Some(bytes) = primed.pop_front() {
                    Some(bytes)
                } else if ended {
                    break;
                } else {
                    match prepared.recv_timeout(Duration::from_millis(10)) {
                        Ok(Playback::Audio(bytes)) => Some(bytes),
                        Ok(Playback::End) => break,
                        Err(RecvTimeoutError::Disconnected) => {
                            return Err("Reply converter stopped")
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            gaps += 1;
                            None
                        }
                    }
                };
                let output = next.as_deref().unwrap_or(&silence);
                self.driver
                    .write_all(output, timeout)
                    .map_err(|_| "Speaker write failed")?;
                frames += output.len() / 8;
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
            for _ in 0..25 {
                if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err("Playback tail cancelled");
                }
                self.driver
                    .write_all(&silence, timeout)
                    .map_err(|_| "Playback silence failed")?;
                self.driver
                    .read(&mut input, timeout)
                    .map_err(|_| "Playback tail capture failed")?;
            }
            Ok(())
        })();
        let tx = self.driver.tx_disable();
        let rx = self.driver.rx_disable();
        log::info!("Playback: frames48={frames} elapsed_ms={} preparation_gaps={gaps} speaker_divisor={SPEAKER_DIVISOR}", started.elapsed().as_millis());
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

    pub fn stream_capture(
        &mut self,
        sender: std::sync::mpsc::SyncSender<Vec<i32>>,
        queued: &AtomicUsize,
        cancel: &AtomicBool,
        deadline: Instant,
        speech: bool,
    ) -> Result<(usize, u128, usize), &'static str> {
        self.preload_silence()
            .map_err(|_| "Capture preload failed")?;
        self.driver
            .rx_enable()
            .map_err(|_| "Capture RX enable failed")?;
        if self.driver.tx_enable().is_err() {
            let _ = self.driver.rx_disable();
            return Err("Capture TX enable failed");
        }
        if speech {
            log::info!("LISTENING: speak naturally; a 1.2-second pause ends your request");
        } else {
            log::info!("LISTENING: speak now for five seconds; upload streams during capture");
        }
        let started = Instant::now();
        let mut samples = 0usize;
        let mut max_queued = 0usize;
        let mut emitted = 0usize;
        let result = (|| {
            let mut input = [0u8; FRAMES * 8];
            let mut block = Vec::with_capacity(2400);
            let timeout = TickType::new_millis(100).ticks();
            let mut gate = espal_core::speech::SpeechGate::default();
            let mut preroll = std::collections::VecDeque::new();
            let mut speaking = !speech;
            'capture: loop {
                if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err("Microphone streaming cancelled");
                }
                // TX generates shared clocks; its preloaded silence is cleared
                // automatically by DMA. No microphone audio is sent to TX.
                let n = self
                    .driver
                    .read(&mut input, timeout)
                    .map_err(|_| "Microphone read failed")?;
                if n == 0 || n % 8 != 0 {
                    return Err("Invalid microphone DMA read");
                }
                for frame in input[..n].chunks_exact(8) {
                    block.push(i32::from_le_bytes(frame[..4].try_into().unwrap()));
                    samples += 1;
                    if block.len() == 2400 {
                        use espal_core::speech::State;
                        let state = if speech {
                            gate.push(&block)
                        } else {
                            State::Speaking
                        };
                        let block = std::mem::replace(&mut block, Vec::with_capacity(2400));
                        let ready = if speaking {
                            vec![block]
                        } else {
                            preroll.push_back(block);
                            if preroll.len() > 6 {
                                preroll.pop_front();
                            }
                            match state {
                                State::Started => {
                                    speaking = true;
                                    log::info!("Speech started; streaming buffered onset");
                                    preroll.drain(..).collect()
                                }
                                State::NoSpeech => return Err("No speech detected"),
                                State::Limit => return Err("Speech input limit exceeded"),
                                _ => Vec::new(),
                            }
                        };
                        for block in ready {
                            let len = block.len();
                            let pending = queued.fetch_add(len, Ordering::AcqRel) + len;
                            max_queued = max_queued.max(pending);
                            if sender.try_send(block).is_err() {
                                queued.fetch_sub(len, Ordering::AcqRel);
                                return Err(
                                    "Microphone buffer overflow or filter stopped; input cancelled",
                                );
                            }
                            emitted += len;
                        }
                        if state == State::Limit {
                            return Err("Speech input limit exceeded");
                        }
                        if state == State::Ended {
                            log::info!("Speech ended after sustained silence");
                            break 'capture;
                        }
                    }
                    if !speech && samples == 240_000 {
                        break 'capture;
                    }
                }
            }
            Ok(())
        })();
        let elapsed = started.elapsed().as_millis();
        let tx = self.driver.tx_disable();
        let rx = self.driver.rx_disable();
        log::info!(
            "Input capture: samples={samples} elapsed_ms={elapsed} max_raw_buffer_ms={}",
            max_queued / 48
        );
        if result.is_err() {
            cancel.store(true, Ordering::Release);
        }
        result?;
        if tx.is_err() || rx.is_err() {
            cancel.store(true, Ordering::Release);
            return Err("Microphone capture shutdown failed");
        }
        log::info!("Capture finished; remaining microphone upload is draining");
        Ok((emitted, elapsed, max_queued))
    }

    pub fn wait_for_wake(
        &mut self,
        detector: &mut crate::wake::Detector,
        deadline: Instant,
    ) -> Result<bool, &'static str> {
        detector.reset()?;
        self.preload_silence().map_err(|_| "Wake preload failed")?;
        self.driver
            .rx_enable()
            .map_err(|_| "Wake RX enable failed")?;
        if self.driver.tx_enable().is_err() {
            let _ = self.driver.rx_disable();
            return Err("Wake TX enable failed");
        }
        log::info!("WAKE READY: say Jarvis; idle audio stays local");
        let result = (|| {
            let mut filter = espal_core::dsp::WakeDownsampler::default();
            let mut input = [0u8; FRAMES * 8];
            let timeout = TickType::new_millis(100).ticks();
            let mut samples = 0usize;
            while Instant::now() < deadline {
                let n = self
                    .driver
                    .read(&mut input, timeout)
                    .map_err(|_| "Wake microphone read failed")?;
                if n == 0 || n % 8 != 0 {
                    return Err("Invalid wake DMA read");
                }
                for frame in input[..n].chunks_exact(8) {
                    let word = i32::from_le_bytes(frame[..4].try_into().unwrap());
                    if let Some(sample) = filter.push_word(word) {
                        if detector.push(sample)? {
                            log::info!("WAKE DETECTED: Jarvis");
                            return Ok(true);
                        }
                    }
                    samples += 1;
                }
                if samples % 4800 == 0 {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            Ok(false)
        })();
        let tx = self.driver.tx_disable();
        let rx = self.driver.rx_disable();
        if tx.is_err() || rx.is_err() {
            return Err("Wake capture shutdown failed");
        }
        result
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
