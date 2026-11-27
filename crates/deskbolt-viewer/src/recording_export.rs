use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use deskbolt_proto::H264VideoEncoder;
use deskbolt_proto::video_pipeline::platform_video_decoders;
use muxide::api::AudioCodec;
use muxide::codec::{av1::ObuIter, h264::{annexb_to_avcc, extract_avc_config}};
use muxide::fragmented::{FragmentAudioConfig, FragmentConfig, FragmentedMuxer};
use rav1e::prelude::*;

use crate::recording::{Recording, Track, new_private_file};
use crate::replay::decode_record;
use crate::session::MonitorDecoder;

const AUDIO_RATE: u32 = 48_000;
const AUDIO_SAMPLES: usize = 960;
const VIDEO_RATE: u32 = 25;
const FRAME_MS: u64 = 40;

#[derive(Clone, Copy)]
pub(crate) enum Format {
    Mp4,
    Webm,
}

pub(crate) fn export(
    source: &Path, target: &Path, display: u8, format: Format, cancelled: &AtomicBool,
) -> io::Result<()> {
    let file = new_private_file(target)?;
    let result = export_into(source, BufWriter::new(file), display, format, cancelled);
    if result.is_err() {
        let _ = std::fs::remove_file(target);
    }
    result
}

fn export_into(
    source: &Path, file: BufWriter<File>, display: u8, format: Format, cancelled: &AtomicBool,
) -> io::Result<()> {
    let mut recording = Recording::open(source)?;
    let start = recording.seek(display, 0)?;
    let mut pending = recording.next()?;
    let mut capabilities = platform_video_decoders();
    let mut decoder = MonitorDecoder::default();
    while decoder.rgba.is_empty() {
        if cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::other("Export cancelled."));
        }
        let record = pending.take().ok_or_else(|| io::Error::other(
            "This device could not decode the recorded display.",
        ))?;
        decode_record(record, display, &mut decoder, &mut capabilities)?;
        pending = recording.next()?;
    }
    let width = decoder.width.next_multiple_of(2).max(16);
    let height = decoder.height.next_multiple_of(2).max(16);
    if u64::from(width) * u64::from(height) > 16_777_216 {
        return Err(io::Error::other("The recorded display is too large to export."));
    }
    let mut audio = MixedAudio::new()?;
    let mut output = match format {
        Format::Mp4 => Output::Mp4(Box::new(Mp4::new(file, width, height)?)),
        Format::Webm => Output::Webm(Box::new(Webm::new(file, width, height, audio.pre_skip)?)),
    };
    let mut pixels = Vec::new();
    let duration = recording.duration_ms.saturating_sub(start).max(FRAME_MS);
    let duration = duration.checked_next_multiple_of(20).ok_or_else(|| io::Error::other("Invalid recording duration."))?;
    for ms in (0..=duration).step_by(20) {
        if cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::other("Export cancelled."));
        }
        while pending.as_ref().is_some_and(|record| record.ms <= start.saturating_add(ms)) {
            let record = pending.take().unwrap();
            if record.track == Track::Video {
                decode_record(record, display, &mut decoder, &mut capabilities)?;
            } else {
                audio.add(record.track, &record.data, record.audio_reset());
            }
            pending = recording.next()?;
        }
        if ms % FRAME_MS == 0 {
            resize_pixels(&decoder, width, height, &mut pixels);
            output.video(ms, &pixels)?;
        }
        output.audio(ms, audio.packet()?)?;
    }
    output.finish()
}

