use thiserror::Error;

pub(crate) const SAMPLE_RATE: u32 = 16_000;
const CHANNELS: u16 = 1;
const BITS_PER_SAMPLE: u16 = 16;
const BYTES_PER_SAMPLE: u32 = 2;
const HEADER_LEN: u32 = 44;

#[derive(Debug, Error)]
#[error("{samples} samples do not fit in a WAV file")]
pub(crate) struct TooLong {
    samples: usize,
}

/// Encodes 16 kHz mono samples as a 16-bit PCM WAV file.
pub(crate) fn encode(samples: &[i16]) -> Result<Vec<u8>, TooLong> {
    let data_len = data_len(samples.len()).ok_or(TooLong {
        samples: samples.len(),
    })?;

    let mut wav = Vec::with_capacity(samples.len() * 2 + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(data_len + HEADER_LEN - 8).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&CHANNELS.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * BYTES_PER_SAMPLE).to_le_bytes());
    wav.extend_from_slice(&(CHANNELS * BITS_PER_SAMPLE / 8).to_le_bytes());
    wav.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(wav)
}

fn data_len(samples: usize) -> Option<u32> {
    u32::try_from(samples)
        .ok()
        .and_then(|count| count.checked_mul(BYTES_PER_SAMPLE))
        .filter(|len| len.checked_add(HEADER_LEN).is_some())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{data_len, encode};

    /// The format fields and samples of a WAV file `encode` wrote.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) struct Decoded {
        pub(crate) format: u16,
        pub(crate) channels: u16,
        pub(crate) sample_rate: u32,
        pub(crate) bits_per_sample: u16,
        pub(crate) samples: Vec<i16>,
    }

    pub(crate) fn decode(wav: &[u8]) -> Decoded {
        let u16_at = |at: usize| u16::from_le_bytes([wav[at], wav[at + 1]]);
        let u32_at = |at: usize| u32::from_le_bytes(wav[at..at + 4].try_into().unwrap());
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32_at(4) as usize, wav.len() - 8);
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32_at(16), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32_at(40) as usize, wav.len() - 44);
        let channels = u16_at(22);
        let sample_rate = u32_at(24);
        let bits_per_sample = u16_at(34);
        assert_eq!(
            u32_at(28),
            sample_rate * u32::from(channels * bits_per_sample / 8)
        );
        assert_eq!(u16_at(32), channels * bits_per_sample / 8);
        Decoded {
            format: u16_at(20),
            channels,
            sample_rate,
            bits_per_sample,
            samples: wav[44..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| i16::from_le_bytes(pair))
                .collect(),
        }
    }

    #[test]
    fn encodes_16_khz_mono_16_bit_pcm() {
        let samples = [0, 1, -1, i16::MAX, i16::MIN, 1234];

        let decoded = decode(&encode(&samples).unwrap());

        assert_eq!(
            decoded,
            Decoded {
                format: 1,
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 16,
                samples: samples.to_vec(),
            }
        );
    }

    #[test]
    fn encodes_an_empty_recording_as_a_header_only() {
        let wav = encode(&[]).unwrap();

        assert_eq!(wav.len(), 44);
        assert_eq!(decode(&wav).samples, Vec::<i16>::new());
    }

    #[test]
    fn refuses_more_samples_than_a_wav_file_can_hold() {
        let most = (u32::MAX - 44) / 2;

        assert_eq!(data_len(most as usize), Some(most * 2));
        assert_eq!(data_len(most as usize + 1), None);
    }
}
