use serde::{Deserialize, Serialize};

use crate::{
    DirtyTile, H264VideoDecoder, H264VideoEncoder, MoveRect, ProtoError, TileRect, VideoMsg,
    apply_tile_batch_rgba, encode_dirty_tile,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    ZstdTiles,
    H264Sw,
    H264Hw,
    HevcHw,
    Av1Hw,
}

impl VideoCodec {
    pub const HARDWARE: [Self; 3] = [Self::Av1Hw, Self::HevcHw, Self::H264Hw];

    pub const fn label(self) -> &'static str {
        match self {
            Self::ZstdTiles => "Zstd 4:4:4",
            Self::H264Sw => "H.264 SW",
            Self::H264Hw => "H.264 HW",
            Self::HevcHw => "HEVC HW",
            Self::Av1Hw => "AV1 HW",
        }
    }

    const fn hardware_bit(self) -> u8 {
        match self {
            Self::ZstdTiles | Self::H264Sw => 0,
            Self::H264Hw => 1,
            Self::HevcHw => 2,
            Self::Av1Hw => 4,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoCapabilities(u8);

impl VideoCapabilities {
    pub fn add(&mut self, codec: VideoCodec) {
        self.0 |= codec.hardware_bit();
    }

    pub fn remove(&mut self, codec: VideoCodec) {
        self.0 &= !codec.hardware_bit();
    }

    pub fn includes(self, codec: VideoCodec) -> bool {
        let bit = codec.hardware_bit();
        bit == 0 || self.0 & bit != 0
    }

    pub fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

#[derive(Clone, Copy)]
pub struct VideoInput<'a> {
    pub bgra: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub monitor: u8,
    pub sequence: u64,
    pub timestamp_us: u64,
    pub bitrate: u32,
    pub fps: u8,
    pub refresh: bool,
    pub dirty_rects: &'a [(i32, i32, i32, i32)],
    pub moves: &'a [MoveRect],
}

impl VideoInput<'_> {
    pub fn packet(self, codec: VideoCodec, keyframe: bool, bytes: Vec<u8>) -> VideoMsg {
        if codec == VideoCodec::H264Sw {
            return VideoMsg::H264Frame {
                monitor_id: self.monitor,
                frame_seq: self.sequence,
                timestamp_us: self.timestamp_us,
                width: self.width as u16,
                height: self.height as u16,
                is_keyframe: keyframe,
                annex_b: bytes,
            };
        }
        VideoMsg::EncodedFrame {
            codec,
            monitor_id: self.monitor,
            frame_seq: self.sequence,
            timestamp_us: self.timestamp_us,
            width: self.width as u16,
            height: self.height as u16,
            is_keyframe: keyframe,
            bytes,
        }
    }
}

pub trait VideoEncoder {
    fn codec(&self) -> VideoCodec;
    fn encode(&mut self, input: VideoInput<'_>) -> Result<Option<VideoMsg>, ProtoError>;
}

pub trait VideoDecoder {
    fn decode(
        &mut self,
        msg: &VideoMsg,
        rgba: &mut Vec<u8>,
        width: &mut u32,
        height: &mut u32,
    ) -> Result<bool, ProtoError>;
}

pub struct ZstdVideo;

impl VideoEncoder for ZstdVideo {
    fn codec(&self) -> VideoCodec {
        VideoCodec::ZstdTiles
    }

    fn encode(&mut self, input: VideoInput<'_>) -> Result<Option<VideoMsg>, ProtoError> {
        let entire_frame = [(0, 0, input.width as i32, input.height as i32)];
        let dirty_rects = if input.refresh || input.dirty_rects.is_empty() && input.moves.is_empty()
        {
            &entire_frame
        } else {
            input.dirty_rects
        };
        Ok(Some(VideoMsg::TileBatch {
            monitor_id: input.monitor,
            frame_seq: input.sequence,
            timestamp_us: input.timestamp_us,
            width: input.width as u16,
            height: input.height as u16,
            is_full_refresh: input.refresh,
            move_rects: if input.refresh {
                Vec::new()
            } else {
                input.moves.to_vec()
            },
            tiles: dirty_rects_to_tiles(input.bgra, input.width, input.height, dirty_rects),
        }))
    }
}

impl VideoDecoder for ZstdVideo {
    fn decode(
        &mut self,
        msg: &VideoMsg,
        rgba: &mut Vec<u8>,
        width: &mut u32,
        height: &mut u32,
    ) -> Result<bool, ProtoError> {
        apply_tile_batch_rgba(msg, rgba, width, height)?;
        Ok(true)
    }
}

impl VideoEncoder for H264VideoEncoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264Sw
    }

    fn encode(&mut self, input: VideoInput<'_>) -> Result<Option<VideoMsg>, ProtoError> {
        self.reconfigure(
            input.width,
            input.height,
            input.bitrate,
            f32::from(input.fps),
        )?;
        if input.refresh {
            self.force_keyframe();
        }
        let (bytes, keyframe) = self.encode_bgra(input.bgra, input.width, input.height)?;
        Ok((!bytes.is_empty()).then(|| input.packet(self.codec(), keyframe, bytes)))
    }
}

