use std::io::Read;

use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, RateControlMode};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

use crate::error::ProtoError;
use crate::messages::{DirtyTile, MoveRect, VideoMsg};

const MAX_TILE_DECOMPRESS: usize = 32 * 1024 * 1024;

/// Pixel rectangle coordinates within a captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Converts packed BGRA8 bytes into RGBA8 in-place or into `dst`.
pub fn bgra_to_rgba_into(bgra: &[u8], rgba: &mut [u8]) -> Result<(), ProtoError> {
    if !bgra.len().is_multiple_of(4) || bgra.len() != rgba.len() {
        return Err(ProtoError::Codec(format!(
            "BGRA->RGBA buffer mismatch: src={}, dst={}",
            bgra.len(),
            rgba.len()
        )));
    }
    for (src, dst) in bgra.as_chunks::<4>().0.iter().zip(rgba.as_chunks_mut::<4>().0.iter_mut()) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = 255;
    }
    Ok(())
}

/// Extracts and Zstd-compresses a rectangular tile `(x, y, width, height)` from a BGRA8 frame.
pub fn encode_dirty_tile(
    bgra_frame: &[u8],
    frame_w: u32,
    frame_h: u32,
    rect: TileRect,
    zstd_level: i32,
) -> Result<DirtyTile, ProtoError> {
    let fw = frame_w as usize;
    let fh = frame_h as usize;
    let tx = rect.x as usize;
    let ty = rect.y as usize;
    let tw = rect.width as usize;
    let th = rect.height as usize;
    if tw == 0 || th == 0 || tx + tw > fw || ty + th > fh {
        return Err(ProtoError::InvalidInput(format!(
            "tile ({tx},{ty} {tw}x{th}) out of bounds for {fw}x{fh}"
        )));
    }
    if bgra_frame.len() < fw * fh * 4 {
        return Err(ProtoError::InvalidInput("source BGRA buffer too small".into()));
    }

    let mut raw_tile = vec![0u8; tw * th * 4];
    let row_bytes = tw * 4;
    let stride_bytes = fw * 4;
    for row in 0..th {
        let src_off = (ty + row) * stride_bytes + tx * 4;
        let dst_off = row * row_bytes;
        raw_tile[dst_off..dst_off + row_bytes]
            .copy_from_slice(&bgra_frame[src_off..src_off + row_bytes]);
    }

    let zstd_bgra = zstd::bulk::compress(&raw_tile, zstd_level)
        .map_err(|e| ProtoError::Codec(format!("zstd tile compress: {e}")))?;

    Ok(DirtyTile {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
        zstd_bgra,
    })
}

/// Overlap-safe rectangular pixel copy inside an RGBA8 framebuffer.
pub fn apply_move_rect_rgba(
    rgba_buf: &mut [u8],
    frame_w: usize,
    frame_h: usize,
    rect: &MoveRect,
) -> Result<(), ProtoError> {
    let w = rect.width as usize;
    let h = rect.height as usize;
    let sx = rect.src_x as usize;
    let sy = rect.src_y as usize;
    let dx = rect.dst_x as usize;
    let dy = rect.dst_y as usize;

    if w == 0 || h == 0 {
        return Ok(());
    }
    if sx + w > frame_w || sy + h > frame_h || dx + w > frame_w || dy + h > frame_h {
        return Err(ProtoError::InvalidInput("move rect out of framebuffer bounds".into()));
    }
    if rgba_buf.len() < frame_w * frame_h * 4 {
        return Err(ProtoError::InvalidInput("RGBA framebuffer too small".into()));
    }

    let row_bytes = w * 4;
    let stride = frame_w * 4;
    let mut temp_row = vec![0u8; row_bytes];

    if dy <= sy {
        for row in 0..h {
            let src_off = (sy + row) * stride + sx * 4;
            let dst_off = (dy + row) * stride + dx * 4;
            temp_row.copy_from_slice(&rgba_buf[src_off..src_off + row_bytes]);
            rgba_buf[dst_off..dst_off + row_bytes].copy_from_slice(&temp_row);
        }
    } else {
        for row in (0..h).rev() {
            let src_off = (sy + row) * stride + sx * 4;
            let dst_off = (dy + row) * stride + dx * 4;
            temp_row.copy_from_slice(&rgba_buf[src_off..src_off + row_bytes]);
            rgba_buf[dst_off..dst_off + row_bytes].copy_from_slice(&temp_row);
        }
    }
    Ok(())
}

