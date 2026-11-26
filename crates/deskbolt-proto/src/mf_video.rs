use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, HRESULT, Interface, s, w};

use crate::video_pipeline::{
    VideoCapabilities, VideoCodec, VideoDecoder, VideoEncoder, VideoInput,
};
use crate::{ProtoError, VideoMsg};

type Result<T> = std::result::Result<T, ProtoError>;

fn codec_error(error: windows::core::Error) -> ProtoError {
    ProtoError::Codec(format!("Video acceleration: {error}"))
}

type Enumerate = unsafe extern "system" fn(
    GUID,
    u32,
    *const MFT_REGISTER_TYPE_INFO,
    *const MFT_REGISTER_TYPE_INFO,
    *mut *mut Option<IMFActivate>,
    *mut u32,
) -> HRESULT;
type CreateInterface = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;

struct Api {
    startup: unsafe extern "system" fn(u32, u32) -> HRESULT,
    shutdown: unsafe extern "system" fn() -> HRESULT,
    enumerate: Enumerate,
    media_type: CreateInterface,
    sample: CreateInterface,
    buffer: unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT,
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        // Keep the system DLL loaded for as long as any codec can hold an interface.
        let dll = LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
        macro_rules! entry {
            ($name:literal, $signature:ty) => {
                std::mem::transmute::<unsafe extern "system" fn() -> isize, $signature>(
                    GetProcAddress(dll, s!($name))?,
                )
            };
        }
        Some(Api {
            startup: entry!("MFStartup", unsafe extern "system" fn(u32, u32) -> HRESULT),
            shutdown: entry!("MFShutdown", unsafe extern "system" fn() -> HRESULT),
            enumerate: entry!("MFTEnumEx", Enumerate),
            media_type: entry!("MFCreateMediaType", CreateInterface),
            sample: entry!("MFCreateSample", CreateInterface),
            buffer: entry!(
                "MFCreateMemoryBuffer",
                unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT
            ),
        })
    })
    .as_ref()
    .ok_or_else(|| {
        ProtoError::Codec("The operating system video accelerator is unavailable.".into())
    })
}

struct MediaFoundation(&'static Api);

impl MediaFoundation {
    fn start() -> Result<Self> {
        // Both the codec and its COM apartment stay on the video worker thread.
        let api = api()?;
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(codec_error)?;
            if let Err(error) = (api.startup)(MF_VERSION, MFSTARTUP_LITE).ok() {
                CoUninitialize();
                return Err(codec_error(error));
            }
        }
        Ok(Self(api))
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.0.shutdown)();
            CoUninitialize();
        }
    }
}

fn subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::H264Hw => MFVideoFormat_H264,
        VideoCodec::HevcHw => MFVideoFormat_HEVC,
        VideoCodec::Av1Hw => MFVideoFormat_AV1,
        _ => unreachable!("only native codecs are enumerated"),
    }
}

fn activations(codec: VideoCodec, encode: bool) -> Result<Vec<IMFActivate>> {
    let compressed = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(codec),
    };
    let raw = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let mut entries = std::ptr::null_mut();
    let mut count = 0;
    let (category, input, output) = if encode {
        (MFT_CATEGORY_VIDEO_ENCODER, &raw, &compressed)
    } else {
        (MFT_CATEGORY_VIDEO_DECODER, &compressed, &raw)
    };
    let api = api()?;
    unsafe {
        (api.enumerate)(
            category,
            (MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER).0 as u32,
            input,
            output,
            &mut entries,
            &mut count,
        )
        .ok()
        .map_err(codec_error)?;
        let mut devices = Vec::new();
        if !entries.is_null() {
            for slot in std::slice::from_raw_parts_mut(entries, count as usize) {
                if let Some(device) = slot.take() {
                    devices.push(device);
                }
            }
            CoTaskMemFree(Some(entries.cast()));
        }
        Ok(devices)
    }
}

