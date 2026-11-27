use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::StreamTrait;

use crate::audio_codec::{AudioDecoder, AudioEncoder};
use crate::audio_io::{PlaybackBuffer, playback_stream};
use crate::voice::select_output_device;

#[derive(Debug)]
pub struct SystemAudioControls {
    pub muted: AtomicBool,
    pub volume: AtomicU32,
}

impl Default for SystemAudioControls {
    fn default() -> Self {
        Self {
            muted: AtomicBool::new(false),
            volume: AtomicU32::new(100),
        }
    }
}

pub struct SystemAudioPlayer {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<PlaybackBuffer>>,
    decoder: AudioDecoder,
}

impl SystemAudioPlayer {
    pub fn start(device: Option<&str>, controls: Arc<SystemAudioControls>) -> Result<Self, String> {
        Self::start_track(device, controls, false, 2)
    }

    pub fn start_track(
        device: Option<&str>, controls: Arc<SystemAudioControls>, mono: bool, double_speed: u32,
    ) -> Result<Self, String> {
        let output = select_output_device(&cpal::default_host(), device)
            .ok_or("No speaker is available.")?;
        let channels = if mono { opus::Channels::Mono } else { opus::Channels::Stereo };
        let buffer = Arc::new(Mutex::new(PlaybackBuffer::new(channels as usize)));
        let playback = buffer.clone();
        let decoder = AudioDecoder::new(channels).map_err(|e| e.to_string())?;
        let stream = playback_stream(&output, move |samples, channels, rate| {
            let gain = if controls.muted.load(Ordering::Relaxed) {
                0.0
            } else {
                controls.volume.load(Ordering::Relaxed).min(100) as f32 / 100.0
            };
            playback
                .lock()
                .unwrap()
                .render(samples, channels, (rate * 2 / double_speed.clamp(1, 8)).max(1), gain);
        })?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(Self {
            _stream: stream,
            buffer,
            decoder,
        })
    }

    pub fn push(&mut self, packet: &[u8]) {
        if let Some(samples) = self.decoder.decode(packet) {
            self.buffer.lock().unwrap().push(samples);
        }
    }
}

pub struct SystemAudioCapture {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl SystemAudioCapture {
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn start(
        mut on_packet: impl FnMut(Vec<u8>) + Send + 'static,
        on_error: impl FnOnce(String) + Send + 'static,
    ) -> Result<Self, String> {
        let mut encoder =
            AudioEncoder::new(opus::Channels::Stereo, 64).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let (ready, init) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let (chunks, frames) = mpsc::sync_channel::<Vec<i16>>(3);
            let (failed, errors) = mpsc::sync_channel(1);
            let _stream = match open_loopback(chunks, failed) {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            if ready.send(Ok(())).is_err() {
                return;
            }
            while !stopping.load(Ordering::Relaxed) {
                if let Ok(error) = errors.try_recv() {
                    on_error(error);
                    break;
                }
                match frames.recv_timeout(Duration::from_millis(20)) {
                    Ok(samples) => {
                        if let Ok(packet) = encoder.encode(&samples) {
                            on_packet(packet);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        match init.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                worker: Some(worker),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("System audio capture did not respond.".into())
            }
        }
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
fn open_loopback(
    chunks: mpsc::SyncSender<Vec<i16>>,
    failed: mpsc::SyncSender<String>,
) -> Result<cpal::Stream, String> {
    use cpal::traits::{DeviceTrait, HostTrait};

    use crate::audio_codec::AUDIO_SAMPLES;
    use crate::audio_io::{CaptureResampler, capture_stream, to_pcm};
    #[cfg(target_os = "linux")]
    let host = cpal::host_from_id(cpal::HostId::PipeWire)
        .map_err(|e| format!("The desktop audio service is unavailable: {e}"))?;
    #[cfg(not(target_os = "linux"))]
    let host = cpal::default_host();

    let device = host
        .default_output_device()
        .ok_or("No desktop playback device is available.")?;
    // The backend uses physical input on a duplex device. Keep that source private.
    #[cfg(target_os = "macos")]
    if device.supports_input() {
        return Err("Select an output-only playback device to forward system audio.".into());
    }
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let mut resampler = CaptureResampler::default();
    let mut pcm = Vec::with_capacity(AUDIO_SAMPLES * 2);
    // Request input from the output device to select the OS playback tap.
    let stream = capture_stream(
        &device,
        config,
        move |samples, channels, rate| {
            for frame in samples.chunks_exact(channels) {
                let stereo = [frame[0], frame[usize::from(channels > 1)]];
                resampler.push(stereo, rate, |sample| {
                    pcm.extend(sample.map(to_pcm));
                    if pcm.len() == AUDIO_SAMPLES * 2 {
                        let _ = chunks.try_send(std::mem::replace(
                            &mut pcm,
                            Vec::with_capacity(AUDIO_SAMPLES * 2),
                        ));
                    }
                });
            }
        },
        move |error| {
            if error.kind() != cpal::ErrorKind::Xrun {
                let _ = failed.try_send(error.to_string());
            }
        },
    )?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn open_loopback(
    _: mpsc::SyncSender<Vec<i16>>,
    _: mpsc::SyncSender<String>,
) -> Result<cpal::Stream, String> {
    Err("System audio forwarding is unavailable on this host.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_codec::{AUDIO_SAMPLES, MAX_AUDIO_PACKET};
    use crate::{ChannelId, CommsMsg, FLAG_NONE, pack_message, unpack_message};

    #[test]
    fn system_audio_framing_bounds() {
        let mut encoder = AudioEncoder::new(opus::Channels::Stereo, 64).unwrap();
        let packet = encoder.encode(&[0; AUDIO_SAMPLES * 2]).unwrap();
        let frame = pack_message(
            ChannelId::Comms,
            FLAG_NONE,
            &CommsMsg::SystemAudioFrame(packet.clone()),
            None,
        )
        .unwrap();
        assert!(
            matches!(unpack_message::<CommsMsg>(&frame).unwrap(), CommsMsg::SystemAudioFrame(bytes) if bytes == packet)
        );
        for size in [0, 4, MAX_AUDIO_PACKET + 1] {
            let frame = pack_message(
                ChannelId::Comms,
                FLAG_NONE,
                &CommsMsg::SystemAudioFrame(vec![0; size]),
                None,
            )
            .unwrap();
            assert!(unpack_message::<CommsMsg>(&frame).is_err());
        }
    }
}