/// Decodes a `VideoMsg::TileBatch` into a persistent RGBA8 framebuffer.
pub fn apply_tile_batch_rgba(
    batch: &VideoMsg,
    rgba_buf: &mut Vec<u8>,
    buf_w: &mut u32,
    buf_h: &mut u32,
) -> Result<(), ProtoError> {
    let VideoMsg::TileBatch {
        width,
        height,
        move_rects,
        tiles,
        ..
    } = batch
    else {
        return Err(ProtoError::Codec("expected TileBatch".into()));
    };

    let fw = *width as usize;
    let fh = *height as usize;
    let required = fw
        .checked_mul(fh)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| ProtoError::Codec("framebuffer dimension overflow".into()))?;
    if required > MAX_TILE_DECOMPRESS {
        return Err(ProtoError::DecompressedTooLarge {
            len: required,
            max: MAX_TILE_DECOMPRESS,
        });
    }

    if *buf_w != *width as u32 || *buf_h != *height as u32 || rgba_buf.len() != required {
        rgba_buf.resize(required, 0);
        *buf_w = *width as u32;
        *buf_h = *height as u32;
    }

    for mv in move_rects {
        apply_move_rect_rgba(rgba_buf, fw, fh, mv)?;
    }

    let stride = fw * 4;
    let mut tile_bgra = Vec::new();
    for tile in tiles {
        let tx = tile.x as usize;
        let ty = tile.y as usize;
        let tw = tile.width as usize;
        let th = tile.height as usize;
        if tw == 0 || th == 0 || tx + tw > fw || ty + th > fh {
            return Err(ProtoError::InvalidInput("dirty tile out of bounds".into()));
        }
        let expected_tile_bytes = tw * th * 4;
        if expected_tile_bytes > MAX_TILE_DECOMPRESS {
            return Err(ProtoError::DecompressedTooLarge {
                len: expected_tile_bytes,
                max: MAX_TILE_DECOMPRESS,
            });
        }

        tile_bgra.clear();
        let decoder = zstd::stream::Decoder::new(tile.zstd_bgra.as_slice())
            .map_err(|e| ProtoError::Codec(format!("tile zstd init: {e}")))?;
        decoder
            .take(expected_tile_bytes as u64 + 1)
            .read_to_end(&mut tile_bgra)
            .map_err(|e| ProtoError::Codec(format!("tile zstd decode: {e}")))?;
        if tile_bgra.len() != expected_tile_bytes {
            return Err(ProtoError::Codec(format!(
                "tile byte count mismatch: expected {expected_tile_bytes}, got {}",
                tile_bgra.len()
            )));
        }

        let row_bytes = tw * 4;
        for row in 0..th {
            let src_off = row * row_bytes;
            let dst_off = (ty + row) * stride + tx * 4;
            bgra_to_rgba_into(
                &tile_bgra[src_off..src_off + row_bytes],
                &mut rgba_buf[dst_off..dst_off + row_bytes],
            )?;
        }
    }
    Ok(())
}

/// Stateful OpenH264 BGRA->I420->Annex-B video encoder handling even-dimension alignment.
pub struct H264VideoEncoder {
    encoder: Encoder,
    padded_w: usize,
    padded_h: usize,
    bitrate_bps: u32,
    max_fps: f32,
    padded_bgra: Vec<u8>,
}

impl H264VideoEncoder {
    pub fn new(width: u32, height: u32, bitrate_bps: u32, max_fps: f32) -> Result<Self, ProtoError> {
        let (padded_w, padded_h) = even_dims(width, height)?;
        let encoder = create_encoder(bitrate_bps, max_fps)?;
        Ok(Self {
            encoder,
            padded_w,
            padded_h,
            bitrate_bps,
            max_fps,
            padded_bgra: Vec::new(),
        })
    }

