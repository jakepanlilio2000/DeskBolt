use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig};
use tokio::sync::mpsc;

pub const VOICE_SAMPLE_RATE: u32 = 16_000;
pub const VOICE_FRAME_SAMPLES: usize = 320; // 20 ms at 16 kHz mono

/// Shared runtime controls updated from UI sliders/buttons and read lock-free by the audio thread.
#[derive(Debug)]
pub struct VoiceControls {
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
            muted: AtomicBool::new(false),
            ptt_required: AtomicBool::new(false),
            ptt_active: AtomicBool::new(false),
            noise_gate_milli: AtomicU32::new(10),  // 0.010 RMS gate
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

/// Pure-Rust bidirectional 16 kHz mono PCM voice engine backed by a dedicated OS audio thread.
pub struct VoiceEngine {
    pub controls: Arc<VoiceControls>,
    playback_q: Arc<Mutex<VecDeque<i16>>>,
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
        let playback_q = Arc::new(Mutex::new(VecDeque::<i16>::with_capacity(VOICE_SAMPLE_RATE as usize)));
        let stop = Arc::new(AtomicBool::new(false));
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let pb_c = playback_q.clone();
        let ctrl_c = controls.clone();
        let stop_c = stop.clone();

        let worker = thread::spawn(move || {
            let host = cpal::default_host();
            let in_dev = select_input_device(&host, input_device_name.as_deref());
            let out_dev = select_output_device(&host, output_device_name.as_deref());
            if in_dev.is_none() && out_dev.is_none() {
                let _ = init_tx.send(Err("No audio input or output devices detected.".into()));
                return;
            }

            let in_stream = in_dev.and_then(|dev| build_capture_stream(&dev, &tx_frames, &ctrl_c).ok());
            let out_stream = out_dev.and_then(|dev| build_playback_stream(&dev, &pb_c, &ctrl_c).ok());

            if let Some(s) = &in_stream {
                let _ = s.play();
            }
            if let Some(s) = &out_stream {
                let _ = s.play();
            }
            if in_stream.is_none() && out_stream.is_none() {
                let _ = init_tx.send(Err("Unable to open audio capture or playback stream.".into()));
                return;
            }
            let _ = init_tx.send(Ok(()));

            while !stop_c.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(50));
            }
            drop(in_stream);
            drop(out_stream);
        });

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                controls,
                playback_q,
                stop,
                worker: Some(worker),
            }),
            Ok(Err(e)) => {
                stop.store(true, Ordering::Relaxed);
                let _ = worker.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                let _ = worker.join();
                Err("Audio thread exited unexpectedly".into())
            }
        }
    }

    /// Queues an incoming 16 kHz mono `i16` little-endian frame for playback.
    pub fn push_remote_frame(&self, bytes: &[u8]) {
        let samples = decode_i16_le(bytes);
        if samples.is_empty() {
            return;
        }
        let mut q = self.playback_q.lock().unwrap();
        // Bound latency drift to ~500 ms max
        while q.len() + samples.len() > (VOICE_SAMPLE_RATE as usize / 2) {
            q.pop_front();
        }
        q.extend(samples);
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

impl Drop for VoiceEngine {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

fn select_input_device(host: &cpal::Host, wanted: Option<&str>) -> Option<cpal::Device> {
    if let Some(name) = wanted.filter(|s| !s.trim().is_empty() && !s.eq_ignore_ascii_case("Default"))
        && let Ok(mut devs) = host.input_devices()
        && let Some(found) = devs.find(|d| d.to_string() == name)
    {
        return Some(found);
    }
    host.default_input_device()
}

fn select_output_device(host: &cpal::Host, wanted: Option<&str>) -> Option<cpal::Device> {
    if let Some(name) = wanted.filter(|s| !s.trim().is_empty() && !s.eq_ignore_ascii_case("Default"))
        && let Ok(mut devs) = host.output_devices()
        && let Some(found) = devs.find(|d| d.to_string() == name)
    {
        return Some(found);
    }
    host.default_output_device()
}

fn build_capture_stream(
    dev: &cpal::Device,
    tx_frames: &mpsc::Sender<Vec<u8>>,
    ctrl: &Arc<VoiceControls>,
) -> Result<cpal::Stream, String> {
    let supported = dev
        .default_input_config()
        .map_err(|e| format!("default_input_config: {e}"))?;
    let in_rate = supported.sample_rate();
    let channels = supported.channels().max(1);
    let fmt = supported.sample_format();
    let cfg = StreamConfig {
        channels,
        sample_rate: in_rate,
        buffer_size: BufferSize::Default,
    };

    let acc = Arc::new(Mutex::new(Vec::<i16>::with_capacity(VOICE_FRAME_SAMPLES * 4)));
    let tx = tx_frames.clone();
    let ctrl_c = ctrl.clone();
    let process_f32 = move |mono_in: &[f32]| {
        let converted = downsample_to_16k_mono(mono_in, in_rate, channels);
        if converted.is_empty() {
            return;
        }
        let mut buf = acc.lock().unwrap();
        buf.extend_from_slice(&converted);
        while buf.len() >= VOICE_FRAME_SAMPLES {
            let chunk: Vec<i16> = buf.drain(..VOICE_FRAME_SAMPLES).collect();
            let rms = compute_rms_i16(&chunk);
            ctrl_c
                .mic_level_milli
                .store((rms * 1000.0).round() as u32, Ordering::Relaxed);
            let muted = ctrl_c.muted.load(Ordering::Relaxed);
            let ptt_ok = !ctrl_c.ptt_required.load(Ordering::Relaxed)
                || ctrl_c.ptt_active.load(Ordering::Relaxed);
            let gate = ctrl_c.noise_gate_milli.load(Ordering::Relaxed) as f32 / 1000.0;
            if !muted && ptt_ok && passes_noise_gate(rms, gate) {
                let _ = tx.try_send(encode_i16_le(&chunk));
            }
        }
    };

    match fmt {
        SampleFormat::F32 => {
            let p = process_f32;
            dev.build_input_stream(
                cfg,
                move |data: &[f32], _| p(data),
                |_| {},
                None,
            )
            .map_err(|e| format!("build_input_stream f32: {e}"))
        }
        SampleFormat::I16 => {
            let p = process_f32;
            dev.build_input_stream(
                cfg,
                move |data: &[i16], _| {
                    let f: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                    p(&f);
                },
                |_| {},
                None,
            )
            .map_err(|e| format!("build_input_stream i16: {e}"))
        }
        SampleFormat::U16 => {
            let p = process_f32;
            dev.build_input_stream(
                cfg,
                move |data: &[u16], _| {
                    let f: Vec<f32> = data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0).collect();
                    p(&f);
                },
                |_| {},
                None,
            )
            .map_err(|e| format!("build_input_stream u16: {e}"))
        }
        other => Err(format!("unsupported capture sample format {other:?}")),
    }
}

fn build_playback_stream(
    dev: &cpal::Device,
    pb_q: &Arc<Mutex<VecDeque<i16>>>,
    ctrl: &Arc<VoiceControls>,
) -> Result<cpal::Stream, String> {
    let supported = dev
        .default_output_config()
        .map_err(|e| format!("default_output_config: {e}"))?;
    let out_rate = supported.sample_rate();
    let channels = supported.channels().max(1);
    let fmt = supported.sample_format();
    let cfg = StreamConfig {
        channels,
        sample_rate: out_rate,
        buffer_size: BufferSize::Default,
    };

    let q_c = pb_q.clone();
    let ctrl_c = ctrl.clone();
    let fill_f32 = move |out_frames: usize| -> Vec<f32> {
        let need_src = ((out_frames as u64 * VOICE_SAMPLE_RATE as u64) / out_rate.max(1) as u64)
            .max(1) as usize;
        let mono_src: Vec<i16> = {
            let mut q = q_c.lock().unwrap();
            let take = need_src.min(q.len());
            q.drain(..take).collect()
        };
        if mono_src.is_empty() {
            return vec![0.0; out_frames * channels as usize];
        }
        let gain = ctrl_c.speaker_vol_milli.load(Ordering::Relaxed) as f32 / 1000.0;
        resample_mono_i16_to_device(&mono_src, out_frames, channels, gain)
    };

    match fmt {
        SampleFormat::F32 => {
            let f = fill_f32;
            dev.build_output_stream(
                cfg,
                move |out: &mut [f32], _| {
                    let frames = out.len() / channels as usize;
                    let src = f(frames);
                    let n = src.len().min(out.len());
                    out[..n].copy_from_slice(&src[..n]);
                },
                |_| {},
                None,
            )
            .map_err(|e| format!("build_output_stream f32: {e}"))
        }
        SampleFormat::I16 => {
            let f = fill_f32;
            dev.build_output_stream(
                cfg,
                move |out: &mut [i16], _| {
                    let frames = out.len() / channels as usize;
                    let src = f(frames);
                    for (dst, val) in out.iter_mut().zip(src) {
                        *dst = (val.clamp(-1.0, 1.0) * 32767.0) as i16;
                    }
                },
                |_| {},
                None,
            )
            .map_err(|e| format!("build_output_stream i16: {e}"))
        }
        SampleFormat::U16 => {
            let f = fill_f32;
            dev.build_output_stream(
                cfg,
                move |out: &mut [u16], _| {
                    let frames = out.len() / channels as usize;
                    let src = f(frames);
                    for (dst, val) in out.iter_mut().zip(src) {
                        *dst = ((val.clamp(-1.0, 1.0) * 32767.0) + 32768.0) as u16;
                    }
                },
                |_| {},
                None,
            )
            .map_err(|e| format!("build_output_stream u16: {e}"))
        }
        other => Err(format!("unsupported playback sample format {other:?}")),
    }
}

/// Mixes interleaved `f32` channels to mono and linearly resamples to 16 kHz `i16`.
pub fn downsample_to_16k_mono(input: &[f32], in_rate: u32, channels: u16) -> Vec<i16> {
    let ch = channels.max(1) as usize;
    let in_frames = input.len() / ch;
    if in_frames == 0 || in_rate == 0 {
        return Vec::new();
    }
    let mut mono = Vec::with_capacity(in_frames);
    for f in input.chunks_exact(ch) {
        let sum: f32 = f.iter().copied().sum();
        mono.push(sum / ch as f32);
    }
    if in_rate == VOICE_SAMPLE_RATE {
        return mono
            .into_iter()
            .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();
    }
    let out_len = ((in_frames as u64 * VOICE_SAMPLE_RATE as u64) / in_rate as u64).max(1) as usize;
    let mut out = Vec::with_capacity(out_len);
    let step = in_rate as f64 / VOICE_SAMPLE_RATE as f64;
    for i in 0..out_len {
        let pos = i as f64 * step;
        let idx = (pos.floor() as usize).min(mono.len() - 1);
        let next = (idx + 1).min(mono.len() - 1);
        let frac = (pos - idx as f64) as f32;
        let sample = mono[idx] + (mono[next] - mono[idx]) * frac;
        out.push((sample.clamp(-1.0, 1.0) * 32767.0) as i16);
    }
    out
}

/// Linearly resamples mono 16 kHz `i16` into `out_frames` interleaved `channels` `f32` samples.
pub fn resample_mono_i16_to_device(
    mono_16k: &[i16],
    out_frames: usize,
    channels: u16,
    gain: f32,
) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    let mut out = vec![0.0; out_frames * ch];
    if mono_16k.is_empty() || out_frames == 0 {
        return out;
    }
    let last = mono_16k.len() - 1;
    for i in 0..out_frames {
        let pos = if out_frames <= 1 {
            0.0
        } else {
            (i as f64 * last as f64) / ((out_frames - 1) as f64)
        };
        let idx = (pos.floor() as usize).min(last);
        let next = (idx + 1).min(last);
        let frac = (pos - idx as f64) as f32;
        let s0 = mono_16k[idx] as f32 / 32768.0;
        let s1 = mono_16k[next] as f32 / 32768.0;
        let val = ((s0 + (s1 - s0) * frac) * gain).clamp(-1.0, 1.0);
        for c in 0..ch {
            out[i * ch + c] = val;
        }
    }
    out
}

