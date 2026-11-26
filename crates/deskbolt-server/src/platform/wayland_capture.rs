use std::collections::HashMap;
use std::io::Cursor;
use std::mem::size_of;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
    mpsc,
};
use std::time::Duration;

use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType, Stream,
};
use deskbolt_proto::{CursorShape, CursorSnapshot, MAX_CURSOR_SIDE, MonitorInfo};
use pipewire::{
    context::ContextRc,
    keys::{MEDIA_CATEGORY, MEDIA_ROLE, MEDIA_TYPE},
    main_loop::MainLoopRc,
    properties,
    spa::{
        param::{
            ParamType,
            format::{FormatProperties, MediaSubtype, MediaType},
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::{self, Object, Pod, Property, Value, serialize::PodSerializer},
        sys as spa,
        utils::{Direction, Id, Rectangle, SpaTypes},
    },
    stream::{StreamFlags, StreamRc, StreamState},
};

use super::{CaptureOutput, RawCapturedFrame, ScreenCapturer, diff_bgra_dirty_rects};

const CURSOR_META_SIZE: usize = size_of::<spa::spa_meta_cursor>()
    + size_of::<spa::spa_meta_bitmap>()
    + MAX_CURSOR_SIDE as usize * MAX_CURSOR_SIDE as usize * 4;
const MAX_SCREEN_SIDE: u32 = 8192;

pub fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
}

struct FramePixels {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

struct Display {
    info: MonitorInfo,
    pending: Option<FramePixels>,
    cursor_shape: Option<CursorShape>,
    cursor: Option<CursorSnapshot>,
}

#[derive(Default)]
struct CaptureState {
    displays: Vec<Display>,
    failure: Option<String>,
}

pub struct WaylandCapturer {
    state: Arc<Mutex<CaptureState>>,
    active: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    previous: HashMap<u8, FramePixels>,
    group: Arc<AtomicBool>,
}

impl WaylandCapturer {
    pub fn new(monitor_id: u8, request_virtual: bool) -> Result<Self, String> {
        let capture = Self {
            state: Arc::default(),
            active: Arc::new(AtomicU8::new(monitor_id)),
            stop: Arc::default(),
            previous: HashMap::new(),
            group: Arc::default(),
        };
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let state = capture.state.clone();
        let stop = capture.stop.clone();
        let active = capture.active.clone();
        let group = capture.group.clone();
        std::thread::spawn(move || {
            if let Err(error) = run_streams(&state, active, group, stop, request_virtual, &ready_tx)
            {
                state.lock().unwrap().failure = Some(error.clone());
                let _ = ready_tx.try_send(Err(error));
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(35))
            .map_err(|_| "Screen sharing timed out.")??;
        Ok(capture)
    }
}

impl Drop for WaylandCapturer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl ScreenCapturer for WaylandCapturer {
    fn monitors(&self) -> Vec<MonitorInfo> {
        self.state
            .lock()
            .unwrap()
            .displays
            .iter()
            .map(|display| display.info.clone())
            .collect()
    }

    fn set_monitor(&mut self, monitor_id: u8) {
        self.active.store(monitor_id, Ordering::Relaxed);
    }

    fn set_workspace_capture(&mut self, enabled: bool) -> bool {
        self.group.store(enabled, Ordering::Relaxed);
        true
    }

    fn capture_next(&mut self, force_full: bool, _timeout_ms: u32) -> CaptureOutput {
        let monitor_id = self.active.load(Ordering::Relaxed);
        let pending = {
            let mut state = self.state.lock().unwrap();
            if let Some(failure) = &state.failure {
                return CaptureOutput::Reacquire(failure.clone());
            }
            state
                .displays
                .get_mut(usize::from(monitor_id))
                .and_then(|display| display.pending.take())
        };
        let previous = self.previous.get(&monitor_id);
        let Some(pixels) = pending.or_else(|| {
            force_full
                .then(|| {
                    previous.map(|p| FramePixels {
                        width: p.width,
                        height: p.height,
                        bgra: p.bgra.clone(),
                    })
                })
                .flatten()
        }) else {
            return CaptureOutput::Timeout;
        };
        let full = force_full
            || previous.is_none_or(|p| (p.width, p.height) != (pixels.width, pixels.height));
        let dirty_rects = if full {
            Vec::new()
        } else {
            diff_bgra_dirty_rects(
                &previous.unwrap().bgra,
                &pixels.bgra,
                pixels.width,
                pixels.height,
                64,
            )
        };
        if !full && dirty_rects.is_empty() {
            return CaptureOutput::Timeout;
        }
        let raw = RawCapturedFrame {
            monitor_id,
            width: pixels.width,
            height: pixels.height,
            bgra: pixels.bgra.clone(),
            move_rects: Vec::new(),
            dirty_rects,
            force_full: full,
        };
        self.previous.insert(monitor_id, pixels);
        CaptureOutput::Frame(raw)
    }

    fn capture_cursor(&mut self) -> Option<CursorSnapshot> {
        self.state
            .lock()
            .unwrap()
            .displays
            .get(usize::from(self.active.load(Ordering::Relaxed)))?
            .cursor
            .clone()
    }
}

fn run_streams(
    state: &Arc<Mutex<CaptureState>>,
    active: Arc<AtomicU8>,
    group: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    request_virtual: bool,
    ready: &mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let (session, streams, fd, cursor_mode) = rt
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(30), async {
                let portal = Screencast::new().await?;
                let session = portal.create_session(Default::default()).await?;
                let supported = portal.available_source_types().await?;
                let mut sources = SourceType::MONITOR & supported;
                if request_virtual && supported.contains(SourceType::VIRTUAL) {
                    sources |= SourceType::VIRTUAL;
                }
                let modes = portal
                    .available_cursor_modes()
                    .await
                    .unwrap_or(CursorMode::HIDDEN);
                let cursor_mode = if modes.contains(CursorMode::METADATA) {
                    CursorMode::METADATA
                } else if modes.contains(CursorMode::EMBEDDED) {
                    CursorMode::EMBEDDED
                } else {
                    CursorMode::HIDDEN
                };
                portal
                    .select_sources(
                        &session,
                        SelectSourcesOptions::default()
                            .set_sources(sources)
                            .set_multiple(true)
                            .set_cursor_mode(cursor_mode),
                    )
                    .await?;
                let response = portal
                    .start(&session, None, Default::default())
                    .await?
                    .response()?;
                let fd = portal
                    .open_pipe_wire_remote(&session, Default::default())
                    .await?;
                Ok::<_, ashpd::Error>((session, response.streams().to_vec(), fd, cursor_mode))
            })
            .await
        })
        .map_err(|_| "Screen sharing timed out.".to_owned())?
        .map_err(|e| e.to_string())?;
    if streams.is_empty() || streams.len() > usize::from(u8::MAX) {
        return Err("No displays were selected.".into());
    }
    state.lock().unwrap().displays = streams
        .iter()
        .enumerate()
        .map(|(index, stream)| Display {
            info: stream_monitor(index, stream),
            pending: None,
            cursor: None,
            cursor_shape: None,
        })
        .collect();
    active.fetch_min((streams.len() - 1) as u8, Ordering::Relaxed);

