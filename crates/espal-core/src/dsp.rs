// 63-tap Blackman-windowed sinc, 10kHz cutoff at 48kHz, unity DC gain.
// Generated with sinc(2*10000/48000*n) and the symmetric Blackman window.
const TAPS: [f32; 63] = [
    -3.68817513525e-20f32,
    9.84370951196e-06f32,
    1.06580511989e-05f32,
    -8.45854524301e-05f32,
    -0.000130368190181f32,
    0.000153916275707f32,
    0.000460005977382f32,
    -8.56557734704e-19f32,
    -0.000955352639071f32,
    -0.000679275028273f32,
    0.00128925616612f32,
    0.00207901643969f32,
    -0.000804982937296f32,
    -0.00397408893157f32,
    -0.00129857154798f32,
    0.00542870612509f32,
    0.00548803747466f32,
    -0.00476770401518f32,
    -0.0112433764378f32,
    8.6565420565e-18f32,
    0.0165258300163f32,
    0.0103332999276f32,
    -0.0176564925293f32,
    -0.026203392027f32,
    0.00955080276068f32,
    0.0455278226671f32,
    0.0148210689897f32,
    -0.0644097235109f32,
    -0.0722312825122f32,
    0.0782482575069f32,
    0.306176160467f32,
    0.41667302641f32,
    0.306176160467f32,
    0.0782482575069f32,
    -0.0722312825122f32,
    -0.0644097235109f32,
    0.0148210689897f32,
    0.0455278226671f32,
    0.00955080276068f32,
    -0.026203392027f32,
    -0.0176564925293f32,
    0.0103332999276f32,
    0.0165258300163f32,
    8.6565420565e-18f32,
    -0.0112433764378f32,
    -0.00476770401518f32,
    0.00548803747466f32,
    0.00542870612509f32,
    -0.00129857154798f32,
    -0.00397408893157f32,
    -0.000804982937296f32,
    0.00207901643969f32,
    0.00128925616612f32,
    -0.000679275028273f32,
    -0.000955352639071f32,
    -8.56557734704e-19f32,
    0.000460005977382f32,
    0.000153916275707f32,
    -0.000130368190181f32,
    -8.45854524301e-05f32,
    1.06580511989e-05f32,
    9.84370951196e-06f32,
    -3.68817513525e-20f32,
];

#[derive(Clone)]
struct Fir {
    history: [f32; 63],
    cursor: usize,
}
impl Fir {
    fn new() -> Self {
        Self {
            history: [0.0; 63],
            cursor: 0,
        }
    }
    fn store(&mut self, sample: f32) {
        self.history[self.cursor] = sample;
        self.cursor = (self.cursor + 1) % 63;
    }
    fn output(&self) -> f32 {
        let mut result = 0.0;
        let mut index = if self.cursor == 0 {
            62
        } else {
            self.cursor - 1
        };
        for tap in TAPS {
            result += tap * self.history[index];
            index = if index == 0 { 62 } else { index - 1 };
        }
        result
    }
    #[cfg(test)]
    fn push(&mut self, sample: f32) -> f32 {
        self.store(sample);
        self.output()
    }
}

fn pcm16(value: f32) -> i16 {
    value.round().clamp(-32768.0, 32767.0) as i16
}

pub struct Downsampler {
    fir: Fir,
    phase: bool,
    gain: f32,
    pub clipped: u64,
}
impl Downsampler {
    pub fn new(gain: u8) -> Self {
        Self {
            fir: Fir::new(),
            phase: false,
            gain: gain.clamp(1, 32) as f32,
            clipped: 0,
        }
    }
    /// Left-aligned signed PCM24 from the mic -> filtered 24kHz PCM16.
    /// Maintain this object across DMA/chunk boundaries; reset between turns.
    pub fn push_word(&mut self, word: i32) -> Option<i16> {
        self.fir.store((word >> 8) as f32 * self.gain / 256.0);
        self.phase = !self.phase;
        if self.phase {
            return None;
        }
        // Decimation discards alternate outputs. Retain every input sample,
        // but only run the convolution for the outputs we actually emit.
        let filtered = self.fir.output();
        if !(-32768.0..=32767.0).contains(&filtered) {
            self.clipped += 1;
        }
        Some(pcm16(filtered))
    }
}

