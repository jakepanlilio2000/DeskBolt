use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
    mpsc as sync_channel,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;

use crate::audio_codec::{AUDIO_RATE, AUDIO_SAMPLES, AudioDecoder, AudioEncoder};
use crate::audio_io::{CaptureResampler, PlaybackBuffer, capture_stream, playback_stream, to_pcm};
use crate::echo::{EchoCanceller, REFERENCE_CAPACITY};

pub const VOICE_SAMPLE_RATE: u32 = AUDIO_RATE;
pub const VOICE_FRAME_SAMPLES: usize = AUDIO_SAMPLES;

/// Shared runtime controls updated from UI sliders/buttons and read lock-free by the audio thread.
#[derive(Debug)]
pub struct VoiceControls {
    pub bitrate_kbps: AtomicU32,
    pub muted: AtomicBool,
    pub ptt_required: AtomicBool,
    pub ptt_active: AtomicBool,
    pub noise_gate_milli: AtomicU32,
    pub speaker_vol_milli: AtomicU32,
    pub mic_level_milli: AtomicU32,
}

impl Default for VoiceControls {
    fn default() -> Self {
        Self {
            bitrate_kbps: AtomicU32::new(32),
            muted: AtomicBool::new(false),
            ptt_required: AtomicBool::new(false),
            ptt_active: AtomicBool::new(false),
            noise_gate_milli: AtomicU32::new(10), // 0.010 RMS gate
            speaker_vol_milli: AtomicU32::new(1000), // 1.0x gain
            mic_level_milli: AtomicU32::new(0),
        }
    }
}

/// Lists available OS audio input and output device names using pure-Rust `cpal`.
pub fn list_voice_devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let inputs = host
        .input_devices()
        .map(|it| it.map(|d| d.to_string()).collect())
        .unwrap_or_default();
    let outputs = host
        .output_devices()
        .map(|it| it.map(|d| d.to_string()).collect())
        .unwrap_or_default();
    (inputs, outputs)
}