fn resize_pixels(source: &MonitorDecoder, width: u32, height: u32, pixels: &mut Vec<u8>) {
    pixels.resize(width as usize * height as usize * 4, 0);
    for y in 0..height {
        for x in 0..width {
            let sx = u64::from(x) * u64::from(source.width) / u64::from(width);
            let sy = u64::from(y) * u64::from(source.height) / u64::from(height);
            let i = (sy * u64::from(source.width) + sx) as usize * 4;
            let j = (y as usize * width as usize + x as usize) * 4;
            let [r, g, b, _] = source.rgba[i..i + 4].try_into().unwrap();
            pixels[j..j + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
}

struct MixedAudio {
    decoder: [opus::Decoder; 3],
    encoder: opus::Encoder,
    pre_skip: u16,
    queues: [VecDeque<Vec<i16>>; 3],
}

impl MixedAudio {
    fn new() -> io::Result<Self> {
        use opus::Channels::{Mono, Stereo};
        let mut encoder = opus::Encoder::new(AUDIO_RATE, Stereo, opus::Application::Audio).map_err(io::Error::other)?;
        encoder.set_bitrate(opus::Bitrate::Bits(64_000)).map_err(io::Error::other)?;
        let pre_skip = encoder.get_lookahead().map_err(io::Error::other)? as u16;
        Ok(Self {
            encoder, pre_skip,
            decoder: [Mono, Mono, Stereo].map(|ch| opus::Decoder::new(AUDIO_RATE, ch).expect("valid Opus format")),
            queues: std::array::from_fn(|_| VecDeque::new()),
        })
    }

    fn add(&mut self, track: Track, data: &[u8], reset: bool) {
        let index = track as usize - 1;
        if reset {
            let _ = self.decoder[index].reset_state();
            self.queues[index].clear();
            return;
        }
        let Some(packet) = data.get(4..) else { return };
        if opus::packet::get_nb_samples(packet, AUDIO_RATE).ok() != Some(AUDIO_SAMPLES) {
            return;
        }
        let channels = if track == Track::SystemAudio { 2 } else { 1 };
        let mut pcm = vec![0i16; AUDIO_SAMPLES * channels];
        if self.decoder[index].decode(packet, &mut pcm, false).ok() == Some(AUDIO_SAMPLES) {
            let queue = &mut self.queues[index];
            if queue.len() >= 10 {
                queue.pop_front();
            }
            queue.push_back(pcm);
        }
    }

    fn packet(&mut self) -> io::Result<Vec<u8>> {
        let tracks = self.queues.each_mut().map(VecDeque::pop_front);
        let pcm: [i16; AUDIO_SAMPLES * 2] = std::array::from_fn(|sample| {
            let sum: i32 = tracks.iter().flatten().map(|track| {
                i32::from(track[if track.len() == AUDIO_SAMPLES { sample / 2 } else { sample }])
            }).sum();
            sum.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
        });
        let mut data = vec![0; 1275];
        let n = self.encoder.encode(&pcm, &mut data).map_err(io::Error::other)?;
        data.truncate(n);
        Ok(data)
    }
}

enum Output {
    Mp4(Box<Mp4>),
    Webm(Box<Webm>),
}

impl Output {
    fn video(&mut self, ms: u64, bgra: &[u8]) -> io::Result<()> {
        match self {
            Self::Mp4(mp4) => mp4.video(ms, bgra),
            Self::Webm(webm) => webm.video(bgra),
        }
    }

    fn audio(&mut self, ms: u64, data: Vec<u8>) -> io::Result<()> {
        match self {
            Self::Mp4(mp4) => mp4.audio(ms, &data),
            Self::Webm(webm) => { webm.audio.push_back((ms, data)); Ok(()) }
        }
    }

    fn finish(self) -> io::Result<()> {
        match self {
            Self::Mp4(mp4) => mp4.finish(),
            Self::Webm(webm) => webm.finish(),
        }
    }
}

struct Mp4 {
    file: BufWriter<File>,
    encoder: H264VideoEncoder,
    mux: Option<FragmentedMuxer>,
    size: (u32, u32),
}

impl Mp4 {
    fn new(file: BufWriter<File>, width: u32, height: u32) -> io::Result<Self> {
        let encoder = H264VideoEncoder::new(width, height, 4_000_000, VIDEO_RATE as f32).map_err(io::Error::other)?;
        Ok(Self { file, encoder, mux: None, size: (width, height) })
    }

    fn video(&mut self, ms: u64, bgra: &[u8]) -> io::Result<()> {
        let (width, height) = self.size;
        if ms.is_multiple_of(2_000) {
            self.encoder.force_keyframe();
        }
        let (data, key) = self.encoder.encode_bgra(bgra, width, height).map_err(io::Error::other)?;
        if data.is_empty() {
            return Err(io::Error::other("The video encoder skipped a frame."));
        }
        if self.mux.is_none() {
            let avc = extract_avc_config(&data).ok_or_else(|| io::Error::other("Invalid H.264 header."))?;
            let mut config = FragmentConfig::new(width, height, 1_000);
            config.sps = avc.sps;
            config.pps = avc.pps;
            let audio = FragmentAudioConfig { codec: AudioCodec::Opus, sample_rate: AUDIO_RATE, channels: 2 };
            let mut mux = FragmentedMuxer::new_with_audio(config, audio).map_err(io::Error::other)?;
            self.file.write_all(&mux.init_segment())?;
            self.mux = Some(mux);
        }
        let mux = self.mux.as_mut().unwrap();
        if key && ms != 0 && let Some(fragment) = mux.flush_segment() {
            self.file.write_all(&fragment)?;
        }
        mux.write_video(ms, ms, &annexb_to_avcc(&data), key).map_err(io::Error::other)
    }

    fn audio(&mut self, ms: u64, data: &[u8]) -> io::Result<()> {
        let pts = ms * u64::from(AUDIO_RATE / 1_000);
        self.mux.as_mut().unwrap().write_audio(pts, pts, data).map_err(io::Error::other)
    }

    fn finish(mut self) -> io::Result<()> {
        if let Some(fragment) = self.mux.as_mut().and_then(FragmentedMuxer::flush_segment) {
            self.file.write_all(&fragment)?;
        }
        self.file.flush()?;
        self.file.get_ref().sync_all()
    }
}

type WebmWriter = webm_muxer::Muxer<BufWriter<File>>;

struct Webm {
    file: Option<BufWriter<File>>,
    mux: Option<(WebmWriter, webm_muxer::TrackId, webm_muxer::TrackId)>,
    encoder: rav1e::Context<u8>,
    size: (u32, u32),
    pre_skip: u16,
    audio: VecDeque<(u64, Vec<u8>)>,
}

impl Webm {
    fn new(file: BufWriter<File>, width: u32, height: u32, pre_skip: u16) -> io::Result<Self> {
        let mut config = EncoderConfig::with_speed_preset(10);
        config.width = width as usize;
        config.height = height as usize;
        config.time_base = Rational::new(1, u64::from(VIDEO_RATE));
        config.low_latency = true;
        config.min_key_frame_interval = 1;
        config.max_key_frame_interval = u64::from(VIDEO_RATE * 2);
        config.speed_settings.rdo_lookahead_frames = 1;
        let encoder = Config::new().with_encoder_config(config).with_threads(4).new_context().map_err(io::Error::other)?;
        Ok(Self { file: Some(file), mux: None, encoder, size: (width, height), pre_skip, audio: VecDeque::new() })
    }

    fn video(&mut self, bgra: &[u8]) -> io::Result<()> {
        let (width, height) = (self.size.0 as usize, self.size.1 as usize);
        let mut frame = self.encoder.new_frame();
        let mut y = vec![0; width * height];
        let mut u = vec![0; y.len() / 4];
        let mut v = vec![0; u.len()];
        for row in (0..height).step_by(2) {
            for col in (0..width).step_by(2) {
                let mut chroma = (0, 0);
                for index in [row * width + col, row * width + col + 1, (row + 1) * width + col, (row + 1) * width + col + 1] {
                    let [b, g, r, _] = bgra[index * 4..index * 4 + 4].try_into().unwrap();
                    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
                    y[index] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                    chroma.0 += -38 * r - 74 * g + 112 * b;
                    chroma.1 += 112 * r - 94 * g - 18 * b;
                }
                let index = (row / 2) * (width / 2) + col / 2;
                u[index] = (((chroma.0 + 512) >> 10) + 128) as u8;
                v[index] = (((chroma.1 + 512) >> 10) + 128) as u8;
            }
        }
        for (plane, data, stride) in [(0, &y, width), (1, &u, width / 2), (2, &v, width / 2)] {
            frame.planes[plane].copy_from_raw_u8(data, stride, 1);
        }
        self.encoder.send_frame(frame).map_err(io::Error::other)?;
        self.drain()
    }

    fn drain(&mut self) -> io::Result<()> {
        loop {
            let packet = match self.encoder.receive_packet() {
                Ok(packet) => packet,
                Err(EncoderStatus::Encoded) => continue,
                Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => return Ok(()),
                Err(error) => return Err(io::Error::other(error)),
            };
            if let Some(file) = self.file.take() {
                let mut config = self.encoder.container_sequence_header();
                let header = ObuIter::new(&packet.data).find(|(info, _)| info.obu_type == 1)
                    .ok_or_else(|| io::Error::other("Invalid AV1 sequence header."))?;
                config.extend_from_slice(header.1);
                let mut mux = WebmWriter::new_seekable(file, webm_muxer::Options::new()).map_err(io::Error::other)?;
                let video = mux.add_video_track(webm_muxer::VideoTrack::new(webm_muxer::VideoCodec::Av1, self.size.0, self.size.1)
                    .with_codec_private(config)).map_err(io::Error::other)?;
                let audio = mux.add_audio_track(webm_muxer::AudioTrack::opus(2, self.pre_skip)).map_err(io::Error::other)?;
                self.mux = Some((mux, video, audio));
            }
            let ms = packet.input_frameno * FRAME_MS;
            self.write_audio_before(ms)?;
            let (mux, video, _) = self.mux.as_mut().unwrap();
            let bytes: Vec<u8> = ObuIter::new(&packet.data).filter(|(info, _)| info.obu_type != 2)
                .flat_map(|(_, bytes)| bytes.iter().copied()).collect();
            mux.write_frame(*video, &bytes, ms * 1_000_000, packet.frame_type == FrameType::KEY)
                .map_err(io::Error::other)?;
        }
    }

    fn write_audio_before(&mut self, end: u64) -> io::Result<()> {
        let (mux, _, track) = self.mux.as_mut().ok_or_else(|| io::Error::other("No video was encoded."))?;
        while self.audio.front().is_some_and(|(ms, _)| *ms < end) {
            let (ms, data) = self.audio.pop_front().unwrap();
            mux.write_frame(*track, &data, ms * 1_000_000, true).map_err(io::Error::other)?;
        }
        Ok(())
    }

    fn finish(mut self) -> io::Result<()> {
        self.encoder.flush();
        self.drain()?;
        self.write_audio_before(u64::MAX)?;
        let (mux, _, _) = self.mux.take().unwrap();
        let mut file = mux.finish().map_err(io::Error::other)?;
        file.flush()?;
        file.get_ref().sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::Recorder;
    use deskbolt_proto::{ChannelId, FLAG_KEYFRAME, TileRect, VideoMsg, encode_dirty_tile, pack_message};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn exports_real_avc_and_av1_with_opus_and_keeps_the_original_recording() {
        let name = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let input = std::env::temp_dir().join(format!("deskbolt-export-{}-{name}.dbrec", std::process::id()));
        let (finish, ready) = std::sync::mpsc::channel();
        let recorder = Recorder::start(&input, move |result| { let _ = finish.send(result); }).unwrap();
        let tile = encode_dirty_tile(&[190, 200, 210, 255].repeat(64 * 64), 64, 64,
            TileRect { x: 0, y: 0, width: 64, height: 64 }, 3).unwrap();
        let frame = pack_message(ChannelId::VideoTiles, FLAG_KEYFRAME, &VideoMsg::TileBatch {
            monitor_id: 0, frame_seq: 0, timestamp_us: 0, width: 64, height: 64,
            is_full_refresh: true, move_rects: Vec::new(), tiles: vec![tile],
        }, None).unwrap();
        assert!(recorder.queue(Track::Video, 0, frame.flags, &frame.payload, true));
        let mut speech = opus::Encoder::new(AUDIO_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let pcm: [i16; AUDIO_SAMPLES] = std::array::from_fn(|i| ((i as f32 / 12.0).sin() * 6_000.0) as i16);
        let mut packet = vec![0; 1279];
        let bytes = speech.encode(&pcm, &mut packet[4..]).unwrap();
        packet.truncate(bytes + 4);
        assert!(recorder.queue(Track::RemoteVoice, 0, crate::recording::AUDIO_RESET, &[], false));
        assert!(recorder.queue(Track::RemoteVoice, 0, 0, &packet, false));
        drop(recorder);
        ready.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        for (ext, format, magic, video) in [
            ("mp4", Format::Mp4, b"ftyp" as &[u8], b"avc1" as &[u8]),
            ("webm", Format::Webm, &[0x1a, 0x45, 0xdf, 0xa3], b"V_AV1"),
        ] {
            let output = input.with_extension(ext);
            export(&input, &output, 0, format, &AtomicBool::new(false)).unwrap();
            let file = std::fs::read(&output).unwrap();
            assert!(file[..8].windows(magic.len()).any(|part| part == magic));
            assert!(file.windows(video.len()).any(|part| part == video));
            let audio = if ext == "mp4" { b"Opus" as &[u8] } else { b"A_OPUS" };
            assert!(file.windows(audio.len()).any(|part| part == audio));
            assert!(export(&input, &output, 0, format, &AtomicBool::new(false)).is_err());
            assert_eq!(std::fs::metadata(&output).unwrap().len(), file.len() as u64);
            std::fs::remove_file(output).unwrap();
        }
        std::fs::remove_file(input).unwrap();
    }
}
