use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::{NonNull, null_mut};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

use crate::video_pipeline::{
    VideoCapabilities, VideoCodec, VideoDecoder, VideoEncoder, VideoInput,
};
use crate::{MAX_DECOMPRESSED_PAYLOAD, ProtoError, VideoMsg};

type Result<T> = std::result::Result<T, ProtoError>;
type Handle = *mut c_void;

const COLOR_I420: i32 = 19;
const COLOR_NV12: i32 = 21;
const CONFIG: u32 = 2;
const SYNC: u32 = 1;
const MAX_SIDE: u32 = 8192;

#[repr(C)]
#[derive(Default)]
struct BufferInfo {
    offset: i32,
    size: i32,
    timestamp: i64,
    flags: u32,
}

#[link(name = "mediandk")]
unsafe extern "C" {
    fn AMediaCodec_createCodecByName(name: *const c_char) -> Handle;
    fn AMediaCodec_configure(
        codec: Handle,
        format: Handle,
        surface: Handle,
        crypto: Handle,
        flags: u32,
    ) -> i32;
    fn AMediaCodec_start(codec: Handle) -> i32;
    fn AMediaCodec_stop(codec: Handle) -> i32;
    fn AMediaCodec_delete(codec: Handle) -> i32;
    fn AMediaCodec_dequeueInputBuffer(codec: Handle, timeout_us: i64) -> isize;
    fn AMediaCodec_getInputBuffer(codec: Handle, index: usize, size: *mut usize) -> *mut u8;
    fn AMediaCodec_queueInputBuffer(
        codec: Handle,
        index: usize,
        offset: i64,
        size: usize,
        pts: u64,
        flags: u32,
    ) -> i32;
    fn AMediaCodec_dequeueOutputBuffer(
        codec: Handle,
        info: *mut BufferInfo,
        timeout_us: i64,
    ) -> isize;
    fn AMediaCodec_getOutputBuffer(codec: Handle, index: usize, size: *mut usize) -> *const u8;
    fn AMediaCodec_releaseOutputBuffer(codec: Handle, index: usize, render: bool) -> i32;
    fn AMediaCodec_getOutputFormat(codec: Handle) -> Handle;
    fn AMediaCodec_setParameters(codec: Handle, params: Handle) -> i32;
    fn AMediaFormat_new() -> Handle;
    fn AMediaFormat_delete(format: Handle) -> i32;
    fn AMediaFormat_setString(format: Handle, key: *const c_char, val: *const c_char);
    fn AMediaFormat_setInt32(format: Handle, key: *const c_char, val: i32);
    fn AMediaFormat_getInt32(format: Handle, key: *const c_char, val: *mut i32) -> bool;
}

#[derive(Debug)]
struct Entry {
    codec: VideoCodec,
    name: CString,
    encode: bool,
    color: i32,
}

static ENTRIES: OnceLock<Vec<Entry>> = OnceLock::new();

pub fn register(description: &str) {
    let _ = ENTRIES.set(parse(description));
}

fn parse(description: &str) -> Vec<Entry> {
    description
        .lines()
        .take(128)
        .filter_map(|line| {
            let mut fields = line.splitn(4, '|');
            let codec = match fields.next()? {
                "video/avc" => VideoCodec::H264Hw,
                "video/hevc" => VideoCodec::HevcHw,
                "video/av01" => VideoCodec::Av1Hw,
                _ => return None,
            };
            let encode = match fields.next()? {
                "e" => true,
                "d" => false,
                _ => return None,
            };
            let color = fields.next()?.parse::<i32>().ok()?;
            if !matches!(color, COLOR_I420 | COLOR_NV12) {
                return None;
            }
            let name = fields.next()?;
            if name.is_empty() || name.len() > 256 {
                return None;
            }
            Some(Entry {
                codec,
                name: CString::new(name).ok()?,
                encode,
                color,
            })
        })
        .collect()
}

pub fn decoders() -> VideoCapabilities {
    let mut caps = VideoCapabilities::default();
    for entry in ENTRIES
        .get()
        .into_iter()
        .flatten()
        .filter(|entry| !entry.encode)
    {
        caps.add(entry.codec);
    }
    caps
}