    pub fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    pub fn reconfigure(&mut self, width: u32, height: u32, bitrate_bps: u32, max_fps: f32) -> Result<(), ProtoError> {
        let (padded_w, padded_h) = even_dims(width, height)?;
        if padded_w != self.padded_w || padded_h != self.padded_h || bitrate_bps != self.bitrate_bps || (max_fps - self.max_fps).abs() > 0.1 {
            self.encoder = create_encoder(bitrate_bps, max_fps)?;
            self.padded_w = padded_w;
            self.padded_h = padded_h;
            self.bitrate_bps = bitrate_bps;
            self.max_fps = max_fps;
        }
        Ok(())
    }

    /// Encodes a tightly-packed BGRA8 frame into `(annex_b_bytes, is_keyframe)`.
    pub fn encode_bgra(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
    ) -> Result<(Vec<u8>, bool), ProtoError> {
        let (pw, ph) = even_dims(width, height)?;
        if pw != self.padded_w || ph != self.padded_h {
            self.reconfigure(width, height, self.bitrate_bps, self.max_fps)?;
        }

        let w = width as usize;
        let h = height as usize;
        if bgra.len() < w * h * 4 {
            return Err(ProtoError::InvalidInput("H264 source BGRA buffer too small".into()));
        }

        let frame_slice = if pw == w && ph == h {
            &bgra[..w * h * 4]
        } else {
            self.padded_bgra.resize(pw * ph * 4, 0);
            self.padded_bgra.fill(0);
            for row in 0..h {
                let src = &bgra[row * w * 4..(row + 1) * w * 4];
                self.padded_bgra[row * pw * 4..row * pw * 4 + w * 4].copy_from_slice(src);
            }
            &self.padded_bgra
        };

        let bgra_src = BgraSliceU8::new(frame_slice, (pw, ph));
        let yuv = YUVBuffer::from_bgra8_source(bgra_src);
        let bitstream = self
            .encoder
            .encode(&yuv)
            .map_err(|e| ProtoError::Codec(format!("openh264 encode: {e}")))?;

        let frame_type = bitstream.frame_type();
        let is_keyframe = matches!(
            frame_type,
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
        );
        Ok((bitstream.to_vec(), is_keyframe))
    }
}

fn even_dims(width: u32, height: u32) -> Result<(usize, usize), ProtoError> {
    if width < 16 || height < 16 || width > 8192 || height > 8192 {
        return Err(ProtoError::InvalidInput(format!(
            "unsupported H264 frame size {width}x{height}"
        )));
    }
    let pw = ((width as usize) + 1) & !1;
    let ph = ((height as usize) + 1) & !1;
    Ok((pw, ph))
}

fn create_encoder(bitrate_bps: u32, max_fps: f32) -> Result<Encoder, ProtoError> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bitrate_bps.max(100_000)))
        .max_frame_rate(FrameRate::from_hz(max_fps.clamp(1.0, 60.0)))
        .rate_control_mode(RateControlMode::Bitrate)
        .skip_frames(true);
    Encoder::with_api_config(OpenH264API::from_source(), config)
        .map_err(|e| ProtoError::Codec(format!("openh264 init: {e}")))
}

/// Stateful OpenH264 Annex-B -> RGBA8 decoder.
pub struct H264VideoDecoder {
    decoder: Decoder,
    scratch_rgba: Vec<u8>,
}

impl H264VideoDecoder {
    pub fn new() -> Result<Self, ProtoError> {
        let decoder = Decoder::new().map_err(|e| ProtoError::Codec(format!("openh264 decoder init: {e}")))?;
        Ok(Self {
            decoder,
            scratch_rgba: Vec::new(),
        })
    }

