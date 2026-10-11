use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};
pub use espal_core::config::Config;

pub struct Store {
    nvs: EspDefaultNvs,
}
impl Store {
    pub fn new(partition: EspDefaultNvsPartition) -> Result<Self, &'static str> {
        Ok(Self {
            nvs: EspDefaultNvs::new(partition, "espal", true)
                .map_err(|_| "NVS initialization failed")?,
        })
    }
    pub fn load(&self) -> Result<Option<Config>, &'static str> {
        let len = self.nvs.blob_len("config").map_err(|_| "NVS read failed")?;
        let Some(len) = len else { return Ok(None) };
        if len > 2048 {
            return Err("Stored config exceeds limit");
        }
        let mut bytes = vec![0; len];
        self.nvs
            .get_blob("config", &mut bytes)
            .map_err(|_| "NVS read failed")?
            .ok_or("Missing config")?;
        let config: Config = serde_json::from_slice(&bytes).map_err(|_| "Stored config invalid")?;
        bytes.fill(0);
        config.validate()?;
        Ok(Some(config))
    }
    pub fn provision(&self, json: &str) -> Result<Config, &'static str> {
        if json.len() > 2048 {
            return Err("Config exceeds limit");
        }
        let config: Config = serde_json::from_str(json).map_err(|_| "Invalid provisioning JSON")?;
        config.validate()?;
        self.nvs
            .set_blob("config", json.as_bytes())
            .map_err(|_| "NVS write failed")?;
        Ok(config)
    }
    pub fn forget(&self) -> Result<(), &'static str> {
        self.nvs.remove("config").map_err(|_| "NVS erase failed")?;
        Ok(())
    }
    pub fn set_voice(&self, voice: &str) -> Result<Config, &'static str> {
        let mut config = self.load()?.ok_or("Provision credentials first")?;
        config.voice = voice.into();
        config.validate()?;
        let mut bytes = serde_json::to_vec(&config).map_err(|_| "Config encoding failed")?;
        let result = self
            .nvs
            .set_blob("config", &bytes)
            .map_err(|_| "NVS write failed");
        bytes.fill(0);
        result?;
        Ok(config)
    }
}