    pipewire::init();
    let main_loop = MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = ContextRc::new(&main_loop, None).map_err(|e| e.to_string())?;
    let core = context.connect_fd_rc(fd, None).map_err(|e| e.to_string())?;
    let loop_ref = main_loop.clone();
    let stop_timer = main_loop.loop_().add_timer(move |_| {
        if stop.load(Ordering::Relaxed) {
            loop_ref.quit();
        }
    });
    stop_timer
        .update_timer(
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(100)),
        )
        .into_result()
        .map_err(|e| e.to_string())?;
    let mut connections = Vec::new();
    for (index, stream) in streams.iter().enumerate() {
        let pw_stream = StreamRc::new(
            core.clone(),
            "DeskBolt",
            properties::properties! {
                *MEDIA_TYPE => "Video", *MEDIA_CATEGORY => "Capture", *MEDIA_ROLE => "Screen",
            },
        )
        .map_err(|e| e.to_string())?;
        let state = state.clone();
        let failure = state.clone();
        let active = active.clone();
        let group = group.clone();
        let validate = cursor_mode == CursorMode::METADATA;
        let listener = pw_stream
            .add_local_listener_with_user_data(VideoInfoRaw::default())
            .state_changed(move |_, _, _, new| {
                if let StreamState::Error(error) = new {
                    failure.lock().unwrap().failure = Some(error);
                }
            })
            .param_changed(move |pw, info, id, param| {
                if id == ParamType::Format.as_raw()
                    && let Some(param) = param
                    && info.parse(param).is_ok()
                    && validate
                    && let Ok(bytes) = cursor_meta_param()
                    && let Some(meta) = Pod::from_bytes(&bytes)
                {
                    let _ = pw.update_params(&mut [meta]);
                }
            })
            .process(move |pw, info| {
                let mut state = state.lock().unwrap();
                let display = &mut state.displays[index];
                // SAFETY: Process owns the dequeued buffer until it is returned to this stream.
                unsafe {
                    let raw = pw.dequeue_raw_buffer();
                    if let Some(buffer) = raw.as_ref().and_then(|raw| raw.buffer.as_ref()) {
                        if validate {
                            read_cursor(buffer, display);
                        }
                        if (group.load(Ordering::Relaxed)
                            || active.load(Ordering::Relaxed) == index as u8)
                            && let Some(frame) = read_pixels(buffer, info)
                        {
                            if display.info.width == 0 || display.info.height == 0 {
                                (display.info.width, display.info.height) =
                                    (frame.width, frame.height);
                            }
                            display.pending = Some(frame);
                        }
                    }
                    if !raw.is_null() {
                        pw.queue_raw_buffer(raw);
                    }
                }
            })
            .register()
            .map_err(|e| e.to_string())?;
        let bytes = video_param()?;
        let format = Pod::from_bytes(&bytes).ok_or("Invalid screen stream format.")?;
        pw_stream
            .connect(
                Direction::Input,
                Some(stream.pipe_wire_node_id()),
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                &mut [format],
            )
            .map_err(|e| e.to_string())?;
        connections.push((listener, pw_stream));
    }
    ready
        .send(Ok(()))
        .map_err(|_| "Screen sharing was cancelled.")?;
    main_loop.run();
    drop(connections);
    let _ =
        rt.block_on(async { tokio::time::timeout(Duration::from_secs(1), session.close()).await });
    Ok(())
}

