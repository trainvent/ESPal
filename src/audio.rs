use esp_idf_svc::hal::{
    delay::TickType,
    gpio::AnyIOPin,
    i2s::{config::*, I2sBiDir, I2sDriver},
    peripherals::Peripherals,
};
use esp_idf_svc::sys::EspError;
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
const FRAMES: usize = 240;

pub struct Diagnostics {
    driver: I2sDriver<'static, I2sBiDir>,
}

impl Diagnostics {
    pub fn new() -> Result<Self, EspError> {
        let p = Peripherals::take()?;
        let config = StdConfig::new(
            Config::default()
                .auto_clear(true)
                .frames_per_buffer(FRAMES as u32),
            StdClkConfig::from_sample_rate_hz(RATE),
            StdSlotConfig::philips_slot_default(DataBitWidth::Bits32, SlotMode::Stereo),
            StdGpioConfig::default(),
        );
        let driver = I2sDriver::new_std_bidir(
            p.i2s0,
            &config,
            p.pins.gpio5,
            p.pins.gpio7,
            p.pins.gpio8,
            None::<AnyIOPin>,
            p.pins.gpio6,
        )?;
        Ok(Self { driver })
    }

    pub fn run(&mut self, tone: bool) -> Result<(), EspError> {
        // Preload silence and clear DMA after transmission to avoid repeating old audio.
        self.driver.preload_data(&[0; FRAMES * 8])?;
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
