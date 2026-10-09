use serde::{Deserialize, Serialize};

// Deliberately no Debug: credentials must never enter logs.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub ssid: String,
    pub password: String,
    pub api_key: String,
    pub model: String,
    pub voice: String,
    pub instructions: String,
    pub mic_gain: u8,
}
impl Config {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.ssid.is_empty() || self.ssid.len() > 32 || self.ssid.contains('\0') {
            return Err("Invalid SSID length");
        }
        if !(self.password.is_empty() || (8..=63).contains(&self.password.len()))
            || self.password.contains('\0')
        {
            return Err("Wi-Fi password must be empty or 8–63 bytes");
        }
        if !(16..=512).contains(&self.api_key.len())
            || !self.api_key.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err("Invalid API key");
        }
        if self.model.is_empty()
            || self.model.len() > 80
            || !self
                .model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
        {
            return Err("Invalid model identifier");
        }
        if ![
            "alloy", "ash", "ballad", "coral", "echo", "sage", "shimmer", "verse", "marin", "cedar",
        ]
        .contains(&self.voice.as_str())
        {
            return Err("Invalid voice");
        }
        if self.instructions.len() > 512 || self.instructions.contains('\0') {
            return Err("Instructions exceed limit");
        }
        if !(1..=32).contains(&self.mic_gain) {
            return Err("Mic gain must be 1–32");
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prevents_header_and_url_injection() {
        let mut c = Config {
            ssid: "test".into(),
            password: "testpass".into(),
            api_key: "test-key-not-a-real-secret".into(),
            model: "gpt-realtime-2.1".into(),
            voice: "marin".into(),
            instructions: "Be brief".into(),
            mic_gain: 8,
        };
        assert!(c.validate().is_ok());
        c.api_key.push_str("\r\nInjected: value");
        assert!(c.validate().is_err());
        c.api_key = "test-key-not-a-real-secret".into();
        c.model.push_str("&other=value");
        assert!(c.validate().is_err());
    }
}