fn stream_monitor(index: usize, stream: &Stream) -> MonitorInfo {
    let (left, top) = stream.position().unwrap_or_default();
    let (width, height) = stream.size().unwrap_or_default();
    MonitorInfo {
        id: index as u8,
        name: format!("Display {}", index + 1),
        is_primary: index == 0,
        left,
        top,
        width: width.max(0) as u32,
        height: height.max(0) as u32,
        dpi_scale: 1.0,
    }
}

fn video_param() -> Result<Vec<u8>, String> {
    let object = pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: MAX_SCREEN_SIDE,
                height: MAX_SCREEN_SIDE
            }
        ),
    );
    serialize(object)
}

fn cursor_meta_param() -> Result<Vec<u8>, String> {
    serialize(Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(
                spa::SPA_PARAM_META_type,
                Value::Id(Id(spa::SPA_META_Cursor)),
            ),
            Property::new(
                spa::SPA_PARAM_META_size,
                Value::Int(CURSOR_META_SIZE as i32),
            ),
        ],
    })
}

fn serialize(object: Object) -> Result<Vec<u8>, String> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map(|(buffer, _)| buffer.into_inner())
        .map_err(|e| e.to_string())
}

unsafe fn read_pixels(buffer: &spa::spa_buffer, info: &VideoInfoRaw) -> Option<FramePixels> {
    let (width, height) = (info.size().width, info.size().height);
    if width == 0
        || height == 0
        || width > MAX_SCREEN_SIDE
        || height > MAX_SCREEN_SIDE
        || buffer.n_datas == 0
    {
        return None;
    }
    // SAFETY: The producer owns the mapped plane and chunk for the duration of this callback.
    let (data, chunk) = unsafe {
        let data = buffer.datas.as_ref()?;
        (data, data.chunk.as_ref()?)
    };
    let row = width as usize * 4;
    let stride = usize::try_from(chunk.stride).ok()?.max(row);
    let offset = chunk.offset as usize;
    let len = stride.checked_mul(height as usize - 1)?.checked_add(row)?;
    if data.data.is_null()
        || len > chunk.size as usize
        || offset.checked_add(len)? > data.maxsize as usize
    {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data.data.cast::<u8>().add(offset), len) };
    let swap_red_blue = match info.format() {
        VideoFormat::BGRA | VideoFormat::BGRx => false,
        VideoFormat::RGBA | VideoFormat::RGBx => true,
        _ => return None,
    };
    let mut bgra = Vec::with_capacity(row * height as usize);
    for row in bytes.chunks(stride) {
        bgra.extend_from_slice(&row[..width as usize * 4]);
    }
    for pixel in bgra.as_chunks_mut::<4>().0 {
        if swap_red_blue {
            pixel.swap(0, 2);
        }
        pixel[3] = 255;
    }
    Some(FramePixels {
        width,
        height,
        bgra,
    })
}

