use std::mem;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SampleFormat, SizedSample, Stream, SupportedStreamConfig};

use crate::resample::Resampler;
use crate::session::{AudioSource, BoxError, Capture, Recording};

/// The default input device of the default audio host (ALSA on Linux).
pub(crate) struct Microphone;

struct MicrophoneCapture {
    stop: Sender<()>,
    thread: JoinHandle<Vec<i16>>,
}

type Samples = Arc<Mutex<(Resampler, Vec<i16>)>>;

impl AudioSource for Microphone {
    fn start(&self) -> Result<Box<dyn Capture>, BoxError> {
        let (started, wait_started) = mpsc::channel();
        let (stop, wait_stop) = mpsc::channel();
        // cpal's Stream is not Send, so one thread owns it from start to stop.
        let thread = thread::spawn(move || record(&started, &wait_stop));
        match wait_started.recv() {
            Ok(Ok(())) => Ok(Box::new(MicrophoneCapture { stop, thread })),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("the recording thread ended before it started".into()),
        }
    }
}

impl Capture for MicrophoneCapture {
    fn stop(self: Box<Self>) -> Result<Recording, BoxError> {
        let _ = self.stop.send(());
        let samples = self
            .thread
            .join()
            .map_err(|_| "the recording thread panicked")?;
        Ok(Recording { samples })
    }
}

fn record(started: &Sender<Result<(), BoxError>>, wait_stop: &Receiver<()>) -> Vec<i16> {
    let (stream, samples) = match open() {
        Ok(opened) => opened,
        Err(error) => {
            let _ = started.send(Err(error));
            return Vec::new();
        }
    };
    let _ = started.send(Ok(()));
    let _ = wait_stop.recv();
    drop(stream);
    let mut samples = samples.lock().unwrap_or_else(PoisonError::into_inner);
    mem::take(&mut samples.1)
}

fn open() -> Result<(Stream, Samples), BoxError> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("there is no default input device")?;
    let config = choose_config(&device)?;
    let samples: Samples = Arc::new(Mutex::new((
        Resampler::new(config.sample_rate(), config.channels()),
        Vec::new(),
    )));
    let stream = match config.sample_format() {
        SampleFormat::I8 => build::<i8>(&device, &config, &samples),
        SampleFormat::I16 => build::<i16>(&device, &config, &samples),
        SampleFormat::I32 => build::<i32>(&device, &config, &samples),
        SampleFormat::U8 => build::<u8>(&device, &config, &samples),
        SampleFormat::U16 => build::<u16>(&device, &config, &samples),
        SampleFormat::U32 => build::<u32>(&device, &config, &samples),
        SampleFormat::F32 => build::<f32>(&device, &config, &samples),
        SampleFormat::F64 => build::<f64>(&device, &config, &samples),
        other => {
            return Err(
                format!("the input device's sample format {other} is not supported").into(),
            );
        }
    }?;
    stream.play()?;
    Ok((stream, samples))
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
    samples: &Samples,
) -> Result<Stream, BoxError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let samples = Arc::clone(samples);
    let stream = device.build_input_stream(
        config.config(),
        move |data: &[T], _: &_| {
            let mut samples = samples.lock().unwrap_or_else(PoisonError::into_inner);
            let (resampler, out) = &mut *samples;
            resampler.push(data.iter().map(|sample| sample.to_sample::<f32>()), out);
        },
        |error| eprintln!("copper-dictate: the microphone reported an error: {error}"),
        None,
    )?;
    Ok(stream)
}
