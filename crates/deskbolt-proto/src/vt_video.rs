use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::Mutex;

use crate::codec::bgra_to_rgba_into;
use crate::video_pipeline::{
    VideoCapabilities, VideoCodec, VideoDecoder, VideoEncoder, VideoInput,
};
use crate::{MAX_DECOMPRESSED_PAYLOAD, ProtoError, VideoMsg};

type Ref = *const c_void;
type Result<T> = std::result::Result<T, ProtoError>;

const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const MAX_SIDE: u32 = 8192;

#[repr(C)]
#[derive(Clone, Copy)]
struct Time {
    value: i64,
    scale: i32,
    flags: u32,
    epoch: i64,
}

const INVALID_TIME: Time = Time {
    value: 0,
    scale: 0,
    flags: 0,
    epoch: 0,
};

#[repr(C)]
struct DecodeCallback {
    function: unsafe extern "C" fn(*mut c_void, *mut c_void, i32, u32, Ref, Time, Time),
    context: *mut c_void,
}

type EncodeCallback = unsafe extern "C" fn(*mut c_void, *mut c_void, i32, u32, Ref);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: Ref;
    static kCFBooleanFalse: Ref;
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    fn CFRelease(object: Ref);
    fn CFNumberCreate(allocator: Ref, number_type: isize, value: Ref) -> Ref;
    fn CFDictionaryCreate(
        allocator: Ref,
        keys: *const Ref,
        values: *const Ref,
        count: isize,
        key_callbacks: Ref,
        value_callbacks: Ref,
    ) -> Ref;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: Ref;
    fn CVPixelBufferCreate(
        allocator: Ref,
        width: usize,
        height: usize,
        format: u32,
        attributes: Ref,
        buffer: *mut Ref,
    ) -> i32;
    fn CVPixelBufferLockBaseAddress(buffer: Ref, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: Ref, flags: u64) -> i32;
    fn CVPixelBufferGetWidth(buffer: Ref) -> usize;
    fn CVPixelBufferGetHeight(buffer: Ref) -> usize;
    fn CVPixelBufferGetBytesPerRow(buffer: Ref) -> usize;
    fn CVPixelBufferGetBaseAddress(buffer: Ref) -> *mut u8;
    fn CVPixelBufferGetPixelFormatType(buffer: Ref) -> u32;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetDataBuffer(sample: Ref) -> Ref;
    fn CMSampleBufferGetFormatDescription(sample: Ref) -> Ref;
    fn CMBlockBufferGetDataLength(buffer: Ref) -> usize;
    fn CMBlockBufferCopyDataBytes(buffer: Ref, offset: usize, len: usize, output: *mut u8) -> i32;
    fn CMBlockBufferCreateWithMemoryBlock(
        allocator: Ref,
        ptr: *mut c_void,
        len: usize,
        block_allocator: Ref,
        source: Ref,
        offset: usize,
        data_len: usize,
        flags: u32,
        out: *mut Ref,
    ) -> i32;
    fn CMBlockBufferReplaceDataBytes(
        input: *const u8,
        block: Ref,
        offset: usize,
        len: usize,
    ) -> i32;
    fn CMSampleBufferCreateReady(
        allocator: Ref,
        block: Ref,
        format: Ref,
        frames: isize,
        timing_count: isize,
        timing: Ref,
        size_count: isize,
        lengths: *const usize,
        sample: *mut Ref,
    ) -> i32;
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: Ref,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        length_size: i32,
        format: *mut Ref,
    ) -> i32;
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: Ref,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        length_size: i32,
        extensions: Ref,
        format: *mut Ref,
    ) -> i32;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        format: Ref,
        index: usize,
        data: *mut *const u8,
        len: *mut usize,
        count: *mut usize,
        size: *mut i32,
    ) -> i32;
    fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        format: Ref,
        index: usize,
        data: *mut *const u8,
        len: *mut usize,
        count: *mut usize,
        size: *mut i32,
    ) -> i32;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: Ref;
    static kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: Ref;
    static kVTCompressionPropertyKey_RealTime: Ref;
    static kVTCompressionPropertyKey_AllowFrameReordering: Ref;
    static kVTCompressionPropertyKey_AverageBitRate: Ref;
    static kVTCompressionPropertyKey_ExpectedFrameRate: Ref;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: Ref;
    fn VTIsHardwareDecodeSupported(codec: u32) -> u8;
    fn VTSessionSetProperty(session: Ref, key: Ref, value: Ref) -> i32;
    fn VTCompressionSessionCreate(
        allocator: Ref,
        width: i32,
        height: i32,
        codec: u32,
        spec: Ref,
        source_attributes: Ref,
        output_allocator: Ref,
        callback: EncodeCallback,
        context: *mut c_void,
        session: *mut Ref,
    ) -> i32;
    fn VTCompressionSessionPrepareToEncodeFrames(session: Ref) -> i32;
    fn VTCompressionSessionInvalidate(session: Ref);
    fn VTCompressionSessionEncodeFrame(
        session: Ref,
        image: Ref,
        pts: Time,
        duration: Time,
        properties: Ref,
        source_context: *mut c_void,
        flags: *mut u32,
    ) -> i32;
    fn VTCompressionSessionCompleteFrames(session: Ref, until: Time) -> i32;
    fn VTDecompressionSessionCreate(
        allocator: Ref,
        format: Ref,
        spec: Ref,
        output_attributes: Ref,
        callback: *const DecodeCallback,
        session: *mut Ref,
    ) -> i32;
    fn VTDecompressionSessionDecodeFrame(
        session: Ref,
        sample: Ref,
        flags: u32,
        source_context: *mut c_void,
        result: *mut u32,
    ) -> i32;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: Ref) -> i32;
    fn VTDecompressionSessionInvalidate(session: Ref);
}

