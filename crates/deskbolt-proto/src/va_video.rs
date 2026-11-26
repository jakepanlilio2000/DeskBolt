use std::ffi::{CString, c_char, c_void};
use std::ptr::{NonNull, null_mut};
use std::sync::OnceLock;

use libloading::Library;

use crate::video_pipeline::{
    VideoCapabilities, VideoCodec, VideoDecoder, VideoEncoder, VideoInput,
};
use crate::{MAX_DECOMPRESSED_PAYLOAD, ProtoError, VideoMsg};

type Result<T> = std::result::Result<T, ProtoError>;
type Handle = *mut c_void;

#[allow(clippy::type_complexity)]
struct Api {
    parse_launch: unsafe extern "C" fn(*const c_char, *mut Handle) -> Handle,
    find: unsafe extern "C" fn(*const c_char) -> Handle,
    unref: unsafe extern "C" fn(Handle),
    state: unsafe extern "C" fn(Handle, i32) -> i32,
    bin_get: unsafe extern "C" fn(Handle, *const c_char) -> Handle,
    new_buffer: unsafe extern "C" fn(Handle, usize, Handle) -> Handle,
    fill: unsafe extern "C" fn(Handle, usize, *const u8, usize) -> usize,
    extract: unsafe extern "C" fn(Handle, usize, *mut u8, usize) -> usize,
    buffer_flags: unsafe extern "C" fn(Handle) -> u32,
    size: unsafe extern "C" fn(Handle) -> usize,
    mini_unref: unsafe extern "C" fn(Handle),
    get_buffer: unsafe extern "C" fn(Handle) -> Handle,
    push: unsafe extern "C" fn(Handle, Handle) -> i32,
    pull: unsafe extern "C" fn(Handle, u64) -> Handle,
    _libraries: [Library; 2],
}

fn error() -> ProtoError {
    ProtoError::Codec("The system video accelerator is unavailable.".into())
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let core = Library::new("libgstreamer-1.0.so.0").ok()?;
        let app = Library::new("libgstapp-1.0.so.0").ok()?;
        let disable_scanner = core
            .get::<unsafe extern "C" fn(i32)>(b"gst_registry_fork_set_enabled\0")
            .ok()?;
        let initialize = core
            .get::<unsafe extern "C" fn(Handle, Handle, Handle) -> i32>(b"gst_init_check\0")
            .ok()?;
        // Load the platform codecs inside this process. The library's plugin scanner must never spawn.
        disable_scanner(0);
        if initialize(null_mut(), null_mut(), null_mut()) == 0 {
            return None;
        }
        macro_rules! load {
            ($lib:ident, $symbol:literal) => {
                *$lib.get(concat!($symbol, "\0").as_bytes()).ok()?
            };
        }
        Some(Api {
            parse_launch: load!(core, "gst_parse_launch"),
            find: load!(core, "gst_element_factory_find"),
            unref: load!(core, "gst_object_unref"),
            state: load!(core, "gst_element_set_state"),
            bin_get: load!(core, "gst_bin_get_by_name"),
            new_buffer: load!(core, "gst_buffer_new_allocate"),
            fill: load!(core, "gst_buffer_fill"),
            extract: load!(core, "gst_buffer_extract"),
            buffer_flags: load!(core, "gst_buffer_get_flags"),
            size: load!(core, "gst_buffer_get_size"),
            mini_unref: load!(core, "gst_mini_object_unref"),
            get_buffer: load!(core, "gst_sample_get_buffer"),
            push: load!(app, "gst_app_src_push_buffer"),
            pull: load!(app, "gst_app_sink_try_pull_sample"),
            _libraries: [core, app],
        })
    })
    .as_ref()
}

struct CodecSpec {
    encoders: &'static [&'static str],
    decoders: &'static [&'static str],
    parser: &'static str,
    caps: &'static str,
}