unsafe fn read_cursor(buffer: &spa::spa_buffer, display: &mut Display) {
    if buffer.n_metas == 0 || buffer.n_metas > 64 || buffer.metas.is_null() {
        return;
    }
    // SAFETY: The buffer's metadata descriptors and allocations remain live until it is queued.
    let metas = unsafe { std::slice::from_raw_parts(buffer.metas, buffer.n_metas as usize) };
    let Some(meta) = metas.iter().find(|meta| meta.type_ == spa::SPA_META_Cursor) else {
        return;
    };
    if meta.data.is_null() {
        return;
    }
    let bytes = unsafe {
        std::slice::from_raw_parts(
            meta.data.cast::<u8>(),
            (meta.size as usize).min(CURSOR_META_SIZE),
        )
    };
    let Some(cursor) = read_meta::<spa::spa_meta_cursor>(bytes, 0) else {
        return;
    };
    if cursor.id == 0 {
        return;
    }
    if cursor.bitmap_offset != 0 {
        display.cursor_shape = cursor_bitmap(bytes, &cursor);
    }
    let (x, y) = (cursor.position.x, cursor.position.y);
    display.cursor = if x < 0 || y < 0 {
        None
    } else {
        display.cursor_shape.as_ref().and_then(|shape| {
            Some(CursorSnapshot {
                x: x.try_into().ok()?,
                y: y.try_into().ok()?,
                shape: shape.clone(),
            })
        })
    };
}

fn cursor_bitmap(bytes: &[u8], cursor: &spa::spa_meta_cursor) -> Option<CursorShape> {
    let offset = cursor.bitmap_offset as usize;
    if offset < size_of::<spa::spa_meta_cursor>() {
        return None;
    }
    let bitmap = read_meta::<spa::spa_meta_bitmap>(bytes, offset)?;
    let width = u16::try_from(bitmap.size.width).ok()?;
    let height = u16::try_from(bitmap.size.height).ok()?;
    let hotspot_x = u16::try_from(cursor.hotspot.x).ok()?;
    let hotspot_y = u16::try_from(cursor.hotspot.y).ok()?;
    if width > MAX_CURSOR_SIDE
        || height > MAX_CURSOR_SIDE
        || hotspot_x >= width
        || hotspot_y >= height
        || (bitmap.offset as usize) < size_of::<spa::spa_meta_bitmap>()
    {
        return None;
    }
    let row = usize::from(width) * 4;
    let stride = usize::try_from(bitmap.stride).ok()?;
    if stride < row {
        return None;
    }
    let start = offset.checked_add(bitmap.offset as usize)?;
    let end = start.checked_add(
        stride
            .checked_mul(usize::from(height) - 1)?
            .checked_add(row)?,
    )?;
    let swap_red_blue = match VideoFormat::from_raw(bitmap.format) {
        VideoFormat::BGRA => true,
        VideoFormat::RGBA => false,
        _ => return None,
    };
    let mut rgba = Vec::with_capacity(row * usize::from(height));
    for line in bytes.get(start..end)?.chunks(stride) {
        rgba.extend_from_slice(&line[..row]);
    }
    if swap_red_blue {
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
    }
    Some(CursorShape {
        width,
        height,
        hotspot_x,
        hotspot_y,
        rgba,
    })
}

fn read_meta<T: Copy>(bytes: &[u8], offset: usize) -> Option<T> {
    let data = bytes.get(offset..offset.checked_add(size_of::<T>())?)?;
    // SAFETY: The local caller uses plain numeric metadata records, and the slice holds the full record.
    Some(unsafe { data.as_ptr().cast::<T>().read_unaligned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_bitmap_checks_offsets_and_rows() {
        let header_len = size_of::<spa::spa_meta_cursor>();
        let bitmap_len = size_of::<spa::spa_meta_bitmap>();
        let mut cursor: spa::spa_meta_cursor = unsafe { std::mem::zeroed() };
        cursor.bitmap_offset = header_len as u32;
        let mut bitmap: spa::spa_meta_bitmap = unsafe { std::mem::zeroed() };
        bitmap.format = VideoFormat::BGRA.as_raw();
        bitmap.size.width = 1;
        bitmap.size.height = 2;
        bitmap.stride = 8;
        bitmap.offset = bitmap_len as u32;
        let start = header_len + bitmap_len;
        let mut bytes = vec![0u8; start + 12];
        // SAFETY: The byte vector has room for the entire metadata header.
        unsafe {
            bytes
                .as_mut_ptr()
                .add(header_len)
                .cast::<spa::spa_meta_bitmap>()
                .write_unaligned(bitmap);
        }
        bytes[start..start + 4].copy_from_slice(&[12, 34, 56, 255]);
        bytes[start + 8..start + 12].copy_from_slice(&[1, 2, 3, 255]);
        assert_eq!(
            cursor_bitmap(&bytes, &cursor).unwrap().rgba,
            [56, 34, 12, 255, 3, 2, 1, 255]
        );
        assert!(cursor_bitmap(&bytes[..bytes.len() - 1], &cursor).is_none());
        cursor.bitmap_offset = u32::MAX;
        assert!(cursor_bitmap(&bytes, &cursor).is_none());
    }
}
