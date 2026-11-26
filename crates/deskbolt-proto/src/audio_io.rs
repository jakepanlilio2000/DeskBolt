use std::collections::VecDeque;

use cpal::traits::DeviceTrait;
use cpal::{Sample, SampleFormat, StreamConfig};

use crate::audio_codec::AUDIO_RATE;

pub(crate) fn capture_stream(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    mut capture: impl FnMut(&[f32], usize, u32) + Send + 'static,
    error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, String> {
    let channels = usize::from(config.channels());
    let rate = config.sample_rate();
    let stream = config.config();
    macro_rules! input {
        ($sample:ty) => {{
            let mut scratch = Vec::new();
            device.build_input_stream(
                stream,
                move |data: &[$sample], _| {
                    scratch.clear();
                    scratch.extend(data.iter().map(|sample| sample.to_sample::<f32>()));
                    capture(&scratch, channels, rate);
                },
                error,
                None,
            )
        }};
    }
    match config.sample_format() {
        SampleFormat::F32 => device.build_input_stream(
            stream,
            move |data: &[f32], _| capture(data, channels, rate),
            error,
            None,
        ),
        SampleFormat::F64 => input!(f64),
        SampleFormat::I16 => input!(i16),
        SampleFormat::I32 => input!(i32),
        SampleFormat::U16 => input!(u16),
        other => return Err(format!("Unsupported audio input format {other:?}.")),
    }
    .map_err(|e| e.to_string())
}

pub(crate) fn playback_stream(
    device: &cpal::Device,
    mut render: impl FnMut(&mut [f32], usize, u32) + Send + 'static,
) -> Result<cpal::Stream, String> {
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let channels = usize::from(config.channels());
    let rate = config.sample_rate();
    let stream: StreamConfig = config.config();
    macro_rules! output {
        ($sample:ty) => {{
            let mut scratch = Vec::new();
            device.build_output_stream(
                stream,
                move |data: &mut [$sample], _| {
                    scratch.resize(data.len(), 0.0);
                    render(&mut scratch, channels, rate);
                    for (sample, value) in data.iter_mut().zip(&scratch) {
                        *sample = value.to_sample::<$sample>();
                    }
                },
                |_| {},
                None,
            )
        }};
    }
    match config.sample_format() {
        SampleFormat::F32 => device.build_output_stream(
            stream,
            move |data: &mut [f32], _| render(data, channels, rate),
            |_| {},
            None,
        ),
        SampleFormat::F64 => output!(f64),
        SampleFormat::I16 => output!(i16),
        SampleFormat::I32 => output!(i32),
        SampleFormat::U16 => output!(u16),
        other => return Err(format!("Unsupported audio output format {other:?}.")),
    }
    .map_err(|e| e.to_string())
}

/// Streaming interpolation preserves the sample clock across callback boundaries.
#[derive(Default)]
pub(crate) struct CaptureResampler {
    previous: Option<[f32; 2]>,
    phase: u32,
}

impl CaptureResampler {
    pub(crate) fn push(&mut self, frame: [f32; 2], rate: u32, mut sample: impl FnMut([f32; 2])) {
        if let Some(previous) = self.previous.replace(frame) {
            while self.phase < AUDIO_RATE {
                let weight = self.phase as f32 / AUDIO_RATE as f32;
                sample(std::array::from_fn(|ch| {
                    previous[ch] + (frame[ch] - previous[ch]) * weight
                }));
                self.phase += rate;
            }
            self.phase -= AUDIO_RATE;
        }
    }
}

pub(crate) struct PlaybackBuffer {
    queue: VecDeque<i16>,
    channels: usize,
    phase: u32,
}

impl PlaybackBuffer {
    pub(crate) fn new(channels: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            channels,
            phase: 0,
        }
    }

    pub(crate) fn push(&mut self, samples: Vec<i16>) {
        let cap = AUDIO_RATE as usize / 5 * self.channels;
        let discard = (self.queue.len() + samples.len())
            .saturating_sub(cap)
            .min(self.queue.len());
        self.queue.drain(..discard);
        self.queue.extend(samples);
    }

    pub(crate) fn render(&mut self, buffer: &mut [f32], channels: usize, rate: u32, gain: f32) {
        for frame in buffer.chunks_exact_mut(channels) {
            let weight = self.phase as f32 / rate as f32;
            let value = |channel: usize| {
                if self.queue.is_empty() {
                    return 0.0;
                }
                let a = self.queue.get(channel).copied().unwrap_or_default() as f32;
                let b = self
                    .queue
                    .get(self.channels + channel)
                    .map_or(a, |&sample| sample as f32);
                ((a + (b - a) * weight) * (gain / 32768.0)).clamp(-1.0, 1.0)
            };
            let left = value(0);
            let right = value(self.channels - 1);
            if channels == 1 {
                frame[0] = (left + right) * 0.5;
            } else {
                frame.fill(0.0);
                frame[0] = left;
                frame[1] = right;
            }
            self.phase += AUDIO_RATE;
            let discard = (self.phase / rate) as usize * self.channels;
            self.phase %= rate;
            self.queue.drain(..discard.min(self.queue.len()));
        }
    }
}

pub(crate) fn to_pcm(value: f32) -> i16 {
    (value * 32768.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16
}