impl VideoDecoder for H264VideoDecoder {
    fn decode(
        &mut self,
        msg: &VideoMsg,
        rgba: &mut Vec<u8>,
        width: &mut u32,
        height: &mut u32,
    ) -> Result<bool, ProtoError> {
        let (payload, w, h) = match msg {
            VideoMsg::H264Frame {
                annex_b,
                width,
                height,
                ..
            }
            | VideoMsg::EncodedFrame {
                codec: VideoCodec::H264Hw,
                bytes: annex_b,
                width,
                height,
                ..
            } => (annex_b, *width, *height),
            _ => return Err(ProtoError::Codec("Expected an H.264 frame".into())),
        };
        self.decode_to_rgba(payload, u32::from(w), u32::from(h), rgba, width, height)
    }
}

pub fn preferred_motion(
    viewers: VideoCapabilities,
    mut try_hardware: impl FnMut(VideoCodec) -> bool,
) -> VideoCodec {
    VideoCodec::HARDWARE
        .into_iter()
        .find(|&codec| viewers.includes(codec) && try_hardware(codec))
        .unwrap_or(VideoCodec::H264Sw)
}

pub fn platform_video_decoders() -> VideoCapabilities {
    #[cfg(target_os = "windows")]
    return crate::mf_video::decoders();

    #[cfg(target_os = "macos")]
    return crate::vt_video::decoders();

    #[cfg(target_os = "linux")]
    return crate::va_video::decoders();

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    VideoCapabilities::default()
}

pub fn platform_video_encoder(
    codec: VideoCodec,
    input: VideoInput<'_>,
) -> Option<Box<dyn VideoEncoder>> {
    #[cfg(target_os = "windows")]
    if VideoCodec::HARDWARE.contains(&codec) {
        return crate::mf_video::Encoder::open(codec, input)
            .ok()
            .map(|encoder| Box::new(encoder) as _);
    }
    #[cfg(target_os = "macos")]
    if matches!(codec, VideoCodec::H264Hw | VideoCodec::HevcHw) {
        return crate::vt_video::Encoder::open(codec, input)
            .ok()
            .map(|encoder| Box::new(encoder) as _);
    }
    #[cfg(target_os = "linux")]
    if VideoCodec::HARDWARE.contains(&codec) {
        return crate::va_video::Encoder::open(codec, input)
            .ok().map(|encoder| Box::new(encoder) as _);
    }
    let _ = (codec, input);
    None
}

