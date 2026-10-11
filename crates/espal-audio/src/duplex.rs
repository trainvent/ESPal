//! ESP-IDF duplex ownership with TX initialized first (clock master).
use esp_idf_svc::{
    hal::{delay::TickType, gpio::Pins, i2s::I2S0},
    sys::*,
};

pub struct Duplex {
    tx: i2s_chan_handle_t,
    rx: i2s_chan_handle_t,
    _i2s: I2S0<'static>,
    _pins: Pins,
}
// Unique channel ownership; methods require &mut self and callbacks are unused.
// Moving the owner to the speaker thread does not share channel access.
unsafe impl Send for Duplex {}

impl Duplex {
    pub fn new(
        i2s: I2S0<'static>,
        pins: Pins,
        frames: usize,
        buffers: usize,
    ) -> Result<Self, EspError> {
        let mut driver = Self {
            tx: core::ptr::null_mut(),
            rx: core::ptr::null_mut(),
            _i2s: i2s,
            _pins: pins,
        };
        let channel = i2s_chan_config_t {
            id: 0,
            role: i2s_role_t_I2S_ROLE_MASTER,
            dma_desc_num: buffers as u32,
            dma_frame_num: frames as u32,
            __bindgen_anon_1: i2s_chan_config_t__bindgen_ty_1 {
                auto_clear_after_cb: true,
            },
            ..Default::default()
        };
        let config = i2s_std_config_t {
            clk_cfg: i2s_std_clk_config_t {
                sample_rate_hz: 48000,
                clk_src: soc_periph_i2s_clk_src_t_I2S_CLK_SRC_DEFAULT,
                mclk_multiple: 256,
                ..Default::default()
            },
            slot_cfg: i2s_std_slot_config_t {
                data_bit_width: 32,
                slot_bit_width: 32,
                slot_mode: i2s_slot_mode_t_I2S_SLOT_MODE_STEREO,
                slot_mask: i2s_std_slot_mask_t_I2S_STD_SLOT_BOTH,
                ws_width: 32,
                bit_shift: true,
                ..Default::default()
            },
            gpio_cfg: i2s_std_gpio_config_t {
                mclk: -1,
                bclk: 5,
                ws: 6,
                dout: 8,
                din: 7,
                ..Default::default()
            },
        };
        unsafe {
            esp!(i2s_new_channel(&channel, &mut driver.tx, &mut driver.rx))?;
            // ESP-IDF makes the second initialized channel the duplex slave.
            esp!(i2s_channel_init_std_mode(driver.tx, &config))?;
            esp!(i2s_channel_init_std_mode(driver.rx, &config))?;
            let mut tx_info = i2s_chan_info_t::default();
            let mut rx_info = i2s_chan_info_t::default();
            esp!(i2s_channel_get_info(driver.tx, &mut tx_info))?;
            esp!(i2s_channel_get_info(driver.rx, &mut rx_info))?;
            log::info!(
                "I2S roles: TX={} RX={} BCLK={}Hz",
                tx_info.role,
                rx_info.role,
                tx_info.bclk_hz
            );
            if tx_info.role != i2s_role_t_I2S_ROLE_MASTER
                || rx_info.role != i2s_role_t_I2S_ROLE_SLAVE
            {
                return Err(EspError::from_infallible::<{ ESP_ERR_INVALID_STATE }>());
            }
        }
        Ok(driver)
    }
    pub fn preload_data(&mut self, data: &[u8]) -> Result<usize, EspError> {
        let mut n = 0;
        unsafe {
            esp!(i2s_channel_preload_data(
                self.tx,
                data.as_ptr().cast(),
                data.len(),
                &mut n
            ))?;
        }
        Ok(n)
    }
    pub fn tx_enable(&mut self) -> Result<(), EspError> {
        unsafe { esp!(i2s_channel_enable(self.tx)) }
    }
    pub fn rx_enable(&mut self) -> Result<(), EspError> {
        unsafe { esp!(i2s_channel_enable(self.rx)) }
    }
    pub fn tx_disable(&mut self) -> Result<(), EspError> {
        unsafe { esp!(i2s_channel_disable(self.tx)) }
    }
    pub fn rx_disable(&mut self) -> Result<(), EspError> {
        unsafe { esp!(i2s_channel_disable(self.rx)) }
    }
    pub fn read(&mut self, data: &mut [u8], ticks: TickType_t) -> Result<usize, EspError> {
        let mut n = 0;
        let result = unsafe {
            i2s_channel_read(
                self.rx,
                data.as_mut_ptr().cast(),
                data.len(),
                &mut n,
                TickType(ticks).as_millis_u32(),
            )
        };
        if result == ESP_ERR_TIMEOUT && n > 0 {
            return Ok(n);
        }
        esp!(result)?;
        Ok(n)
    }
    pub fn write_all(&mut self, mut data: &[u8], ticks: TickType_t) -> Result<(), EspError> {
        while !data.is_empty() {
            let mut n = 0;
            let result = unsafe {
                i2s_channel_write(
                    self.tx,
                    data.as_ptr().cast(),
                    data.len(),
                    &mut n,
                    TickType(ticks).as_millis_u32(),
                )
            };
            if result != ESP_ERR_TIMEOUT || n == 0 {
                esp!(result)?;
            }
            if n == 0 {
                return Err(EspError::from_infallible::<{ ESP_ERR_TIMEOUT }>());
            }
            data = &data[n..];
        }
        Ok(())
    }
}
impl Drop for Duplex {
    fn drop(&mut self) {
        unsafe {
            if !self.rx.is_null() {
                let _ = i2s_channel_disable(self.rx);
            }
            if !self.tx.is_null() {
                let _ = i2s_channel_disable(self.tx);
            }
            if !self.rx.is_null() {
                let _ = i2s_del_channel(self.rx);
            }
            if !self.tx.is_null() {
                let _ = i2s_del_channel(self.tx);
            }
        }
    }
}