fn mime(codec: VideoCodec) -> &'static CStr {
    match codec {
        VideoCodec::H264Hw => c"video/avc",
        VideoCodec::HevcHw => c"video/hevc",
        VideoCodec::Av1Hw => c"video/av01",
        _ => unreachable!("no codec identifier for software paths"),
    }
}

fn error(message: &str) -> ProtoError {
    ProtoError::Codec(message.into())
}

fn status(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(ProtoError::Codec(format!(
            "The system video accelerator returned {code}."
        )))
    }
}

struct MediaFormat(NonNull<c_void>);

impl MediaFormat {
    fn new() -> Result<Self> {
        Self::owned(unsafe { AMediaFormat_new() })
    }

    fn owned(format: Handle) -> Result<Self> {
        NonNull::new(format)
            .map(Self)
            .ok_or_else(|| error("Could not allocate the video format."))
    }

    fn get(&self, key: &CStr, default: i32) -> i32 {
        let mut value = 0;
        if unsafe { AMediaFormat_getInt32(self.0.as_ptr(), key.as_ptr(), &mut value) } {
            value
        } else {
            default
        }
    }

    fn set(&self, key: &CStr, value: i32) {
        unsafe { AMediaFormat_setInt32(self.0.as_ptr(), key.as_ptr(), value) }
    }
}

impl Drop for MediaFormat {
    fn drop(&mut self) {
        unsafe { AMediaFormat_delete(self.0.as_ptr()) };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Config {
    width: u32,
    height: u32,
    fps: u8,
    bitrate: u32,
}

impl From<VideoInput<'_>> for Config {
    fn from(input: VideoInput<'_>) -> Self {
        Self {
            width: input.width,
            height: input.height,
            fps: input.fps,
            bitrate: input.bitrate,
        }
    }
}

struct Codec {
    raw: NonNull<c_void>,
    started: bool,
    config: Config,
    empty: u8,
    color: i32,
    output_format: Option<MediaFormat>,
}

impl Codec {
    fn open(codec: VideoCodec, encode: bool, config: Config) -> Result<Self> {
        if !(16..=MAX_SIDE).contains(&config.width)
            || !(16..=MAX_SIDE).contains(&config.height)
            || !config.width.is_multiple_of(2)
            || !config.height.is_multiple_of(2)
        {
            return Err(error(
                "The hardware video codec cannot use these dimensions.",
            ));
        }
        for entry in ENTRIES
            .get()
            .into_iter()
            .flatten()
            .filter(|entry| entry.codec == codec && entry.encode == encode)
        {
            if let Ok(opened) = Self::create(entry, config) {
                return Ok(opened);
            }
        }
        Err(error("No compatible hardware video codec is available."))
    }

    fn create(entry: &Entry, config: Config) -> Result<Self> {
        let raw = NonNull::new(unsafe { AMediaCodec_createCodecByName(entry.name.as_ptr()) })
            .ok_or_else(|| error("The system failed to open the hardware video codec."))?;
        let mut codec = Self {
            raw,
            started: false,
            config,
            empty: 0,
            color: entry.color,
            output_format: None,
        };
        let format = MediaFormat::new()?;
        let media = format.0.as_ptr();
        unsafe { AMediaFormat_setString(media, c"mime".as_ptr(), mime(entry.codec).as_ptr()) };
        format.set(c"width", config.width as i32);
        format.set(c"height", config.height as i32);
        format.set(c"color-format", entry.color);
        if entry.encode {
            format.set(c"bitrate", config.bitrate as i32);
            format.set(c"frame-rate", i32::from(config.fps));
            format.set(c"i-frame-interval", 2);
            format.set(c"max-bframes", 0);
            format.set(c"prepend-sps-pps-to-idr-frames", 1);
        }
        unsafe {
            status(AMediaCodec_configure(
                raw.as_ptr(),
                media,
                null_mut(),
                null_mut(),
                u32::from(entry.encode),
            ))?;
            status(AMediaCodec_start(raw.as_ptr()))?;
        }
        codec.started = true;
        Ok(codec)
    }