struct Owned(Ref);

impl Owned {
    fn create(f: impl FnOnce(*mut Ref) -> i32) -> Result<Self> {
        let mut object = null();
        status(f(&mut object))?;
        Self::new(object)
    }

    fn new(object: Ref) -> Result<Self> {
        if object.is_null() {
            Err(error("The video subsystem did not allocate an object."))
        } else {
            Ok(Self(object))
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
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
            "The operating system video codec returned {code}."
        )))
    }
}

fn codec_type(codec: VideoCodec) -> Option<u32> {
    match codec {
        VideoCodec::H264Hw => Some(u32::from_be_bytes(*b"avc1")),
        VideoCodec::HevcHw => Some(u32::from_be_bytes(*b"hvc1")),
        _ => None,
    }
}

pub fn decoders() -> VideoCapabilities {
    let mut caps = VideoCapabilities::default();
    for codec in [VideoCodec::H264Hw, VideoCodec::HevcHw] {
        if codec_type(codec).is_some_and(|kind| unsafe { VTIsHardwareDecodeSupported(kind) != 0 }) {
            caps.add(codec);
        }
    }
    caps
}

fn dictionary(key: Ref, value: Ref) -> Result<Owned> {
    Owned::new(unsafe {
        CFDictionaryCreate(
            null(),
            &key,
            &value,
            1,
            (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
            (&raw const kCFTypeDictionaryValueCallBacks).cast(),
        )
    })
}

fn number(value: i32) -> Result<Owned> {
    Owned::new(unsafe { CFNumberCreate(null(), 3, (&value as *const i32).cast()) })
}

struct PixelLock(Ref, u64);

impl PixelLock {
    fn lock(buffer: Ref, flags: u64) -> Result<Self> {
        status(unsafe { CVPixelBufferLockBaseAddress(buffer, flags) })?;
        Ok(Self(buffer, flags))
    }
}

impl Drop for PixelLock {
    fn drop(&mut self) {
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, self.1) };
    }
}

type Encoded = (Vec<u8>, bool);
struct EncodeState {
    codec: VideoCodec,
    access_unit: Mutex<Option<Result<Encoded>>>,
}

type Decoded = (Vec<u8>, u32, u32);
struct DecodeState(Mutex<Option<Result<Decoded>>>);

#[derive(Clone, Copy, PartialEq, Eq)]
struct Config(u32, u32, u8, u32);

impl From<VideoInput<'_>> for Config {
    fn from(input: VideoInput<'_>) -> Self {
        Self(input.width, input.height, input.fps, input.bitrate)
    }
}

pub struct Encoder {
    context: Box<EncodeState>,
    session: Owned,
    config: Config,
}