pub fn compute_rms_i16(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples
        .iter()
        .map(|&s| {
            let f = s as f64 / 32768.0;
            f * f
        })
        .sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

pub fn passes_noise_gate(rms: f32, threshold: f32) -> bool {
    threshold <= 0.0 || rms >= threshold
}

pub fn encode_i16_le(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

pub fn decode_i16_le(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_resampling_gate_and_pcm_roundtrip() {
        // 48 kHz stereo -> 16 kHz mono preserves length ratio ~1:3
        let stereo_48k = vec![0.5f32; 480 * 2];
        let mono_16k = downsample_to_16k_mono(&stereo_48k, 48_000, 2);
        assert_eq!(mono_16k.len(), 160);
        assert!(mono_16k.iter().all(|&s| (s - 16383).abs() <= 4));

        let rms = compute_rms_i16(&mono_16k);
        assert!(rms > 0.45 && passes_noise_gate(rms, 0.05));
        assert!(!passes_noise_gate(0.002, 0.01));

        let bytes = encode_i16_le(&mono_16k);
        assert_eq!(decode_i16_le(&bytes), mono_16k);

        let up = resample_mono_i16_to_device(&mono_16k, 480, 2, 0.5);
        assert_eq!(up.len(), 480 * 2);
        assert!(up.iter().all(|&s| (s - 0.25).abs() < 0.02));
    }
}
