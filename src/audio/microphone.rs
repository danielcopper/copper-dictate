use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    Device, ErrorKind, FromSample, SampleFormat, SizedSample, Stream, SupportedStreamConfig,
    SupportedStreamConfigRange,
};

use crate::audio::resample::Resampler;
use crate::log;
use crate::session::{AudioSource, BoxError, Capture, Recording, Stopped};

/// The default input device of the default audio host (ALSA on Linux).
pub(crate) struct Microphone;

struct MicrophoneCapture {
    stream: Stream,
    captured: Arc<Mutex<Captured>>,
}

struct Captured {
    resampler: Resampler,
    samples: Vec<i16>,
    glitches: u32,
    failure: Option<cpal::Error>,
}

impl Captured {
    fn new(input_rate: u32, channels: u16) -> Self {
        Self {
            resampler: Resampler::new(input_rate, channels),
            samples: Vec::new(),
            glitches: 0,
            failure: None,
        }
    }

    fn push(&mut self, interleaved: impl IntoIterator<Item = f32>) {
        self.resampler.push(interleaved, &mut self.samples);
    }

    fn report(&mut self, error: cpal::Error) {
        match error.kind() {
            // cpal recovers from these on its own and keeps delivering audio.
            ErrorKind::Xrun | ErrorKind::DeviceChanged => {
                log::eprint(format_args!("the microphone reported a glitch: {error}"));
                self.glitches = self.glitches.saturating_add(1);
            }
            // cpal: "Audio will still play", only without real-time scheduling.
            ErrorKind::RealtimeDenied => {
                log::eprint(format_args!(
                    "the microphone records without real-time priority: {error}"
                ));
            }
            _ => {
                if self.failure.is_none() {
                    self.failure = Some(error);
                }
            }
        }
    }

    fn take(&mut self) -> Result<Stopped, BoxError> {
        match self.failure.take() {
            Some(error) => {
                Err(format!("the microphone failed during the recording: {error}").into())
            }
            None => Ok(Stopped {
                recording: Recording {
                    samples: mem::take(&mut self.samples),
                },
                glitches: mem::take(&mut self.glitches),
            }),
        }
    }
}

impl AudioSource for Microphone {
    fn start(&self) -> Result<Box<dyn Capture>, BoxError> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or("there is no default input device")?;
        let config = choose_config(&device)?;
        let captured = Arc::new(Mutex::new(Captured::new(
            config.sample_rate(),
            config.channels(),
        )));
        let stream = match config.sample_format() {
            SampleFormat::I8 => build::<i8>(&device, &config, &captured),
            SampleFormat::I16 => build::<i16>(&device, &config, &captured),
            SampleFormat::I32 => build::<i32>(&device, &config, &captured),
            SampleFormat::U8 => build::<u8>(&device, &config, &captured),
            SampleFormat::U16 => build::<u16>(&device, &config, &captured),
            SampleFormat::U32 => build::<u32>(&device, &config, &captured),
            SampleFormat::F32 => build::<f32>(&device, &config, &captured),
            SampleFormat::F64 => build::<f64>(&device, &config, &captured),
            other => {
                return Err(
                    format!("the input device's sample format {other} is not supported").into(),
                );
            }
        }?;
        stream.play()?;
        log::eprint(format_args!(
            "the microphone records at {}",
            describe(&config)
        ));
        Ok(Box::new(MicrophoneCapture { stream, captured }))
    }
}

impl Capture for MicrophoneCapture {
    fn stop(self: Box<Self>) -> Result<Stopped, BoxError> {
        let Self { stream, captured } = *self;
        drop(stream);
        lock(&captured).take()
    }
}

fn lock(captured: &Mutex<Captured>) -> MutexGuard<'_, Captured> {
    captured.lock().unwrap_or_else(PoisonError::into_inner)
}

fn choose_config(device: &Device) -> Result<SupportedStreamConfig, BoxError> {
    match device
        .supported_input_configs()
        .ok()
        .and_then(config_at_16_khz)
    {
        Some(config) => Ok(config),
        None => Ok(device.default_input_config()?),
    }
}

fn config_at_16_khz(
    configs: impl IntoIterator<Item = SupportedStreamConfigRange>,
) -> Option<SupportedStreamConfig> {
    configs
        .into_iter()
        .filter_map(|range| {
            let preference = format_preference(range.sample_format())?;
            Some((
                preference,
                range.try_with_sample_rate(Recording::SAMPLE_RATE)?,
            ))
        })
        .min_by_key(|(preference, config)| (*preference, config.channels()))
        .map(|(_, config)| config)
}

/// Lower is better; `None` for formats of fewer than 16 bits and for those
/// [`AudioSource::start`] cannot build a stream for.
fn format_preference(format: SampleFormat) -> Option<u8> {
    match format {
        SampleFormat::F32 => Some(0),
        SampleFormat::I16 => Some(1),
        SampleFormat::F64 => Some(2),
        SampleFormat::I32 => Some(3),
        SampleFormat::U32 => Some(4),
        SampleFormat::U16 => Some(5),
        _ => None,
    }
}

fn describe(config: &SupportedStreamConfig) -> String {
    let channels = config.channels();
    format!(
        "{} Hz, {channels} {}, {}",
        config.sample_rate(),
        if channels == 1 { "channel" } else { "channels" },
        config.sample_format()
    )
}