pub fn platform_video_decoder(
    codec: VideoCodec,
    width: u16,
    height: u16,
) -> Option<Box<dyn VideoDecoder>> {
    #[cfg(target_os = "windows")]
    if VideoCodec::HARDWARE.contains(&codec) {
        return crate::mf_video::Decoder::open(codec, u32::from(width), u32::from(height))
            .ok()
            .map(|decoder| Box::new(decoder) as _);
    }
    #[cfg(target_os = "macos")]
    if matches!(codec, VideoCodec::H264Hw | VideoCodec::HevcHw) {
        return crate::vt_video::Decoder::open(codec, width, height)
            .ok()
            .map(|decoder| Box::new(decoder) as _);
    }
    #[cfg(target_os = "linux")]
    if VideoCodec::HARDWARE.contains(&codec) {
        return crate::va_video::Decoder::open(codec, width, height)
            .ok().map(|decoder| Box::new(decoder) as _);
    }
    let _ = (codec, width, height);
    None
}

/// Coalesces dirty rectangles and slices oversized rects into Zstd level-3 `DirtyTile`s.
pub fn dirty_rects_to_tiles(
    bgra: &[u8],
    w: u32,
    h: u32,
    dirty_rects: &[(i32, i32, i32, i32)],
) -> Vec<DirtyTile> {
    let mut clamped: Vec<(u16, u16, u16, u16)> = dirty_rects
        .iter()
        .filter_map(|&(l, t, r, b)| {
            let x0 = l.clamp(0, w as i32) as u16;
            let y0 = t.clamp(0, h as i32) as u16;
            let x1 = r.clamp(0, w as i32) as u16;
            let y1 = b.clamp(0, h as i32) as u16;
            (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
        })
        .collect();

    let mut merged: Vec<(u16, u16, u16, u16)> = Vec::with_capacity(clamped.len());
    for rect in clamped.drain(..) {
        let mut absorbed = false;
        for existing in &mut merged {
            let ux0 = existing.0.min(rect.0);
            let uy0 = existing.1.min(rect.1);
            let ux1 = existing.2.max(rect.2);
            let uy1 = existing.3.max(rect.3);
            let union_area = ((ux1 - ux0) as u64) * ((uy1 - uy0) as u64);
            let sum_area = ((existing.2 - existing.0) as u64) * ((existing.3 - existing.1) as u64)
                + ((rect.2 - rect.0) as u64) * ((rect.3 - rect.1) as u64);
            if union_area * 4 <= sum_area * 5 {
                *existing = (ux0, uy0, ux1, uy1);
                absorbed = true;
                break;
            }
        }
        if !absorbed {
            merged.push(rect);
        }
    }

    let mut tiles = Vec::new();
    const MAX_CHUNK: u16 = 512;
    for (x0, y0, x1, y1) in merged {
        let mut y = y0;
        while y < y1 {
            let th = (y1 - y).min(MAX_CHUNK);
            let mut x = x0;
            while x < x1 {
                let tw = (x1 - x).min(MAX_CHUNK);
                if let Ok(tile) = encode_dirty_tile(
                    bgra,
                    w,
                    h,
                    TileRect {
                        x,
                        y,
                        width: tw,
                        height: th,
                    },
                    3,
                ) {
                    tiles.push(tile);
                }
                x += tw;
            }
            y += th;
        }
    }
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_only_shared_codecs_and_falls_back_when_creation_fails() {
        let mut first = VideoCapabilities::default();
        first.add(VideoCodec::Av1Hw);
        first.add(VideoCodec::H264Hw);
        let mut second = VideoCapabilities::default();
        second.add(VideoCodec::HevcHw);
        second.add(VideoCodec::H264Hw);
        let shared = first.intersect(second);
        assert_eq!(preferred_motion(shared, |_| true), VideoCodec::H264Hw);
        assert_eq!(preferred_motion(shared, |_| false), VideoCodec::H264Sw);
        assert_eq!(
            preferred_motion(first, |codec| codec != VideoCodec::Av1Hw),
            VideoCodec::H264Hw
        );
        assert!(shared.includes(VideoCodec::ZstdTiles));
    }
}