impl Encoder {
    pub fn open(codec: VideoCodec, input: VideoInput<'_>) -> Result<Self> {
        let kind =
            codec_type(codec).ok_or_else(|| error("Unsupported accelerated video codec."))?;
        if !(16..=MAX_SIDE).contains(&input.width) || !(16..=MAX_SIDE).contains(&input.height) {
            return Err(error("Unsupported accelerated video size."));
        }
        let mut context = Box::new(EncodeState {
            codec,
            access_unit: Mutex::new(None),
        });
        let spec = unsafe {
            dictionary(
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                kCFBooleanTrue,
            )?
        };
        let pixel_format = number(BGRA as i32)?;
        let pixels = unsafe { dictionary(kCVPixelBufferPixelFormatTypeKey, pixel_format.0)? };
        let session = Owned::create(|out| unsafe {
            VTCompressionSessionCreate(
                null(),
                input.width as i32,
                input.height as i32,
                kind,
                spec.0,
                pixels.0,
                null(),
                encoded_callback,
                (&mut *context as *mut EncodeState).cast(),
                out,
            )
        })?;
        let encoder = Self {
            context,
            session,
            config: input.into(),
        };
        let bitrate = number(input.bitrate as i32)?;
        let fps = number(i32::from(input.fps))?;
        unsafe {
            for (key, value) in [
                (kVTCompressionPropertyKey_RealTime, kCFBooleanTrue),
                (
                    kVTCompressionPropertyKey_AllowFrameReordering,
                    kCFBooleanFalse,
                ),
                (kVTCompressionPropertyKey_AverageBitRate, bitrate.0),
                (kVTCompressionPropertyKey_ExpectedFrameRate, fps.0),
            ] {
                status(VTSessionSetProperty(encoder.session.0, key, value))?;
            }
            status(VTCompressionSessionPrepareToEncodeFrames(encoder.session.0))?;
        }
        Ok(encoder)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { VTCompressionSessionInvalidate(self.session.0) };
    }
}

impl VideoEncoder for Encoder {
    fn codec(&self) -> VideoCodec {
        self.context.codec
    }

    fn encode(&mut self, input: VideoInput<'_>) -> Result<Option<VideoMsg>> {
        if self.config != Config::from(input) {
            *self = Self::open(self.codec(), input)?;
        }
        let (width, height) = (input.width as usize, input.height as usize);
        if input.bgra.len() != width * height * 4 {
            return Err(error("Invalid desktop frame size."));
        }
        let image = Owned::create(|out| unsafe {
            CVPixelBufferCreate(null(), width, height, BGRA, null(), out)
        })?;
        {
            let _lock = PixelLock::lock(image.0, 0)?;
            unsafe {
                let target = CVPixelBufferGetBaseAddress(image.0);
                let pitch = CVPixelBufferGetBytesPerRow(image.0);
                if target.is_null() || pitch < width * 4 {
                    return Err(error("Invalid accelerated input buffer."));
                }
                for (row, bytes) in input.bgra.chunks_exact(width * 4).enumerate() {
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        target.add(row * pitch),
                        bytes.len(),
                    );
                }
            }
        }
        let pts = Time {
            value: input.sequence as i64,
            scale: i32::from(input.fps),
            flags: 1,
            epoch: 0,
        };
        let flags = if input.refresh {
            Some(unsafe { dictionary(kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue)? })
        } else {
            None
        };
        unsafe {
            status(VTCompressionSessionEncodeFrame(
                self.session.0,
                image.0,
                pts,
                INVALID_TIME,
                flags.as_ref().map_or(null(), |d| d.0),
                null_mut(),
                null_mut(),
            ))?;
            status(VTCompressionSessionCompleteFrames(
                self.session.0,
                INVALID_TIME,
            ))?;
        }
        let frame = self
            .context
            .access_unit
            .lock()
            .unwrap()
            .take()
            .transpose()?;
        Ok(frame.map(|(bytes, keyframe)| input.packet(self.codec(), keyframe, bytes)))
    }
}

unsafe extern "C" fn encoded_callback(
    context: *mut c_void,
    _: *mut c_void,
    code: i32,
    _: u32,
    sample: Ref,
) {
    if context.is_null() || (sample.is_null() && code == 0) {
        return;
    }
    let state = unsafe { &*context.cast::<EncodeState>() };
    let frame = status(code).and_then(|()| read_sample(sample, state.codec));
    if let Ok(mut output) = state.access_unit.lock() {
        *output = Some(frame);
    }
}