fn build<T>(
    device: &Device,
    config: &SupportedStreamConfig,
    captured: &Arc<Mutex<Captured>>,
) -> Result<Stream, BoxError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let for_data = Arc::clone(captured);
    let for_errors = Arc::clone(captured);
    let stream = device.build_input_stream(
        config.config(),
        move |data: &[T], _: &_| {
            lock(&for_data).push(data.iter().map(|sample| sample.to_sample::<f32>()));
        },
        move |error| lock(&for_errors).report(error),
        None,
    )?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use cpal::{
        ErrorKind, SampleFormat, SupportedBufferSize, SupportedStreamConfig,
        SupportedStreamConfigRange,
    };

    use super::{Captured, config_at_16_khz, describe};

    fn any_rate(channels: u16, format: SampleFormat) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(channels, 1, 384_000, SupportedBufferSize::Unknown, format)
    }

    fn at_16_khz(channels: u16, format: SampleFormat) -> SupportedStreamConfig {
        SupportedStreamConfig::new(channels, 16_000, SupportedBufferSize::Unknown, format)
    }

    #[test]
    fn float_is_chosen_over_the_8_bit_formats_listed_before_it() {
        let configs = [
            any_rate(1, SampleFormat::U8),
            any_rate(2, SampleFormat::U8),
            any_rate(1, SampleFormat::I16),
            any_rate(1, SampleFormat::I32),
            any_rate(2, SampleFormat::F32),
            any_rate(1, SampleFormat::F32),
        ];

        assert_eq!(
            config_at_16_khz(configs),
            Some(at_16_khz(1, SampleFormat::F32))
        );
    }

    #[test]
    fn without_float_16_bit_integers_come_first_whatever_their_channels() {
        let configs = [
            any_rate(1, SampleFormat::U8),
            any_rate(1, SampleFormat::I8),
            any_rate(1, SampleFormat::I32),
            any_rate(2, SampleFormat::I16),
        ];

        assert_eq!(
            config_at_16_khz(configs),
            Some(at_16_khz(2, SampleFormat::I16))
        );
    }

    #[test]
    fn another_format_of_at_least_16_bits_is_chosen_over_8_bit_ones() {
        let configs = [
            any_rate(1, SampleFormat::U8),
            any_rate(1, SampleFormat::I8),
            any_rate(2, SampleFormat::U16),
        ];

        assert_eq!(
            config_at_16_khz(configs),
            Some(at_16_khz(2, SampleFormat::U16))
        );
    }

    #[test]
    fn of_one_format_the_fewest_channels_are_chosen() {
        let configs = [
            any_rate(2, SampleFormat::F32),
            any_rate(1, SampleFormat::F32),
        ];

        assert_eq!(
            config_at_16_khz(configs),
            Some(at_16_khz(1, SampleFormat::F32))
        );
    }

    #[test]
    fn with_only_8_bit_or_1_bit_formats_at_16_khz_the_default_is_used() {
        let configs = [
            any_rate(1, SampleFormat::U8),
            any_rate(1, SampleFormat::I8),
            any_rate(1, SampleFormat::DsdU32),
            SupportedStreamConfigRange::new(
                1,
                44_100,
                48_000,
                SupportedBufferSize::Unknown,
                SampleFormat::F32,
            ),
        ];

        assert_eq!(config_at_16_khz(configs), None);
    }

    #[test]
    fn the_opened_configuration_is_described_by_rate_channels_and_format() {
        let mono = at_16_khz(1, SampleFormat::F32);
        let stereo =
            SupportedStreamConfig::new(2, 48_000, SupportedBufferSize::Unknown, SampleFormat::I16);

        assert_eq!(describe(&mono), "16000 Hz, 1 channel, f32");
        assert_eq!(describe(&stereo), "48000 Hz, 2 channels, i16");
    }

    #[test]
    fn a_stream_error_other_than_a_glitch_fails_the_stop() {
        let mut captured = Captured::new(16_000, 1);
        captured.push([0.5; 160]);

        captured.report(cpal::Error::with_message(
            ErrorKind::BackendError,
            "snd_pcm_readi failed",
        ));

        assert_eq!(
            captured.take().unwrap_err().to_string(),
            "the microphone failed during the recording: snd_pcm_readi failed"
        );
    }

    #[test]
    fn a_glitch_the_stream_recovers_from_keeps_the_recording() {
        let mut captured = Captured::new(16_000, 1);
        captured.push([0.5; 160]);

        captured.report(cpal::Error::new(ErrorKind::Xrun));
        captured.push([0.5; 160]);

        assert_eq!(captured.take().unwrap().recording.samples.len(), 320);
    }

    #[test]
    fn every_glitch_the_stream_recovers_from_is_counted() {
        let mut captured = Captured::new(16_000, 1);
        captured.push([0.5; 160]);

        captured.report(cpal::Error::new(ErrorKind::Xrun));
        captured.report(cpal::Error::new(ErrorKind::DeviceChanged));
        captured.report(cpal::Error::new(ErrorKind::RealtimeDenied));
        captured.report(cpal::Error::new(ErrorKind::Xrun));

        assert_eq!(
            captured.take().unwrap().glitches,
            3,
            "a refused real-time priority loses no audio and is no glitch"
        );
    }
}
