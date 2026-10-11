//! Local endpointing for 50ms, left-aligned 48kHz microphone blocks.
//! This detects energy and pauses; it is not a wake-word recognizer.
#[derive(Debug, PartialEq, Eq)]
pub enum State {
    Waiting,
    Started,
    Speaking,
    Ended,
    NoSpeech,
    Limit,
}

pub struct SpeechGate {
    noise: f32,
    onset: usize,
    quiet: usize,
    blocks: usize,
    started: bool,
}

impl Default for SpeechGate {
    fn default() -> Self {
        Self {
            noise: 20.0,
            onset: 0,
            quiet: 0,
            blocks: 0,
            started: false,
        }
    }
}

impl SpeechGate {
    pub fn push(&mut self, words: &[i32]) -> State {
        self.blocks += 1;
        let energy = words
            .iter()
            .map(|word| {
                let sample = i64::from(word >> 16);
                sample * sample
            })
            .sum::<i64>() as f32
            / words.len().max(1) as f32;
        // Native PCM16 on this microphone is quiet: measured room noise is
        // about 4–20 RMS, and ordinary speech can be below the old 180 floor.
        let threshold = (self.noise * 3.0).max(60.0);
        let voice = energy >= threshold * threshold;
        if self.blocks >= 12_000 {
            return State::Limit;
        }
        if !self.started {
            if voice {
                self.onset += 1;
                if self.onset >= 3 {
                    self.started = true;
                    return State::Started;
                }
            } else {
                self.onset = 0;
                self.noise = (self.noise * 0.97 + energy.sqrt() * 0.03).clamp(4.0, 1000.0);
            }
            if self.blocks >= 300 {
                State::NoSpeech
            } else {
                State::Waiting
            }
        } else {
            self.quiet = if voice { 0 } else { self.quiet + 1 };
            if self.quiet >= 24 {
                State::Ended
            } else {
                State::Speaking
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn block(level: i32) -> Vec<i32> {
        vec![level << 16; 2400]
    }
    #[test]
    fn long_speech_short_pause_and_endpoint() {
        let mut gate = SpeechGate::default();
        for _ in 0..20 {
            assert_eq!(gate.push(&block(30)), State::Waiting);
        }
        gate.push(&block(2000));
        gate.push(&block(2000));
        assert_eq!(gate.push(&block(2000)), State::Started);
        for _ in 0..240 {
            assert_eq!(gate.push(&block(-2000)), State::Speaking);
        }
        for _ in 0..14 {
            assert_eq!(gate.push(&block(30)), State::Speaking);
        }
        assert_eq!(gate.push(&block(2000)), State::Speaking);
        for _ in 0..23 {
            assert_eq!(gate.push(&block(30)), State::Speaking);
        }
        assert_eq!(gate.push(&block(30)), State::Ended);
    }
    #[test]
    fn background_and_brief_impulse_do_not_start_turn() {
        let mut gate = SpeechGate::default();
        gate.push(&block(3000));
        gate.push(&block(3000));
        for _ in 0..297 {
            assert_eq!(gate.push(&block(8)), State::Waiting);
        }
        assert_eq!(gate.push(&block(8)), State::NoSpeech);
    }
    #[test]
    fn quiet_speech_starts_without_waiting_for_louder_words() {
        let mut gate = SpeechGate::default();
        assert_eq!(gate.push(&block(100)), State::Waiting);
        assert_eq!(gate.push(&block(100)), State::Waiting);
        assert_eq!(gate.push(&block(100)), State::Started);
        for _ in 0..23 {
            assert_eq!(gate.push(&block(8)), State::Speaking);
        }
        assert_eq!(gate.push(&block(8)), State::Ended);
    }
    #[test]
    fn finite_limit_on_continuous_input() {
        let mut gate = SpeechGate::default();
        for _ in 0..11_999 {
            gate.push(&block(2000));
        }
        assert_eq!(gate.push(&block(2000)), State::Limit);
    }
}