fn read_sample(sample: Ref, codec: VideoCodec) -> Result<Encoded> {
    let data = unsafe { CMSampleBufferGetDataBuffer(sample) };
    let format = unsafe { CMSampleBufferGetFormatDescription(sample) };
    if data.is_null() || format.is_null() {
        return Err(error("Missing accelerated video data."));
    }
    let len = unsafe { CMBlockBufferGetDataLength(data) };
    if len > MAX_DECOMPRESSED_PAYLOAD {
        return Err(error("The accelerated video frame is too large."));
    }
    let get_set = if codec == VideoCodec::H264Hw {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex
    } else {
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex
    };
    let mut count = 0;
    let mut length_size = 0;
    status(unsafe {
        get_set(
            format,
            0,
            null_mut(),
            null_mut(),
            &mut count,
            &mut length_size,
        )
    })?;
    if !(1..=4).contains(&length_size) || count > 16 {
        return Err(error("Invalid video parameter sets."));
    }
    let mut encoded = vec![0u8; len];
    status(unsafe { CMBlockBufferCopyDataBytes(data, 0, len, encoded.as_mut_ptr()) })?;
    let parts = length_prefixed(&encoded, length_size as usize)?;
    let keyframe = parts.iter().any(|unit| is_keyframe(codec, unit));
    let mut output = Vec::with_capacity(len);
    if keyframe {
        for index in 0..count {
            let mut data = null();
            let mut size = 0;
            status(unsafe {
                get_set(format, index, &mut data, &mut size, null_mut(), null_mut())
            })?;
            if data.is_null() || size == 0 || size > 64 * 1024 {
                return Err(error("Invalid video parameter set."));
            }
            output.extend_from_slice(&[0, 0, 0, 1]);
            output.extend_from_slice(unsafe { std::slice::from_raw_parts(data, size) });
        }
    }
    for unit in parts {
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(unit);
    }
    Ok((output, keyframe))
}

fn length_prefixed(mut encoded: &[u8], prefix: usize) -> Result<Vec<&[u8]>> {
    let mut parts = Vec::new();
    while !encoded.is_empty() {
        let (length, rest) = encoded
            .split_at_checked(prefix)
            .ok_or_else(|| error("Truncated video packet."))?;
        let size = length
            .iter()
            .fold(0usize, |size, byte| (size << 8) | usize::from(*byte));
        let (unit, remaining) = rest
            .split_at_checked(size)
            .filter(|(unit, _)| !unit.is_empty())
            .ok_or_else(|| error("Invalid accelerated video packet length."))?;
        parts.push(unit);
        if parts.len() > 1024 {
            return Err(error("Too many video units in one packet."));
        }
        encoded = remaining;
    }
    Ok(parts)
}

fn unit_type(codec: VideoCodec, unit: &[u8]) -> u8 {
    if codec == VideoCodec::H264Hw {
        unit[0] & 0x1f
    } else {
        (unit[0] >> 1) & 0x3f
    }
}

fn is_keyframe(codec: VideoCodec, unit: &[u8]) -> bool {
    let kind = unit_type(codec, unit);
    if codec == VideoCodec::H264Hw {
        kind == 5
    } else {
        (16..=23).contains(&kind)
    }
}

struct Decompression {
    session: Owned,
    format: Owned,
}

impl Drop for Decompression {
    fn drop(&mut self) {
        unsafe { VTDecompressionSessionInvalidate(self.session.0) };
    }
}

pub struct Decoder {
    active: Option<Decompression>,
    state: Box<DecodeState>,
    codec: VideoCodec,
    size: (u32, u32),
}

impl Decoder {
    pub fn open(codec: VideoCodec, width: u16, height: u16) -> Result<Self> {
        if !decoders().includes(codec)
            || codec_type(codec).is_none()
            || !(16..=MAX_SIDE).contains(&u32::from(width))
            || !(16..=MAX_SIDE).contains(&u32::from(height))
        {
            return Err(error("The hardware video decoder is unavailable."));
        }
        Ok(Self {
            active: None,
            state: Box::new(DecodeState(Mutex::new(None))),
            codec,
            size: (u32::from(width), u32::from(height)),
        })
    }