pub struct VoiceEngine {
    playback: Arc<Mutex<PlaybackBuffer>>,
    decoder: Mutex<AudioDecoder>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl VoiceEngine {
    pub fn start(
        input_device_name: Option<String>,
        output_device_name: Option<String>,
        tx_frames: mpsc::Sender<Vec<u8>>,
        controls: Arc<VoiceControls>,
    ) -> Result<Self, String> {
        let decoder = AudioDecoder::new(opus::Channels::Mono).map_err(|e| e.to_string())?;
        let bitrate = controls.bitrate_kbps.load(Ordering::Relaxed).clamp(24, 48);
        let mut encoder =
            AudioEncoder::new(opus::Channels::Mono, bitrate).map_err(|e| e.to_string())?;
        let playback = Arc::new(Mutex::new(PlaybackBuffer::new(1)));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, init) = sync_channel::sync_channel(1);
        let queue = playback.clone();
        let stopping = stop.clone();

        let worker = thread::spawn(move || {
            let reference = Arc::new(Mutex::new(VecDeque::with_capacity(REFERENCE_CAPACITY)));
            let (capture_tx, capture_rx) = sync_channel::sync_channel::<Vec<i16>>(3);
            let input_controls = controls.clone();
            let mut capture = CaptureResampler::default();
            let mut frame = Vec::with_capacity(AUDIO_SAMPLES);
            let host = cpal::default_host();
            let input =
                select_input_device(&host, input_device_name.as_deref()).and_then(|device| {
                    let config = device.default_input_config().ok()?;
                    capture_stream(
                        &device,
                        config,
                        move |samples, channels, rate| {
                            let enabled = microphone_enabled(&input_controls);
                            for samples in samples.chunks_exact(channels) {
                                let mono = if enabled {
                                    samples.iter().sum::<f32>() / channels as f32
                                } else {
                                    0.0
                                };
                                capture.push([mono; 2], rate, |sample| {
                                    frame.push(if enabled { to_pcm(sample[0]) } else { 0 });
                                    if frame.len() == AUDIO_SAMPLES {
                                        let _ = capture_tx.try_send(std::mem::replace(
                                            &mut frame,
                                            Vec::with_capacity(AUDIO_SAMPLES),
                                        ));
                                    }
                                });
                            }
                        },
                        |_| {},
                    )
                    .ok()
                    .filter(|stream| stream.play().is_ok())
                });
            let output_controls = controls.clone();
            let output_reference = reference.clone();
            let mut render_resampler = CaptureResampler::default();
            let output =
                select_output_device(&host, output_device_name.as_deref()).and_then(|device| {
                    playback_stream(&device, move |samples, channels, rate| {
                        let gain = output_controls
                            .speaker_vol_milli
                            .load(Ordering::Relaxed)
                            .min(2000) as f32
                            / 1000.0;
                        queue.lock().unwrap().render(samples, channels, rate, gain);
                        let mut reference = output_reference.lock().unwrap();
                        for sample in samples.chunks_exact(channels) {
                            render_resampler.push([sample[0]; 2], rate, |sample| {
                                if reference.len() == REFERENCE_CAPACITY {
                                    reference.pop_front();
                                }
                                reference.push_back(to_pcm(sample[0]));
                            });
                        }
                    })
                    .ok()
                    .filter(|stream| stream.play().is_ok())
                });
            if input.is_none() && output.is_none() {
                let _ = ready.send(Err("Unable to open an audio input or output device.".into()));
                return;
            }
            if ready.send(Ok(())).is_err() {
                return;
            }
            let mut echo = EchoCanceller::default();
            let mut denoise = nnnoiseless::DenoiseState::new();
            let half = nnnoiseless::DenoiseState::FRAME_SIZE;
            let mut filtered = [0.0; nnnoiseless::DenoiseState::FRAME_SIZE];
            let mut playback_history = VecDeque::with_capacity(REFERENCE_CAPACITY);
            while !stopping.load(Ordering::Relaxed) {
                let mut samples = match capture_rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(samples) => samples,
                    Err(sync_channel::RecvTimeoutError::Timeout) => continue,
                    Err(sync_channel::RecvTimeoutError::Disconnected) => {
                        thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                };
                let enabled = microphone_enabled(&controls);
                if enabled {
                    playback_history.clone_from(&reference.lock().unwrap());
                    echo.process(&mut samples, &playback_history);
                    for chunk in samples.chunks_exact_mut(half) {
                        let pcm: [f32; nnnoiseless::DenoiseState::FRAME_SIZE] =
                            std::array::from_fn(|i| chunk[i] as f32);
                        denoise.process_frame(&mut filtered, &pcm);
                        for (sample, filtered) in chunk.iter_mut().zip(filtered) {
                            *sample = filtered.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                        }
                    }
                }
                let rms = if enabled {
                    (samples
                        .iter()
                        .map(|&x| (x as f32 / 32768.0).powi(2))
                        .sum::<f32>()
                        / AUDIO_SAMPLES as f32)
                        .sqrt()
                } else {
                    0.0
                };
                controls
                    .mic_level_milli
                    .store((rms * 1000.0).round() as u32, Ordering::Relaxed);
                let gate = controls.noise_gate_milli.load(Ordering::Relaxed) as f32 / 1000.0;
                if enabled
                    && microphone_enabled(&controls)
                    && rms >= gate
                    && let Ok(packet) = encoder.encode(&samples)
                {
                    let _ = tx_frames.try_send(packet);
                }
            }
        });
        match init.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                playback,
                decoder: Mutex::new(decoder),
                stop,
                worker: Some(worker),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("Audio device did not respond.".into())
            }
        }
    }

    pub fn push_remote_frame(&self, packet: &[u8]) {
        if let Some(samples) = self.decoder.lock().unwrap().decode(packet) {
            self.playback.lock().unwrap().push(samples);
        }
    }
}

impl Drop for VoiceEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn microphone_enabled(controls: &VoiceControls) -> bool {
    !controls.muted.load(Ordering::Relaxed)
        && (!controls.ptt_required.load(Ordering::Relaxed)
            || controls.ptt_active.load(Ordering::Relaxed))
}

fn select_input_device(host: &cpal::Host, wanted: Option<&str>) -> Option<cpal::Device> {
    if let Some(name) =
        wanted.filter(|s| !s.trim().is_empty() && !s.eq_ignore_ascii_case("Default"))
        && let Ok(mut devs) = host.input_devices()
        && let Some(found) = devs.find(|d| d.to_string() == name)
    {
        return Some(found);
    }
    host.default_input_device()
}

pub(crate) fn select_output_device(
    host: &cpal::Host,
    wanted: Option<&str>,
) -> Option<cpal::Device> {
    if let Some(name) =
        wanted.filter(|s| !s.trim().is_empty() && !s.eq_ignore_ascii_case("Default"))
        && let Ok(mut devs) = host.output_devices()
        && let Some(found) = devs.find(|d| d.to_string() == name)
    {
        return Some(found);
    }
    host.default_output_device()
}