fn spec(codec: VideoCodec) -> Option<CodecSpec> {
    Some(match codec {
        VideoCodec::H264Hw => CodecSpec {
            encoders: &["vah264lpenc", "vah264enc"],
            decoders: &["vah264dec", "v4l2slh264dec"],
            parser: "h264parse config-interval=-1",
            caps: "video/x-h264,stream-format=byte-stream,alignment=au",
        },
        VideoCodec::HevcHw => CodecSpec {
            encoders: &["vah265lpenc", "vah265enc"],
            decoders: &["vah265dec", "v4l2slh265dec"],
            parser: "h265parse config-interval=-1",
            caps: "video/x-h265,stream-format=byte-stream,alignment=au",
        },
        VideoCodec::Av1Hw => CodecSpec {
            encoders: &["vaav1enc"],
            decoders: &["vaav1dec", "v4l2slav1dec"],
            parser: "av1parse",
            caps: "video/x-av1,stream-format=obu-stream,alignment=tu",
        },
        _ => return None,
    })
}

fn present(api: &Api, name: &str) -> bool {
    let name = CString::new(name).unwrap();
    unsafe {
        let factory = (api.find)(name.as_ptr());
        if factory.is_null() {
            false
        } else {
            (api.unref)(factory);
            true
        }
    }
}

pub fn decoders() -> VideoCapabilities {
    let mut caps = VideoCapabilities::default();
    if let Some(api) = api() {
        for codec in VideoCodec::HARDWARE {
            if spec(codec)
                .unwrap()
                .decoders
                .iter()
                .any(|name| present(api, name))
            {
                caps.add(codec);
            }
        }
    }
    caps
}