pub fn decoders() -> VideoCapabilities {
    let mut caps = VideoCapabilities::default();
    if let Ok(_runtime) = MediaFoundation::start() {
        for codec in VideoCodec::HARDWARE {
            if activations(codec, false).is_ok_and(|devices| !devices.is_empty()) {
                caps.add(codec);
            }
        }
    }
    caps
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Format {
    width: u32,
    height: u32,
    fps: u8,
    bitrate: u32,
}

impl Format {
    fn validate(self) -> Result<Self> {
        if !(16..=8192).contains(&self.width)
            || !(16..=8192).contains(&self.height)
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(ProtoError::Codec(
                "The accelerated codec cannot use this frame size.".into(),
            ));
        }
        Ok(self)
    }

    fn duration(self) -> i64 {
        10_000_000 / i64::from(self.fps.max(1))
    }

    fn media_type(
        self,
        api: &Api,
        subtype: GUID,
        compressed: bool,
    ) -> windows::core::Result<IMFMediaType> {
        unsafe {
            let mut media = std::ptr::null_mut();
            (api.media_type)(&mut media).ok()?;
            let media = IMFMediaType::from_raw(media);
            media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            media.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            media.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(self.width) << 32) | u64::from(self.height),
            )?;
            media.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(self.fps.max(1)) << 32) | 1)?;
            media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
            media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            if compressed && self.bitrate > 0 {
                media.SetUINT32(&MF_MT_AVG_BITRATE, self.bitrate)?;
            }
            Ok(media)
        }
    }
}

struct Transform {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    input: u32,
    output: u32,
    credit: u32,
    format: Format,
    duration: i64,
    timestamp: i64,
    empty_outputs: u8,
    runtime: MediaFoundation,
}

impl Transform {
    fn open(codec: VideoCodec, encode: bool, format: Format) -> Result<Self> {
        let format = format.validate()?;
        let runtime = MediaFoundation::start()?;
        for device in activations(codec, encode)? {
            // An enumerated hardware transform can still reject this frame size or system-memory input.
            let setup: windows::core::Result<_> = unsafe {
                (|| {
                    let transform: IMFTransform = device.ActivateObject()?;
                    let events: IMFMediaEventGenerator = transform.cast()?;
                    let attrs = transform.GetAttributes()?;
                    attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                    let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
                    if let Ok(api) = transform.cast::<ICodecAPI>() {
                        let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                    }
                    let (mut input, mut output) = ([0], [0]);
                    if let Err(error) = transform.GetStreamIDs(&mut input, &mut output)
                        && error.code() != windows::Win32::Foundation::E_NOTIMPL
                    {
                        return Err(error);
                    }
                    let (input, output) = (input[0], output[0]);
                    let raw = format.media_type(runtime.0, MFVideoFormat_NV12, false)?;
                    let compressed = format.media_type(runtime.0, subtype(codec), true)?;
                    if encode {
                        transform.SetOutputType(output, &compressed, 0)?;
                        transform.SetInputType(input, &raw, 0)?;
                    } else {
                        transform.SetInputType(input, &compressed, 0)?;
                        transform.SetOutputType(output, &raw, 0)?;
                    }
                    transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
                    transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
                    Ok((transform, events, input, output))
                })()
            };
            if let Ok((transform, events, input, output)) = setup {
                return Ok(Self {
                    transform,
                    events,
                    input,
                    output,
                    format,
                    duration: format.duration(),
                    timestamp: 0,
                    credit: 0,
                    empty_outputs: 0,
                    runtime,
                });
            }
        }
        Err(ProtoError::Codec(
            "No compatible hardware video transform is available.".into(),
        ))
    }