    /// Decodes an Annex-B packet into `out_rgba` cropped to `(display_w, display_h)`.
    /// Returns `Ok(true)` if a frame was produced, `Ok(false)` if decoder buffered without output.
    pub fn decode_to_rgba(
        &mut self,
        annex_b: &[u8],
        display_w: u32,
        display_h: u32,
        out_rgba: &mut Vec<u8>,
        out_w: &mut u32,
        out_h: &mut u32,
    ) -> Result<bool, ProtoError> {
        let Some(yuv) = self
            .decoder
            .decode(annex_b)
            .map_err(|e| ProtoError::Codec(format!("openh264 decode: {e}")))?
        else {
            return Ok(false);
        };

        let (dec_w, dec_h) = yuv.dimensions();
        let needed_dec = dec_w * dec_h * 4;
        self.scratch_rgba.resize(needed_dec, 0);
        yuv.write_rgba8(&mut self.scratch_rgba);

        let target_w = (display_w as usize).min(dec_w);
        let target_h = (display_h as usize).min(dec_h);
        out_rgba.resize(target_w * target_h * 4, 0);

        if target_w == dec_w && target_h == dec_h {
            out_rgba.copy_from_slice(&self.scratch_rgba[..target_w * target_h * 4]);
        } else {
            for row in 0..target_h {
                let src = &self.scratch_rgba[row * dec_w * 4..row * dec_w * 4 + target_w * 4];
                out_rgba[row * target_w * 4..(row + 1) * target_w * 4].copy_from_slice(src);
            }
        }
        *out_w = target_w as u32;
        *out_h = target_h as u32;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zstd_tile_batch_move_and_dirty_roundtrip() {
        let w = 64u32;
        let h = 64u32;
        let mut src_bgra = vec![0u8; (w * h * 4) as usize];
        // Paint a distinct BGRA block at (4, 4) size (8x8): B=10, G=20, R=200, A=255
        for y in 4..12 {
            for x in 4..12 {
                let off = ((y * w + x) * 4) as usize;
                src_bgra[off] = 10;
                src_bgra[off + 1] = 20;
                src_bgra[off + 2] = 200;
                src_bgra[off + 3] = 255;
            }
        }
        let tile = encode_dirty_tile(
            &src_bgra,
            w,
            h,
            TileRect {
                x: 4,
                y: 4,
                width: 8,
                height: 8,
            },
            3,
        )
        .unwrap();
        let batch1 = VideoMsg::TileBatch {
            monitor_id: 0,
            frame_seq: 1,
            timestamp_us: 100,
            width: w as u16,
            height: h as u16,
            is_full_refresh: false,
            move_rects: vec![],
            tiles: vec![tile],
        };

        let mut rgba = Vec::new();
        let mut bw = 0;
        let mut bh = 0;
        apply_tile_batch_rgba(&batch1, &mut rgba, &mut bw, &mut bh).unwrap();
        assert_eq!((bw, bh), (w, h));
        let px_off = ((6 * w + 6) * 4) as usize;
        assert_eq!(&rgba[px_off..px_off + 4], &[200, 20, 10, 255]);

        // Move (4,4,8x8) -> (16, 16) overlapping or shifted
        let batch2 = VideoMsg::TileBatch {
            monitor_id: 0,
            frame_seq: 2,
            timestamp_us: 200,
            width: w as u16,
            height: h as u16,
            is_full_refresh: false,
            move_rects: vec![MoveRect {
                src_x: 4,
                src_y: 4,
                dst_x: 16,
                dst_y: 16,
                width: 8,
                height: 8,
            }],
            tiles: vec![],
        };
        apply_tile_batch_rgba(&batch2, &mut rgba, &mut bw, &mut bh).unwrap();
        let moved_off = ((18 * w + 18) * 4) as usize;
        assert_eq!(&rgba[moved_off..moved_off + 4], &[200, 20, 10, 255]);
    }

    #[test]
    fn openh264_encode_decode_roundtrip() {
        let w = 160u32;
        let h = 120u32;
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let off = ((y * w + x) * 4) as usize;
                bgra[off] = (x % 255) as u8;
                bgra[off + 1] = (y % 255) as u8;
                bgra[off + 2] = 180;
                bgra[off + 3] = 255;
            }
        }

        let mut enc = H264VideoEncoder::new(w, h, 1_500_000, 30.0).unwrap();
        enc.force_keyframe();
        let (annex_b, is_kf) = enc.encode_bgra(&bgra, w, h).unwrap();
        assert!(is_kf);
        assert!(!annex_b.is_empty());

        let mut dec = H264VideoDecoder::new().unwrap();
        let mut out_rgba = Vec::new();
        let mut ow = 0;
        let mut oh = 0;
        let produced = dec
            .decode_to_rgba(&annex_b, w, h, &mut out_rgba, &mut ow, &mut oh)
            .unwrap();
        assert!(produced);
        assert_eq!((ow, oh), (w, h));
        assert_eq!(out_rgba.len(), (w * h * 4) as usize);
    }
}