    fn request_keyframe(&self) -> Result<()> {
        let params = MediaFormat::new()?;
        params.set(c"request-sync", 0);
        status(unsafe { AMediaCodec_setParameters(self.raw.as_ptr(), params.0.as_ptr()) })
    }

    fn input(&self, bytes: &[u8], pts: u64) -> Result<()> {
        let raw = self.raw.as_ptr();
        let index = unsafe { AMediaCodec_dequeueInputBuffer(raw, 25_000) };
        let index = usize::try_from(index)
            .map_err(|_| error("No hardware video input buffer became available."))?;
        let mut capacity = 0;
        unsafe {
            let buffer = AMediaCodec_getInputBuffer(raw, index, &mut capacity);
            if buffer.is_null() || bytes.len() > capacity {
                return Err(error("The hardware codec rejected the input buffer size."));
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
            status(AMediaCodec_queueInputBuffer(
                raw,
                index,
                0,
                bytes.len(),
                pts,
                0,
            ))
        }
    }

    fn output(&mut self, max_size: usize) -> Result<Option<(Vec<u8>, u32)>> {
        let raw = self.raw.as_ptr();
        let deadline = Instant::now() + Duration::from_millis(35);
        let mut info = BufferInfo::default();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let index = unsafe {
                AMediaCodec_dequeueOutputBuffer(raw, &mut info, remaining.as_micros() as i64)
            };
            match index {
                n if n >= 0 => {
                    let index = n as usize;
                    let result = unsafe {
                        let mut size = 0;
                        let buffer = AMediaCodec_getOutputBuffer(raw, index, &mut size);
                        let offset = usize::try_from(info.offset).unwrap_or(usize::MAX);
                        let length = usize::try_from(info.size).unwrap_or(usize::MAX);
                        let result = if !buffer.is_null()
                            && length <= max_size
                            && offset.saturating_add(length) <= size
                        {
                            Ok(std::slice::from_raw_parts(buffer.add(offset), length).to_vec())
                        } else {
                            Err(error(
                                "The hardware codec returned an invalid output buffer.",
                            ))
                        };
                        status(AMediaCodec_releaseOutputBuffer(raw, index, false))?;
                        result
                    }?;
                    self.empty = 0;
                    return Ok(Some((result, info.flags)));
                }
                -2 => {
                    self.output_format = Some(MediaFormat::owned(unsafe {
                        AMediaCodec_getOutputFormat(raw)
                    })?)
                }
                -3 => {}
                -1 => break,
                _ => {
                    return Err(error(
                        "The hardware codec failed while waiting for a video frame.",
                    ));
                }
            }
            if remaining.is_zero() {
                break;
            }
        }
        self.empty += 1;
        if self.empty >= 4 {
            Err(error("The hardware codec stopped producing video."))
        } else {
            Ok(None)
        }
    }
}

impl Drop for Codec {
    fn drop(&mut self) {
        unsafe {
            if self.started {
                AMediaCodec_stop(self.raw.as_ptr());
            }
            AMediaCodec_delete(self.raw.as_ptr());
        }
    }
}

pub struct Encoder {
    codec: VideoCodec,
    inner: Codec,
    headers: Vec<u8>,
    sent_first: bool,
}

impl Encoder {
    pub fn open(codec: VideoCodec, input: VideoInput<'_>) -> Result<Self> {
        Ok(Self {
            codec,
            inner: Codec::open(codec, true, input.into())?,
            headers: Vec::new(),
            sent_first: false,
        })
    }
}

impl VideoEncoder for Encoder {
    fn codec(&self) -> VideoCodec {
        self.codec
    }

