use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    Device, ErrorKind, FromSample, SampleFormat, SizedSample, Stream, SupportedStreamConfig,
};

use crate::resample::Resampler;
use crate::session::{AudioSource, BoxError, Capture, Recording};

/// The default input device of the default audio host (ALSA on Linux).
pub(crate) struct Microphone;

struct MicrophoneCapture {
    stream: Stream,
    captured: Arc<Mutex<Captured>>,
}

struct Captured {
    resampler: Resampler,
    samples: Vec<i16>,
    failure: Option<cpal::Error>,
}

impl Captured {
    fn new(input_rate: u32, channels: u16) -> Self {
        Self {
            resampler: Resampler::new(input_rate, channels),
            samples: Vec::new(),
            failure: None,
        }
    }

    fn push(&mut self, interleaved: impl IntoIterator<Item = f32>) {
        self.resampler.push(interleaved, &mut self.samples);
    }

    fn report(&mut self, error: cpal::Error) {
        // cpal recovers from these on its own and keeps delivering audio.
        if matches!(
            error.kind(),
            ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied
        ) {
            eprintln!("copper-dictate: the microphone reported a glitch: {error}");
        } else if self.failure.is_none() {
            self.failure = Some(error);
        }
    }

    fn take(&mut self) -> Result<Recording, BoxError> {
        match self.failure.take() {
            Some(error) => {
                Err(format!("the microphone stopped during the recording: {error}").into())
            }
            None => Ok(Recording {
                samples: mem::take(&mut self.samples),
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
        Ok(Box::new(MicrophoneCapture { stream, captured }))
    }
}

impl Capture for MicrophoneCapture {
    fn stop(self: Box<Self>) -> Result<Recording, BoxError> {
        let Self { stream, captured } = *self;
        drop(stream);
        lock(&captured).take()
    }
}

fn lock(captured: &Mutex<Captured>) -> MutexGuard<'_, Captured> {
    captured.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 16 kHz where the device offers it, which leaves resampling to the audio
/// server; the device's default otherwise.
fn choose_config(device: &Device) -> Result<SupportedStreamConfig, BoxError> {
    let at_16_khz = device.supported_input_configs().ok().and_then(|configs| {
        configs
            .filter_map(|range| range.try_with_sample_rate(Recording::SAMPLE_RATE))
            .min_by_key(SupportedStreamConfig::channels)
    });
    match at_16_khz {
        Some(config) => Ok(config),
        None => Ok(device.default_input_config()?),
    }
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
    use cpal::ErrorKind;

    use super::Captured;

    #[test]
    fn a_stream_error_that_ends_the_capture_fails_the_stop() {
        let mut captured = Captured::new(16_000, 1);
        captured.push([0.5; 160]);

        captured.report(cpal::Error::new(ErrorKind::DeviceNotAvailable));

        let error = captured.take().unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("the microphone stopped during the recording: "),
            "{error}"
        );
    }

    #[test]
    fn a_glitch_the_stream_recovers_from_keeps_the_recording() {
        let mut captured = Captured::new(16_000, 1);
        captured.push([0.5; 160]);

        captured.report(cpal::Error::new(ErrorKind::Xrun));
        captured.push([0.5; 160]);

        assert_eq!(captured.take().unwrap().samples.len(), 320);
    }
}
