use cpal::Sample;

use crate::wav::SAMPLE_RATE;

/// Turns a microphone's interleaved samples, at any rate and channel count, into
/// 16 kHz mono, one chunk at a time.
///
/// Downsampling averages the input over each output sample's span, which also
/// filters out most of what lies above the new Nyquist frequency; upsampling
/// interpolates linearly.
pub(crate) struct Resampler {
    channels: u16,
    step: f64,
    frame: Vec<f64>,
    sum: f64,
    filled: f64,
    previous: Option<f64>,
    position: f64,
}

impl Resampler {
    pub(crate) fn new(input_rate: u32, channels: u16) -> Self {
        let channels = channels.max(1);
        Self {
            channels,
            step: f64::from(input_rate) / f64::from(SAMPLE_RATE),
            frame: Vec::with_capacity(usize::from(channels)),
            sum: 0.0,
            filled: 0.0,
            previous: None,
            position: 0.0,
        }
    }

    pub(crate) fn push(&mut self, interleaved: impl IntoIterator<Item = f32>, out: &mut Vec<i16>) {
        for sample in interleaved {
            self.frame.push(f64::from(sample));
            if self.frame.len() == usize::from(self.channels) {
                let mono = self.frame.iter().sum::<f64>() / f64::from(self.channels);
                self.frame.clear();
                if self.step >= 1.0 {
                    self.average(mono, out);
                } else {
                    self.interpolate(mono, out);
                }
            }
        }
    }

    fn average(&mut self, sample: f64, out: &mut Vec<i16>) {
        let mut weight = 1.0;
        while self.filled + weight >= self.step {
            let taken = self.step - self.filled;
            out.push(to_i16((self.sum + sample * taken) / self.step));
            weight -= taken;
            self.sum = 0.0;
            self.filled = 0.0;
        }
        self.sum += sample * weight;
        self.filled += weight;
    }

    fn interpolate(&mut self, sample: f64, out: &mut Vec<i16>) {
        let Some(previous) = self.previous.replace(sample) else {
            return;
        };
        while self.position < 1.0 {
            out.push(to_i16(previous + (sample - previous) * self.position));
            self.position += self.step;
        }
        self.position -= 1.0;
    }
}

fn to_i16(sample: f64) -> i16 {
    sample.clamp(-1.0, 1.0).to_sample()
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use cpal::Sample;

    use super::Resampler;

    fn tone(frequency: f64, rate: u32, frames: u32, channels: u16) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| {
                let value = 0.5 * (TAU * frequency * f64::from(n) / f64::from(rate)).sin();
                std::iter::repeat_n(value.to_sample::<f32>(), usize::from(channels))
            })
            .collect()
    }

    fn rising_zero_crossings(samples: &[i16]) -> usize {
        samples
            .windows(2)
            .filter(|pair| pair[0] < 0 && pair[1] >= 0)
            .count()
    }

    fn resample_in_chunks(input_rate: u32, channels: u16, input: &[f32]) -> Vec<i16> {
        let mut resampler = Resampler::new(input_rate, channels);
        let mut out = Vec::new();
        for chunk in input.chunks(333) {
            resampler.push(chunk.iter().copied(), &mut out);
        }
        out
    }

    #[test]
    fn downsamples_48_khz_stereo_to_16_khz_mono_keeping_the_pitch() {
        let out = resample_in_chunks(48_000, 2, &tone(440.0, 48_000, 96_000, 2));

        assert_eq!(out.len(), 32_000);
        assert!((879..=881).contains(&rising_zero_crossings(&out)));
        let peak = out.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((15_500..=16_500).contains(&peak), "peak {peak}");
    }

    #[test]
    fn downsamples_44_1_khz_to_16_khz() {
        let out = resample_in_chunks(44_100, 1, &tone(440.0, 44_100, 88_200, 1));

        assert!(
            (31_999..=32_000).contains(&out.len()),
            "{} samples",
            out.len()
        );
        assert!((879..=881).contains(&rising_zero_crossings(&out)));
    }

    #[test]
    fn passes_16_khz_mono_through_unchanged() {
        let input = tone(440.0, 16_000, 8_000, 1);

        let out = resample_in_chunks(16_000, 1, &input);

        let expected: Vec<i16> = input.iter().map(|&s| super::to_i16(f64::from(s))).collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn upsamples_8_khz_to_16_khz() {
        let out = resample_in_chunks(8_000, 1, &tone(440.0, 8_000, 16_000, 1));

        assert!(
            (31_998..=32_000).contains(&out.len()),
            "{} samples",
            out.len()
        );
        assert!((879..=881).contains(&rising_zero_crossings(&out)));
    }

    #[test]
    fn mixes_the_channels_down_by_averaging() {
        let out = resample_in_chunks(16_000, 2, &[0.5, -0.5, 0.5, 0.0]);

        assert_eq!(out, [0, 8192]);
    }

    #[test]
    fn clamps_samples_outside_full_scale() {
        let out = resample_in_chunks(16_000, 1, &[2.0, -2.0]);

        assert_eq!(out, [i16::MAX, i16::MIN]);
    }
}