struct Object(NonNull<c_void>, &'static Api);

impl Object {
    fn new(value: Handle, api: &'static Api) -> Result<Self> {
        NonNull::new(value)
            .map(|ptr| Self(ptr, api))
            .ok_or_else(error)
    }

    fn ptr(&self) -> Handle {
        self.0.as_ptr()
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        unsafe { (self.1.unref)(self.ptr()) }
    }
}

struct Sample(NonNull<c_void>, &'static Api);

impl Drop for Sample {
    fn drop(&mut self) {
        unsafe { (self.1.mini_unref)(self.0.as_ptr()) }
    }
}

struct Pipeline {
    pipeline: Object,
    source: Object,
    sink: Object,
    empty: u8,
}

impl Pipeline {
    fn open(description: &str) -> Result<Self> {
        let api = api().ok_or_else(error)?;
        let description = CString::new(description).map_err(|_| error())?;
        unsafe {
            let pipeline = Object::new((api.parse_launch)(description.as_ptr(), null_mut()), api)?;
            let source = Object::new((api.bin_get)(pipeline.ptr(), c"input".as_ptr()), api)?;
            let sink = Object::new((api.bin_get)(pipeline.ptr(), c"result".as_ptr()), api)?;
            let stream = Self {
                pipeline,
                source,
                sink,
                empty: 0,
            };
            if (api.state)(stream.pipeline.ptr(), 4) == 0 {
                return Err(error());
            }
            Ok(stream)
        }
    }

    fn push_and_pull(
        &mut self,
        bytes: &[u8],
        max_output: usize,
    ) -> Result<Option<(Vec<u8>, bool)>> {
        let api = self.pipeline.1;
        unsafe {
            let input = (api.new_buffer)(null_mut(), bytes.len(), null_mut());
            if input.is_null() {
                return Err(error());
            }
            if (api.fill)(input, 0, bytes.as_ptr(), bytes.len()) != bytes.len() {
                (api.mini_unref)(input);
                return Err(error());
            }
            // The source takes ownership even if the flow status reports failure.
            if (api.push)(self.source.ptr(), input) != 0 {
                return Err(error());
            }
            let Some(sample) = NonNull::new((api.pull)(self.sink.ptr(), 40_000_000)) else {
                self.empty += 1;
                return if self.empty >= 4 {
                    Err(error())
                } else {
                    Ok(None)
                };
            };
            let sample = Sample(sample, api);
            let buffer = (api.get_buffer)(sample.0.as_ptr());
            if buffer.is_null() {
                return Err(error());
            }
            let length = (api.size)(buffer);
            if length == 0 || length > max_output {
                return Err(error());
            }
            let mut data = vec![0u8; length];
            if (api.extract)(buffer, 0, data.as_mut_ptr(), length) != length {
                return Err(error());
            }
            self.empty = 0;
            Ok(Some((data, (api.buffer_flags)(buffer) & 8192 == 0)))
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        unsafe { (self.pipeline.1.state)(self.pipeline.ptr(), 1) };
    }
}

const SOURCE: &str = "appsrc name=input is-live=true do-timestamp=true format=time max-bytes=0 max-buffers=4 leaky-type=upstream";
const SINK: &str = "appsink name=result sync=false async=false max-buffers=2 drop=true";

#[derive(Clone, Copy, PartialEq, Eq)]
struct Config(u32, u32, u8, u32);

impl From<VideoInput<'_>> for Config {
    fn from(input: VideoInput<'_>) -> Self {
        Self(input.width, input.height, input.fps, input.bitrate)
    }
}

pub struct Encoder {
    pipeline: Pipeline,
    codec: VideoCodec,
    config: Config,
    started: bool,
}

impl Encoder {
    pub fn open(codec: VideoCodec, input: VideoInput<'_>) -> Result<Self> {
        let spec = spec(codec).ok_or_else(error)?;
        let api = api().ok_or_else(error)?;
        validate_size(input.width, input.height)?;
        for element in spec.encoders.iter().filter(|name| present(api, name)) {
            let rgba = format!(
                "video/x-raw,format=BGRA,width={},height={},framerate={}/1",
                input.width, input.height, input.fps
            );
            let bitrate = input.bitrate / 1000;
            let enc_options = if codec == VideoCodec::Av1Hw {
                ""
            } else {
                " b-frames=0"
            };
            let pipeline = format!(
                "{SOURCE} caps={rgba} ! videoconvert ! video/x-raw,format=NV12 ! {element} bitrate={bitrate}{enc_options} ! {} ! {} ! {SINK}",
                spec.parser, spec.caps
            );
            if let Ok(pipeline) = Pipeline::open(&pipeline) {
                return Ok(Self {
                    pipeline,
                    codec,
                    config: input.into(),
                    started: false,
                });
            }
        }
        Err(error())
    }
}

impl VideoEncoder for Encoder {
    fn codec(&self) -> VideoCodec {
        self.codec
    }

    fn encode(&mut self, input: VideoInput<'_>) -> Result<Option<VideoMsg>> {
        if self.config != input.into() || self.started && input.refresh {
            *self = Self::open(self.codec, input)?;
        }
        if input.bgra.len() != (input.width * input.height * 4) as usize {
            return Err(error());
        }
        let Some((access_unit, keyframe)) = self
            .pipeline
            .push_and_pull(input.bgra, MAX_DECOMPRESSED_PAYLOAD)?
        else {
            return Ok(None);
        };
        if !self.started && !keyframe {
            return Err(error());
        }
        self.started = true;
        Ok(Some(input.packet(self.codec, keyframe, access_unit)))
    }
}

pub struct Decoder {
    pipeline: Pipeline,
    width: u32,
    height: u32,
}

impl Decoder {
    pub fn open(codec: VideoCodec, width: u16, height: u16) -> Result<Self> {
        let spec = spec(codec).ok_or_else(error)?;
        let api = api().ok_or_else(error)?;
        let (width, height) = (u32::from(width), u32::from(height));
        validate_size(width, height)?;
        for decoder in spec.decoders.iter().filter(|name| present(api, name)) {
            let desc = format!(
                "{SOURCE} caps={} ! {decoder} ! videoconvert ! video/x-raw,format=RGBA,width={width},height={height} ! {SINK}",
                spec.caps
            );
            if let Ok(pipeline) = Pipeline::open(&desc) {
                return Ok(Self {
                    pipeline,
                    width,
                    height,
                });
            }
        }
        Err(error())
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
        let expected = (self.width * self.height * 4) as usize;
        let Some((decoded, _)) = self.pipeline.push_and_pull(bytes, expected)? else {
            return Ok(false);
        };
        if decoded.len() != expected {
            return Err(error());
        }
        *rgba = decoded;
        (*width, *height) = (self.width, self.height);
        Ok(true)
    }
}

fn validate_size(width: u32, height: u32) -> Result<()> {
    if !(16..=8192).contains(&width)
        || !(16..=8192).contains(&height)
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
    {
        return Err(error());
    }
    Ok(())
}