pub struct Upsampler {
    history: [f32; 32],
    cursor: usize,
}
impl Default for Upsampler {
    fn default() -> Self {
        Self::new()
    }
}
impl Upsampler {
    pub fn new() -> Self {
        Self {
            history: [0.0; 32],
            cursor: 0,
        }
    }
    /// Zero insertion and reconstruction filtering; fixed gain prevents the
    /// amplitude loss from zero stuffing. Output at 48kHz, no slot conversion.
    pub fn push(&mut self, sample: i16) -> [i16; 2] {
        // Polyphase form of the same zero-stuffed FIR: skip multiplications
        // by inserted zeros and compute both output phases from input history.
        self.history[self.cursor] = sample as f32 * 2.0;
        let mut even = 0.0;
        let mut odd = 0.0;
        for index in 0..32 {
            let value = self.history[(self.cursor + 32 - index) & 31];
            even += TAPS[index * 2] * value;
            if index < 31 {
                odd += TAPS[index * 2 + 1] * value;
            }
        }
        self.cursor = (self.cursor + 1) & 31;
        [pcm16(even), pcm16(odd)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decimation_matches_full_rate_reference() {
        for gain in [1, 8, 32] {
            let mut reference = Fir::new();
            let mut optimized = Downsampler::new(gain);
            let mut clipped = 0;
            for index in 0..8192 {
                let word = (index as i32).wrapping_mul(7919 * 65536);
                let filtered = reference.push((word >> 8) as f32 * gain as f32 / 256.0);
                let expected = if index % 2 == 1 {
                    if !(-32768.0..=32767.0).contains(&filtered) {
                        clipped += 1;
                    }
                    Some(pcm16(filtered))
                } else {
                    None
                };
                assert_eq!(optimized.push_word(word), expected);
            }
            assert_eq!(optimized.clipped, clipped);
        }
    }
    fn response(hz: f32) -> f32 {
        let mut filter = Downsampler::new(1);
        let mut energy = 0.0;
        let mut count = 0;
        for i in 0..48000 {
            let sample =
                (10000.0 * (std::f64::consts::TAU * hz as f64 * i as f64 / 48000.0).sin()) as i32;
            if let Some(v) = filter.push_word(sample << 16) {
                if i > 1000 {
                    energy += (v as f32).powi(2);
                    count += 1;
                }
            }
        }
        (energy / count as f32).sqrt()
    }
    #[test]
    fn polyphase_matches_zero_stuffed_reference() {
        let mut reference = Fir::new();
        let mut optimized = Upsampler::new();
        for index in 0..4096 {
            let sample = ((index * 7919 % 65536) as i32 - 32768) as i16;
            let expected = [
                pcm16(reference.push(sample as f32 * 2.0)),
                pcm16(reference.push(0.0)),
            ];
            assert_eq!(optimized.push(sample), expected);
        }
    }
    #[test]
    fn anti_alias_filter() {
        let speech = response(1000.0);
        let alias = response(16000.0);
        assert!((speech / 7071.0 - 1.0).abs() < 0.01);
        assert!(alias / speech < 0.001, "alias/speech={}", alias / speech);
    }
    #[test]
    fn signed_alignment_gain_and_saturation() {
        for (word, want) in [
            (0, 0),
            (1000 << 16, 8000),
            (-1000 << 16, -8000),
            (i32::MAX, 32767),
            (i32::MIN, -32768),
        ] {
            let mut d = Downsampler::new(8);
            let mut last = 0;
            for _ in 0..100 {
                if let Some(v) = d.push_word(word) {
                    last = v;
                }
            }
            assert!((last as i32 - want).abs() <= 1);
        }
    }
    #[test]
    fn exact_counts_and_continuity() {
        let mut a = Downsampler::new(1);
        let mut b = Downsampler::new(1);
        let input: Vec<i32> = (0..960).map(|i| i << 16).collect();
        let expected: Vec<_> = input.iter().filter_map(|v| a.push_word(*v)).collect();
        let actual: Vec<_> = input
            .chunks(137)
            .flat_map(|c| c.iter().filter_map(|v| b.push_word(*v)).collect::<Vec<_>>())
            .collect();
        assert_eq!(expected.len(), 480);
        assert_eq!(expected, actual);
    }
    #[test]
    fn interpolation_dc_and_image_rejection() {
        let mut u = Upsampler::new();
        for _ in 0..100 {
            u.push(1000);
        }
        for v in u.push(1000) {
            assert!((v as i32 - 1000).abs() <= 1);
        }
        let mut u = Upsampler::new();
        let mut samples = Vec::new();
        for i in 0..24000 {
            samples.extend(u.push(
                (10000.0 * (std::f32::consts::TAU * 1000.0 * i as f32 / 24000.0).sin()) as i16,
            ));
        }
        let amplitude = |hz: f64| {
            let (mut re, mut im) = (0.0, 0.0);
            for (i, v) in samples.iter().enumerate().skip(1000) {
                let phase = std::f64::consts::TAU * hz * i as f64 / 48000.0;
                re += *v as f64 * phase.cos();
                im += *v as f64 * phase.sin();
            }
            (re * re + im * im).sqrt()
        };
        assert!(amplitude(23000.0) / amplitude(1000.0) < 0.001);
    }
}
