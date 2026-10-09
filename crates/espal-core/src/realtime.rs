use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

pub const MAX_MESSAGE: usize = 64 * 1024;

pub fn session_update(voice: &str, instructions: &str) -> Value {
    json!({"type":"session.update","session":{
        "type":"realtime","output_modalities":["audio"],"instructions":instructions,
        "audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"turn_detection":null},
                 "output":{"format":{"type":"audio/pcm","rate":24000},"voice":voice}}
    }})
}
pub fn append(pcm: &[u8]) -> Value {
    json!({"type":"input_audio_buffer.append","audio":STANDARD.encode(pcm)})
}
pub fn audio_delta(event: &Value) -> Result<Option<Vec<u8>>, &'static str> {
    if event["type"] != "response.output_audio.delta" {
        return Ok(None);
    }
    let encoded = event["delta"].as_str().ok_or("Missing audio delta")?;
    if encoded.len() > MAX_MESSAGE {
        return Err("Oversized audio delta");
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid audio base64")?;
    if bytes.len() % 2 != 0 {
        return Err("Unaligned PCM16 delta");
    }
    Ok(Some(bytes))
}

/// Assemble both split receive callbacks and RFC6455 continuation frames.
#[derive(Default)]
pub struct Assembler {
    message: Vec<u8>,
    frame_offset: usize,
    frame_len: usize,
    active: bool,
}
impl Assembler {
    pub fn push(
        &mut self,
        opcode: u8,
        fin: bool,
        offset: usize,
        total: usize,
        bytes: &[u8],
    ) -> Result<Option<Vec<u8>>, &'static str> {
        if opcode >= 8 {
            return Ok(None);
        } // ping/pong/close do not disrupt text
        if offset == 0 {
            if self.frame_offset != self.frame_len {
                return Err("Truncated frame");
            }
            if opcode == 1 {
                if self.active {
                    return Err("Overlapping message");
                }
                self.active = true;
                self.message.clear();
            } else if opcode != 0 || !self.active {
                return Err("Unexpected frame opcode");
            }
            self.frame_len = total;
            self.frame_offset = 0;
        }
        if !self.active
            || offset != self.frame_offset
            || total != self.frame_len
            || bytes.len() > total.saturating_sub(offset)
        {
            return Err("Invalid frame offset");
        }
        if self.message.len() + bytes.len() > MAX_MESSAGE {
            return Err("Message limit exceeded");
        }
        self.message.extend_from_slice(bytes);
        self.frame_offset += bytes.len();
        if self.frame_offset == total && fin {
            self.active = false;
            return Ok(Some(std::mem::take(&mut self.message)));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receive_and_websocket_fragmentation() {
        let mut a = Assembler::default();
        assert_eq!(a.push(1, false, 0, 4, b"{\"ty").unwrap(), None);
        assert_eq!(a.push(9, true, 0, 1, b"x").unwrap(), None);
        assert_eq!(a.push(0, true, 0, 5, b"pe").unwrap(), None);
        assert_eq!(
            a.push(0, true, 2, 5, b"\":}").unwrap(),
            Some(b"{\"type\":}".to_vec())
        );
    }
    #[test]
    fn rejects_overflow_and_out_of_order() {
        let mut a = Assembler::default();
        assert!(a.push(1, true, 1, 2, b"x").is_err());
        let mut a = Assembler::default();
        assert!(a
            .push(1, true, 0, MAX_MESSAGE + 1, &vec![0; MAX_MESSAGE + 1])
            .is_err());
    }
    #[test]
    fn ga_audio_event_and_alignment() {
        assert_eq!(
            audio_delta(&json!({"type":"response.output_audio.delta","delta":"AQD//w=="})).unwrap(),
            Some(vec![1, 0, 255, 255])
        );
        assert!(
            audio_delta(&json!({"type":"response.output_audio.delta","delta":"AQ=="})).is_err()
        );
        assert_eq!(
            session_update("marin", "test")["session"]["audio"]["input"]["turn_detection"],
            Value::Null
        );
    }
}