    fn configure(&mut self, sets: &[&[u8]]) -> Result<()> {
        let pointers: Vec<_> = sets.iter().map(|set| set.as_ptr()).collect();
        let sizes: Vec<_> = sets.iter().map(|set| set.len()).collect();
        let format = Owned::create(|out| unsafe {
            if self.codec == VideoCodec::H264Hw {
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    null(),
                    pointers.len(),
                    pointers.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    out,
                )
            } else {
                CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    null(),
                    pointers.len(),
                    pointers.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    null(),
                    out,
                )
            }
        })?;
        let callback = DecodeCallback {
            function: decoded_callback,
            context: (&mut *self.state as *mut DecodeState).cast(),
        };
        let pixel_format = number(BGRA as i32)?;
        let spec = unsafe {
            dictionary(
                kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
                kCFBooleanTrue,
            )?
        };
        let attributes = unsafe { dictionary(kCVPixelBufferPixelFormatTypeKey, pixel_format.0)? };
        let session = Owned::create(|out| unsafe {
            VTDecompressionSessionCreate(null(), format.0, spec.0, attributes.0, &callback, out)
        })?;
        self.active = Some(Decompression { session, format });
        Ok(())
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
            bytes, is_keyframe, ..
        } = msg
        else {
            return Ok(false);
        };
        let mut sets = [None; 3];
        let mut data = Vec::with_capacity(bytes.len());
        for unit in openh264::nal_units(bytes) {
            let unit = unit
                .strip_prefix(&[0, 0, 0, 1])
                .or_else(|| unit.strip_prefix(&[0, 0, 1]))
                .unwrap_or_default();
            if unit.is_empty() {
                continue;
            }
            let kind = unit_type(self.codec, unit);
            let slot = match (self.codec, kind) {
                (VideoCodec::H264Hw, 7 | 8) => Some(usize::from(kind - 7)),
                (VideoCodec::HevcHw, 32..=34) => Some(usize::from(kind - 32)),
                _ => None,
            };
            if let Some(index) = slot {
                sets[index] = Some(unit);
            } else {
                data.extend_from_slice(&(unit.len() as u32).to_be_bytes());
                data.extend_from_slice(unit);
            }
        }
        if *is_keyframe && (sets[0].is_some() || self.active.is_none()) {
            let parameters: Vec<_> = sets.into_iter().flatten().collect();
            if parameters.len()
                != if self.codec == VideoCodec::H264Hw {
                    2
                } else {
                    3
                }
            {
                return Err(error("The keyframe is missing video configuration."));
            }
            self.configure(&parameters)?;
        }
        let Some(active) = self.active.as_ref() else {
            return Ok(false);
        };
        if data.is_empty() {
            return Ok(false);
        }
        let block = Owned::create(|out| unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                null(),
                null_mut(),
                data.len(),
                null(),
                null(),
                0,
                data.len(),
                1,
                out,
            )
        })?;
        status(unsafe { CMBlockBufferReplaceDataBytes(data.as_ptr(), block.0, 0, data.len()) })?;
        let sample = Owned::create(|out| unsafe {
            CMSampleBufferCreateReady(
                null(),
                block.0,
                active.format.0,
                1,
                0,
                null(),
                1,
                &data.len(),
                out,
            )
        })?;
        unsafe {
            status(VTDecompressionSessionDecodeFrame(
                active.session.0,
                sample.0,
                0,
                null_mut(),
                null_mut(),
            ))?;
            status(VTDecompressionSessionWaitForAsynchronousFrames(
                active.session.0,
            ))?;
        }
        let Some((pixels, w, h)) = self.state.0.lock().unwrap().take().transpose()? else {
            return Ok(false);
        };
        if self.size != (w, h) {
            return Err(error("Unexpected hardware decoded frame size."));
        }
        *rgba = pixels;
        (*width, *height) = (w, h);
        Ok(true)
    }
}

unsafe extern "C" fn decoded_callback(
    context: *mut c_void,
    _: *mut c_void,
    code: i32,
    _: u32,
    image: Ref,
    _: Time,
    _: Time,
) {
    if context.is_null() || (image.is_null() && code == 0) {
        return;
    }
    let state = unsafe { &*context.cast::<DecodeState>() };
    let frame = status(code).and_then(|()| read_pixels(image));
    if let Ok(mut output) = state.0.lock() {
        *output = Some(frame);
    }
}

fn read_pixels(image: Ref) -> Result<Decoded> {
    let _lock = PixelLock::lock(image, 1)?;
    let (w, h, pitch, source) = unsafe {
        if CVPixelBufferGetPixelFormatType(image) != BGRA {
            return Err(error("Unexpected accelerated pixel format."));
        }
        (
            CVPixelBufferGetWidth(image),
            CVPixelBufferGetHeight(image),
            CVPixelBufferGetBytesPerRow(image),
            CVPixelBufferGetBaseAddress(image),
        )
    };
    if source.is_null()
        || !(1..=MAX_SIDE as usize).contains(&w)
        || !(1..=MAX_SIDE as usize).contains(&h)
        || pitch < w * 4
    {
        return Err(error("Invalid accelerated output buffer."));
    }
    let mut rgba = vec![0; w * h * 4];
    for (row, dest) in rgba.chunks_exact_mut(w * 4).enumerate() {
        let bgra = unsafe { std::slice::from_raw_parts(source.add(row * pitch), w * 4) };
        bgra_to_rgba_into(bgra, dest)?;
    }
    Ok((rgba, w as u32, h as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_unit_lengths_and_keyframe_headers_are_bounded() {
        let bytes = [0, 2, 5, 66, 0, 1, 1];
        let units = length_prefixed(&bytes, 2).unwrap();
        assert!(is_keyframe(VideoCodec::H264Hw, units[0]));
        assert!(!is_keyframe(VideoCodec::H264Hw, units[1]));
        assert!(length_prefixed(&bytes[..6], 2).is_err());
    }
}