    fn encode(&mut self, input: VideoInput<'_>) -> Result<Option<VideoMsg>> {
        if self.inner.config != input.into() {
            *self = Self::open(self.codec, input)?;
        }
        if input.refresh && self.sent_first && self.inner.request_keyframe().is_err() {
            *self = Self::open(self.codec, input)?;
        }
        let (width, height) = (input.width as usize, input.height as usize);
        if input.bgra.len() != width * height * 4 {
            return Err(error("Invalid accelerated video input dimensions."));
        }
        let yuv = YUVBuffer::from_bgra8_source(BgraSliceU8::new(input.bgra, (width, height)));
        let mut bytes = Vec::with_capacity(width * height * 3 / 2);
        bytes.extend_from_slice(yuv.y());
        if self.inner.color == COLOR_NV12 {
            for (&u, &v) in yuv.u().iter().zip(yuv.v()) {
                bytes.extend([u, v]);
            }
        } else {
            bytes.extend_from_slice(yuv.u());
            bytes.extend_from_slice(yuv.v());
        }
        self.inner.input(&bytes, input.timestamp_us)?;
        for _ in 0..2 {
            let Some((mut bytes, flags)) = self.inner.output(MAX_DECOMPRESSED_PAYLOAD)? else {
                return Ok(None);
            };
            if flags & CONFIG != 0 {
                if bytes.len() > 128 * 1024 {
                    return Err(error("Invalid configuration packet size."));
                }
                self.headers = bytes;
                continue;
            }
            if bytes.is_empty() {
                return Ok(None);
            }
            let keyframe = flags & SYNC != 0;
            if !self.sent_first && !keyframe {
                return Err(error("The hardware encoder did not start with a keyframe."));
            }
            if keyframe && !self.headers.is_empty() && !bytes.starts_with(&self.headers) {
                let mut prefix = self.headers.clone();
                prefix.append(&mut bytes);
                bytes = prefix;
            }
            self.sent_first = true;
            return Ok(Some(input.packet(self.codec, keyframe, bytes)));
        }
        Ok(None)
    }
}

pub struct Decoder(Codec);

impl Decoder {
    pub fn open(codec: VideoCodec, width: u16, height: u16) -> Result<Self> {
        Ok(Self(Codec::open(
            codec,
            false,
            Config {
                width: u32::from(width),
                height: u32::from(height),
                fps: 60,
                bitrate: 0,
            },
        )?))
    }
}

impl VideoDecoder for Decoder {
    fn decode(
        &mut self,
        msg: &VideoMsg,
        rgba: &mut Vec<u8>,
        width: &mut u32,
        height: &mut u32,
    ) -> Result<bool> {
        let VideoMsg::EncodedFrame {
            bytes,
            timestamp_us,
            ..
        } = msg
        else {
            return Ok(false);
        };
        self.0.input(bytes, *timestamp_us)?;
        let Some((pixels, _)) = self.0.output(MAX_SIDE as usize * MAX_SIDE as usize * 2)? else {
            return Ok(false);
        };
        let config = self.0.config;
        let format = self
            .0
            .output_format
            .as_ref()
            .ok_or_else(|| error("The decoder did not declare its pixel format."))?;
        let stride = format.get(c"stride", 0).max(config.width as i32);
        let slice = format.get(c"slice-height", 0).max(config.height as i32);
        let color = format.get(c"color-format", self.0.color);
        if !matches!(color, COLOR_I420 | COLOR_NV12)
            || format.get(c"crop-left", 0) != 0
            || format.get(c"crop-top", 0) != 0
            || !(crate::yuv_video::Yuv420 {
                pixels: &pixels,
                width: config.width as usize,
                height: config.height as usize,
                stride: stride as usize,
                stored_height: slice as usize,
                planar: color == COLOR_I420,
            })
            .write_rgba(rgba)
        {
            return Err(error(
                "The hardware decoder produced an unsupported video layout.",
            ));
        }
        (*width, *height) = (config.width, config.height);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_limits_hardware_to_supported_buffer_formats() {
        let entries = parse(
            "video/hevc|d|19|platform.hevc\nvideo/avc|e|21|platform.avc\nvideo/av01|d|54|unusable\n",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].codec, VideoCodec::HevcHw);
        assert!(entries[1].encode);
    }
}