    fn force_keyframe(&self) -> Result<()> {
        unsafe {
            self.transform
                .cast::<ICodecAPI>()
                .and_then(|api| {
                    api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(true))
                })
                .map_err(codec_error)
        }
    }

    fn output(&self, decode: bool) -> Result<Option<IMFSample>> {
        unsafe {
            for _ in 0..2 {
                let info = self
                    .transform
                    .GetOutputStreamInfo(self.output)
                    .map_err(codec_error)?;
                let mask =
                    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0;
                let sample = if info.dwFlags & mask as u32 == 0 {
                    let min_raw = if decode {
                        self.format.width * self.format.height * 3 / 2
                    } else {
                        0
                    };
                    Some(make_sample(self.runtime.0, info.cbSize.max(min_raw), &[])?)
                } else {
                    None
                };
                let mut buffer = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: self.output,
                    pSample: ManuallyDrop::new(sample),
                    pEvents: ManuallyDrop::new(None),
                    dwStatus: 0,
                }];
                let result = self.transform.ProcessOutput(0, &mut buffer, &mut 0);
                let sample = ManuallyDrop::take(&mut buffer[0].pSample);
                ManuallyDrop::drop(&mut buffer[0].pEvents);
                match result {
                    Ok(()) => return Ok(sample),
                    Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                        return Ok(None);
                    }
                    Err(error) if decode && error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        let mut index = 0;
                        loop {
                            let offered = self
                                .transform
                                .GetOutputAvailableType(self.output, index)
                                .map_err(codec_error)?;
                            if offered.GetGUID(&MF_MT_SUBTYPE).map_err(codec_error)?
                                == MFVideoFormat_NV12
                            {
                                self.transform
                                    .SetOutputType(self.output, &offered, 0)
                                    .map_err(codec_error)?;
                                break;
                            }
                            index += 1;
                        }
                    }
                    Err(error) => return Err(codec_error(error)),
                }
            }
        }
        Ok(None)
    }

    fn transcode(&mut self, bytes: &[u8], decode: bool) -> Result<Option<IMFSample>> {
        let deadline = Instant::now() + Duration::from_millis(35);
        let mut incoming = Some(bytes);
        let mut output = None;
        while Instant::now() < deadline {
            if self.credit > 0
                && let Some(bytes) = incoming.take()
            {
                let size = u32::try_from(bytes.len())
                    .map_err(|_| ProtoError::Codec("Video sample is too large.".into()))?;
                let sample = make_sample(self.runtime.0, size, bytes)?;
                unsafe {
                    sample.SetSampleTime(self.timestamp).map_err(codec_error)?;
                    sample
                        .SetSampleDuration(self.duration)
                        .map_err(codec_error)?;
                    self.transform
                        .ProcessInput(self.input, &sample, 0)
                        .map_err(codec_error)?;
                }
                self.credit -= 1;
                self.timestamp += self.duration;
            }
            if incoming.is_none() && output.is_some() {
                break;
            }
            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => return Err(codec_error(error)),
            };
            unsafe {
                event
                    .GetStatus()
                    .and_then(|status| status.ok())
                    .map_err(codec_error)?;
                let kind = event.GetType().map_err(codec_error)?;
                if kind == METransformNeedInput.0 as u32 {
                    self.credit += 1;
                } else if kind == METransformHaveOutput.0 as u32 {
                    output = self.output(decode)?;
                }
            }
        }
        self.empty_outputs = if output.is_none() {
            self.empty_outputs + 1
        } else {
            0
        };
        if self.empty_outputs >= 4 {
            return Err(ProtoError::Codec(
                "The hardware video transform stopped producing frames.".into(),
            ));
        }
        Ok(output)
    }
}

impl Drop for Transform {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

fn make_sample(api: &Api, capacity: u32, data: &[u8]) -> Result<IMFSample> {
    unsafe {
        let mut sample = std::ptr::null_mut();
        (api.sample)(&mut sample).ok().map_err(codec_error)?;
        let sample = IMFSample::from_raw(sample);
        let mut buffer = std::ptr::null_mut();
        (api.buffer)(capacity, &mut buffer)
            .ok()
            .map_err(codec_error)?;
        let buffer = IMFMediaBuffer::from_raw(buffer);
        if !data.is_empty() {
            let mut pointer = std::ptr::null_mut();
            buffer.Lock(&mut pointer, None, None).map_err(codec_error)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), pointer, data.len());
            buffer.Unlock().map_err(codec_error)?;
            buffer
                .SetCurrentLength(data.len() as u32)
                .map_err(codec_error)?;
        }
        sample.AddBuffer(&buffer).map_err(codec_error)?;
        Ok(sample)
    }
}

fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().map_err(codec_error)?;
        let mut pointer = std::ptr::null_mut();
        let mut length = 0;
        buffer
            .Lock(&mut pointer, None, Some(&mut length))
            .map_err(codec_error)?;
        let data = if length == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(pointer, length as usize).to_vec()
        };
        buffer.Unlock().map_err(codec_error)?;
        Ok(data)
    }
}

pub struct Encoder {
    codec: VideoCodec,
    inner: Transform,
    first: bool,
}

impl Encoder {
    pub fn open(codec: VideoCodec, input: VideoInput<'_>) -> Result<Self> {
        Ok(Self {
            codec,
            inner: Transform::open(codec, true, encoder_format(input))?,
            first: true,
        })
    }
}

fn encoder_format(input: VideoInput<'_>) -> Format {
    Format {
        width: input.width,
        height: input.height,
        fps: input.fps,
        bitrate: input.bitrate,
    }
}

impl VideoEncoder for Encoder {
    fn codec(&self) -> VideoCodec {
        self.codec
    }

    fn encode(&mut self, input: VideoInput<'_>) -> Result<Option<VideoMsg>> {
        if self.inner.format != encoder_format(input) {
            *self = Self::open(self.codec, input)?;
        }
        if input.refresh && !self.first && self.inner.force_keyframe().is_err() {
            *self = Self::open(self.codec, input)?;
        }
        let (width, height) = (input.width as usize, input.height as usize);
        if input.bgra.len() != width * height * 4 {
            return Err(ProtoError::Codec(
                "Invalid accelerated video input size.".into(),
            ));
        }
        let yuv = YUVBuffer::from_bgra8_source(BgraSliceU8::new(input.bgra, (width, height)));
        let mut nv12 = Vec::with_capacity(width * height * 3 / 2);
        nv12.extend_from_slice(yuv.y());
        for (&u, &v) in yuv.u().iter().zip(yuv.v()) {
            nv12.extend([u, v]);
        }
        let Some(sample) = self.inner.transcode(&nv12, false)? else {
            return Ok(None);
        };
        let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0 };
        self.first = false;
        let bytes = sample_bytes(&sample)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(input.packet(self.codec, keyframe, bytes)))
    }
}

pub struct Decoder {
    inner: Transform,
}

impl Decoder {
    pub fn open(codec: VideoCodec, width: u32, height: u32) -> Result<Self> {
        Ok(Self {
            inner: Transform::open(
                codec,
                false,
                Format {
                    width,
                    height,
                    fps: 60,
                    bitrate: 0,
                },
            )?,
        })
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
        let VideoMsg::EncodedFrame { bytes, .. } = msg else {
            return Ok(false);
        };
        let Some(sample) = self.inner.transcode(bytes, true)? else {
            return Ok(false);
        };
        let data = sample_bytes(&sample)?;
        let (w, h) = (
            self.inner.format.width as usize,
            self.inner.format.height as usize,
        );
        let format = unsafe { self.inner.transform.GetOutputCurrentType(self.inner.output) }
            .map_err(codec_error)?;
        let size = unsafe { format.GetUINT64(&MF_MT_FRAME_SIZE) }.map_err(codec_error)?;
        let (stored_w, stored_h) = ((size >> 32) as usize, size as u32 as usize);
        let stride = unsafe { format.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
            .map_or(stored_w, |stride| stride as usize);
        let surface = crate::yuv_video::Yuv420 {
            pixels: &data,
            width: w,
            height: h,
            stored_height: stored_h,
            stride,
            planar: false,
        };
        if !surface.write_rgba(rgba) {
            return Err(ProtoError::Codec(
                "The accelerated decoder returned an invalid pixel buffer.".into(),
            ));
        }
        *width = w as u32;
        *height = h as u32;
        Ok(true)
    }
}
