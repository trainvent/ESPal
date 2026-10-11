//! Owned offline WakeNet instance. Idle microphone audio stays on the Nano.
use esp_idf_svc::sys::*;

// ESP-SR 2.0.5 caches one global model registry. Multiple independent WakeNet
// instances must share it; deinitializing one would invalidate all the others.
// Keep this small flash mapping for the firmware lifetime, serialized at init.
static MODELS: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
fn models() -> Result<*mut srmodel_list_t, &'static str> {
    let mut address = MODELS.lock().map_err(|_| "Wake registry lock failed")?;
    if *address == 0 {
        *address = unsafe { esp_srmodel_init(c"model".as_ptr()) } as usize;
    }
    if *address == 0 {
        return Err("Wake models unavailable");
    }
    Ok(*address as *mut srmodel_list_t)
}

pub struct Detector {
    models: *mut srmodel_list_t,
    iface: *const esp_wn_iface_t,
    data: *mut model_iface_data_t,
    chunk: usize,
    buffer: Vec<i16>,
    name: *const core::ffi::c_char,
    used: bool,
}
impl Detector {
    pub fn new() -> Result<Self, &'static str> {
        let mut detector = Self {
            models: std::ptr::null_mut(),
            iface: std::ptr::null(),
            data: std::ptr::null_mut(),
            chunk: 0,
            buffer: Vec::new(),
            name: std::ptr::null(),
            used: false,
        };
        unsafe {
            detector.models = models()?;
            if detector.models.is_null() {
                return Err("Wake models unavailable");
            }
            let name = esp_srmodel_filter(detector.models, c"wn9".as_ptr(), c"jarvis".as_ptr());
            if name.is_null() {
                return Err("Jarvis wake model missing");
            }
            detector.iface = esp_wn_handle_from_name(name);
            detector.name = name;
            if detector.iface.is_null() {
                return Err("Wake interface unavailable");
            }
            let iface = &*detector.iface;
            detector.data =
                iface.create.ok_or("Wake create unavailable")?(name.cast(), det_mode_t_DET_MODE_90);
            if detector.data.is_null() {
                return Err("Wake model allocation failed");
            }
            if iface.set_det_threshold.ok_or("Wake threshold unavailable")?(detector.data, 0.55, 1) != 1 {
                return Err("Wake threshold setting failed");
            }
            let rate = iface.get_samp_rate.ok_or("Wake sample rate unavailable")?(detector.data);
            let channels = iface.get_channel_num.ok_or("Wake channels unavailable")?(detector.data);
            let chunk = iface
                .get_samp_chunksize
                .ok_or("Wake chunk size unavailable")?(detector.data);
            if rate != 16000 || channels != 1 || !(1..=8192).contains(&chunk) {
                return Err("Unsupported wake model audio format");
            }
            detector.chunk = chunk as usize;
            detector.buffer = Vec::with_capacity(detector.chunk);
            log::info!("Offline wake model ready: Jarvis rate={rate} chunk={chunk}");
        }
        Ok(detector)
    }
    pub fn reset(&mut self) -> Result<(), &'static str> {
        self.buffer.clear();
        // clean() in WakeNet 2.0.5 dereferences uninitialized queues before
        // the first detect frame. Recreate used instances instead.
        if self.used {
            unsafe {
                (*self.iface).destroy.ok_or("Wake destroy unavailable")?(self.data);
                self.data = std::ptr::null_mut();
                self.data = (*self.iface).create.ok_or("Wake create unavailable")?(
                    self.name.cast(),
                    det_mode_t_DET_MODE_90,
                );
            }
            if self.data.is_null() {
                return Err("Wake model restart failed");
            }
            unsafe {
                if (*self.iface).set_det_threshold.ok_or("Wake threshold unavailable")?(self.data, 0.55, 1) != 1 {
                    return Err("Wake threshold setting failed");
                }
            }
            self.used = false;
        }
        Ok(())
    }
    pub fn push(&mut self, sample: i16) -> Result<bool, &'static str> {
        // The ICS43434 signal is quiet at native PCM16 scale. Apply fixed gain
        // only at WakeNet input, after AEC when playing, with saturation.
        self.buffer.push((i32::from(sample) * 16).clamp(i16::MIN as i32, i16::MAX as i32) as i16);
        if self.buffer.len() < self.chunk {
            return Ok(false);
        }
        let state = unsafe {
            (*self.iface).detect.ok_or("Wake detect unavailable")?(
                self.data,
                self.buffer.as_mut_ptr(),
            )
        };
        self.used = true;
        self.buffer.clear();
        Ok(state == wakenet_state_t_WAKENET_DETECTED)
    }
}
impl Drop for Detector {
    fn drop(&mut self) {
        unsafe {
            if !self.data.is_null() && !self.iface.is_null() {
                if let Some(destroy) = (*self.iface).destroy {
                    destroy(self.data);
                }
            }
        }
    }
}
