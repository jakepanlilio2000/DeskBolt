use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deskbolt_proto::{
    CursorDelta, DesktopState, DirtyTile, H264VideoEncoder, InputMsg, IpcMsg, MonitorInfo, TileRect, VideoMsg,
    VideoQualityMode, encode_dirty_tile,
};
#[cfg(target_os = "windows")]
use deskbolt_proto::{
    ChannelId, FLAG_KEYFRAME, FLAG_NONE, pack_message, read_frame_async, unpack_message,
    write_frame_async,
};
#[cfg(not(target_os = "android"))]
use deskbolt_proto::{ClipboardSync, DEFAULT_CLIPBOARD_MAX_BYTES, native_clipboard::NativeClipboard};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::warn;

#[cfg(target_os = "windows")]
use crate::ipc::connect_ipc_client;
use crate::platform::{
    self, CaptureOutput, RawCapturedFrame, ScreenCapturer, bind_active_input_desktop,
    enumerate_monitors,
};

/// Runs `--agent`: connects over `\\.\pipe\DeskBolt_IPC` to the Session-0 service on Windows.
#[cfg(target_os = "windows")]
pub async fn run_agent_process(pipe_name: &str) -> Result<(), String> {
    let client = connect_ipc_client(pipe_name).await?;
    let (mut pipe_rx, mut pipe_tx) = tokio::io::split(client);

    let (cmd_tx, cmd_rx) = mpsc::channel::<IpcMsg>(256);
    let (ev_tx, mut ev_rx) = mpsc::channel::<IpcMsg>(64);
    let stop = Arc::new(AtomicBool::new(false));

    let worker_stop = stop.clone();
    let worker_handle = thread::spawn(move || run_agent_workers(cmd_rx, ev_tx, worker_stop));

    let write_task = tokio::spawn(async move {
        while let Some(msg) = ev_rx.recv().await {
            let flags = match &msg {
                IpcMsg::Video(VideoMsg::H264Frame { is_keyframe: true, .. })
                | IpcMsg::Video(VideoMsg::TileBatch { is_full_refresh: true, .. }) => FLAG_KEYFRAME,
                _ => FLAG_NONE,
            };
            if let Ok(frame) = pack_message(ChannelId::AuthControl, flags, &msg, None)
                && write_frame_async(&mut pipe_tx, frame.channel, frame.flags, &frame.payload)
                    .await
                    .is_err()
            {
                break;
            }
        }
    });

    while let Ok(Some(frame)) = read_frame_async(&mut pipe_rx).await {
        if let Ok(msg) = unpack_message::<IpcMsg>(&frame) {
            if matches!(msg, IpcMsg::Shutdown) {
                break;
            }
            if cmd_tx.send(msg).await.is_err() {
                break;
            }
        }
    }

    stop.store(true, Ordering::SeqCst);
    drop(cmd_tx);
    write_task.abort();
    let _ = worker_handle.join();
    Ok(())
}

/// Runs the desktop capture/input engine in-process (used by `--console`, Linux/macOS `--service`, and Android host).
pub async fn run_inprocess_agent(
    mut to_agent_rx: mpsc::Receiver<IpcMsg>,
    from_agent_tx: broadcast::Sender<Arc<IpcMsg>>,
    monitors_tx: watch::Sender<(Vec<MonitorInfo>, u8)>,
    mut activity_rx: mpsc::UnboundedReceiver<crate::ipc::ViewerActivity>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<IpcMsg>(256);
    let (ev_tx, mut ev_rx) = mpsc::channel::<IpcMsg>(64);
    let stop = Arc::new(AtomicBool::new(false));

    let worker_stop = stop.clone();
    let worker = thread::spawn(move || run_agent_workers(cmd_rx, ev_tx, worker_stop));

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            Some(_) = activity_rx.recv() => {}
            Some(cmd) = to_agent_rx.recv() => {
                let _ = cmd_tx.send(cmd).await;
            }
            Some(ev) = ev_rx.recv() => {
                match &ev {
                    IpcMsg::AgentReady { monitors, active_monitor_id, .. }
                    | IpcMsg::MonitorList { monitors, active_monitor_id } => {
                        let _ = monitors_tx.send((monitors.clone(), *active_monitor_id));
                    }
                    _ => {}
                }
                let _ = from_agent_tx.send(Arc::new(ev));
            }
        }
    }

    stop.store(true, Ordering::SeqCst);
    let _ = cmd_tx.send(IpcMsg::Shutdown).await;
    drop(cmd_tx);
    let _ = worker.join();
}

struct SharedAgentState {
    monitors: Mutex<Vec<MonitorInfo>>,
    active_monitor: Mutex<u8>,
    quality_mode: Mutex<VideoQualityMode>,
    target_fps: Mutex<u8>,
    bitrate_kbps: Mutex<u32>,
    force_keyframe: AtomicBool,
    desktop_switched: AtomicBool,
    capture_cursor: AtomicBool,
}

struct LatestRawSlot {
    data: Mutex<Option<RawCapturedFrame>>,
    cv: Condvar,
}

fn run_agent_workers(
    mut cmd_rx: mpsc::Receiver<IpcMsg>,
    ev_tx: mpsc::Sender<IpcMsg>,
    stop: Arc<AtomicBool>,
) {
    let monitors = enumerate_monitors();
    let initial_mon = monitors
        .iter()
        .find(|m| m.is_primary)
        .map(|m| m.id)
        .unwrap_or(0);

    let shared = Arc::new(SharedAgentState {
        monitors: Mutex::new(monitors.clone()),
        active_monitor: Mutex::new(initial_mon),
        quality_mode: Mutex::new(VideoQualityMode::Auto),
        target_fps: Mutex::new(30),
        bitrate_kbps: Mutex::new(4000),
        force_keyframe: AtomicBool::new(true),
        desktop_switched: AtomicBool::new(true),
        capture_cursor: AtomicBool::new(true),
    });

    let _ = ev_tx.blocking_send(IpcMsg::AgentReady {
        session_id: 0,
        monitors,
        active_monitor_id: initial_mon,
    });

    let slot = Arc::new(LatestRawSlot {
        data: Mutex::new(None),
        cv: Condvar::new(),
    });

    let (input_tx, input_rx) = std::sync::mpsc::channel::<InputMsg>();
    #[cfg(not(target_os = "android"))]
    let (clip_tx, clip_rx) = std::sync::mpsc::channel();

    let cap_stop = stop.clone();
    let cap_shared = shared.clone();
    let cap_slot = slot.clone();
    let cap_ev = ev_tx.clone();
    let cap_handle = thread::spawn(move || capture_loop(cap_shared, cap_slot, cap_ev, cap_stop));

    let enc_stop = stop.clone();
    let enc_shared = shared.clone();
    let enc_slot = slot.clone();
    let enc_ev = ev_tx.clone();
    let enc_handle = thread::spawn(move || encode_loop(enc_shared, enc_slot, enc_ev, enc_stop));

    let inp_stop = stop.clone();
    let inp_shared = shared.clone();
    let inp_handle = thread::spawn(move || input_loop(inp_shared, input_rx, inp_stop));

    #[cfg(not(target_os = "android"))]
    let clip_handle = {
        let clip_stop = stop.clone();
        let clip_ev = ev_tx.clone();
        thread::spawn(move || run_clipboard_loop(clip_rx, clip_ev, clip_stop))
    };

    while let Some(cmd) = cmd_rx.blocking_recv() {
        match cmd {
            IpcMsg::Input(inp) => {
                let _ = input_tx.send(inp);
            }
            #[cfg(not(target_os = "android"))]
            clip @ (IpcMsg::Clipboard(_) | IpcMsg::SetClipboardLimit(_)) => {
                let _ = clip_tx.send(clip);
            }
            #[cfg(target_os = "android")]
            IpcMsg::Clipboard(clip) => platform::android::write_clipboard(clip),
            #[cfg(target_os = "android")]
            IpcMsg::SetClipboardLimit(bytes) => platform::android::set_clipboard_limit(bytes as usize),
            IpcMsg::QualityPreset {
                mode,
                target_fps,
                h264_bitrate_kbps,
            } => {
                *shared.quality_mode.lock().unwrap() = mode;
                *shared.target_fps.lock().unwrap() = target_fps.clamp(15, 120);
                *shared.bitrate_kbps.lock().unwrap() = h264_bitrate_kbps.max(250);
                shared.force_keyframe.store(true, Ordering::SeqCst);
            }
            IpcMsg::MonitorSelect { monitor_id } => {
                *shared.active_monitor.lock().unwrap() = monitor_id;
                shared.force_keyframe.store(true, Ordering::SeqCst);
            }
            IpcMsg::KeyframeRequest { .. } => {
                shared.force_keyframe.store(true, Ordering::SeqCst);
            }
            IpcMsg::SetCurtainMode(enabled) => {
                let _ = platform::set_curtain_mode(enabled);
            }
            IpcMsg::SetCaptureCursor(enabled) => {
                shared.capture_cursor.store(enabled, Ordering::Relaxed);
            }
            IpcMsg::ShowPopup {
                title,
                body,
                urgent,
            } => {
                let _ = platform::show_desktop_popup(&title, &body, urgent);
            }
            IpcMsg::Shutdown => break,
            _ => {}
        }
    }

    stop.store(true, Ordering::SeqCst);
    slot.cv.notify_all();
    drop(input_tx);
    let _ = cap_handle.join();
    let _ = enc_handle.join();
    let _ = inp_handle.join();
    #[cfg(not(target_os = "android"))]
    {
        drop(clip_tx);
        let _ = clip_handle.join();
    }
}

#[cfg(not(target_os = "android"))]
fn run_clipboard_loop(
    rx: std::sync::mpsc::Receiver<IpcMsg>,
    events: mpsc::Sender<IpcMsg>,
    stop: Arc<AtomicBool>,
) {
    let _ = bind_active_input_desktop();
    let mut clipboard = None;
    let mut previous = None;
    let mut initialized = false;
    let mut limit = DEFAULT_CLIPBOARD_MAX_BYTES;
    let mut seq = 0;
    while !stop.load(Ordering::Relaxed) {
        let command = rx.recv_timeout(Duration::from_millis(250)).ok();
        if let Some(IpcMsg::SetClipboardLimit(bytes)) = command {
            limit = bytes as usize;
            continue;
        }
        if clipboard.is_none() {
            clipboard = NativeClipboard::open();
        }
        let Some(clipboard) = clipboard.as_mut() else { continue };
        if let Some(IpcMsg::Clipboard(message)) = command
            && clipboard.write(&message, limit)
        {
            previous = clipboard.read(limit);
            initialized = true;
            continue;
        }
        let current = clipboard.read(limit);
        if initialized && current != previous
            && let Some(data) = current.as_ref()
        {
            seq += 1;
            if events.blocking_send(IpcMsg::Clipboard(ClipboardSync::set(seq, data.clone()))).is_err() {
                break;
            }
        }
        previous = current;
        initialized = true;
    }
}

fn capture_loop(
    shared: Arc<SharedAgentState>,
    slot: Arc<LatestRawSlot>,
    ev_tx: mpsc::Sender<IpcMsg>,
    stop: Arc<AtomicBool>,
) {
    let mut current_desktop = bind_active_input_desktop().unwrap_or_else(|_| "Default".into());
    emit_desktop_status(&ev_tx, &current_desktop);

    let mut capturer: Option<Box<dyn ScreenCapturer>> = None;
    let mut current_mon_idx: u8 = *shared.active_monitor.lock().unwrap();
    let mut last_dims: (u32, u32) = (0, 0);
    let mut need_full = true;
    let mut cursor_delta = CursorDelta::default();

    while !stop.load(Ordering::SeqCst) {
        let wanted_mon = *shared.active_monitor.lock().unwrap();
        let fps = *shared.target_fps.lock().unwrap();
        let frame_interval = Duration::from_millis((1000 / fps.max(1) as u64).max(8));
        let loop_start = Instant::now();

        if capturer.is_none() {
            match platform::create_capturer(wanted_mon) {
                Ok(mut cap) => {
                    let desk = cap.current_desktop_name();
                    if desk != current_desktop {
                        current_desktop = desk;
                        shared.desktop_switched.store(true, Ordering::SeqCst);
                        emit_desktop_status(&ev_tx, &current_desktop);
                    }
                    let mons = cap.monitors();
                    let selected = mons.iter().find(|monitor| monitor.id == wanted_mon)
                        .or_else(|| mons.first()).map_or(wanted_mon, |monitor| monitor.id);
                    if !mons.is_empty() {
                        *shared.monitors.lock().unwrap() = mons.clone();
                        if selected != wanted_mon {
                            let mut active = shared.active_monitor.lock().unwrap();
                            if *active == wanted_mon {
                                *active = selected;
                            }
                            cap.set_monitor(selected);
                        }
                        let _ = ev_tx.blocking_send(IpcMsg::MonitorList {
                            monitors: mons,
                            active_monitor_id: selected,
                        });
                    }
                    current_mon_idx = selected;
                    need_full = true;
                    capturer = Some(cap);
                }
                Err(e) => {
                    warn!("Screen capturer deferred ({e}); retrying");
                    thread::sleep(Duration::from_millis(120));
                    continue;
                }
            }
        }

        if let Some(cap) = capturer.as_mut() {
            let active = *shared.active_monitor.lock().unwrap();
            if active != current_mon_idx {
                cap.set_monitor(active);
                current_mon_idx = active;
                need_full = true;
            }

            let force_kf = shared.force_keyframe.swap(false, Ordering::SeqCst);
            let output = cap.capture_next(need_full || force_kf, 33);
            let cursor = if shared.capture_cursor.load(Ordering::Relaxed)
                && matches!(output, CaptureOutput::Frame(_) | CaptureOutput::Timeout)
            {
                cap.capture_cursor()
            } else {
                None
            };
            if let Some(update) = cursor_delta.update(current_mon_idx, cursor, force_kf)
                && ev_tx.blocking_send(IpcMsg::Video(update)).is_err()
            {
                break;
            }
            match output {
                CaptureOutput::Frame(raw) => {
                    if (raw.width, raw.height) != last_dims && raw.width > 0 && raw.height > 0 {
                        last_dims = (raw.width, raw.height);
                        let new_mons = cap.monitors();
                        let dpi = new_mons
                            .iter()
                            .find(|mi| mi.id == current_mon_idx)
                            .map(|mi| mi.dpi_scale)
                            .unwrap_or(1.0);
                        *shared.monitors.lock().unwrap() = new_mons.clone();
                        let _ = ev_tx.blocking_send(IpcMsg::MonitorList {
                            monitors: new_mons,
                            active_monitor_id: current_mon_idx,
                        });
                        let _ = ev_tx.blocking_send(IpcMsg::ResolutionChange {
                            monitor_id: current_mon_idx,
                            width: raw.width as u16,
                            height: raw.height as u16,
                            dpi_scale: dpi,
                        });
                    }
                    need_full = false;
                    let mut guard = slot.data.lock().unwrap();
                    *guard = Some(raw);
                    slot.cv.notify_one();
                }
                CaptureOutput::Timeout => {}
                CaptureOutput::DesktopSwitched { state, name } => {
                    if force_kf {
                        shared.force_keyframe.store(true, Ordering::SeqCst);
                    }
                    if name != current_desktop {
                        current_desktop = name.clone();
                        shared.desktop_switched.store(true, Ordering::SeqCst);
                    }
                    let _ = ev_tx.blocking_send(IpcMsg::DesktopStatus {
                        state,
                        desktop_name: name,
                    });
                    capturer = None;
                    need_full = true;
                }
                CaptureOutput::Reacquire(_) => {
                    capturer = None;
                    need_full = true;
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }

        let elapsed = loop_start.elapsed();
        if elapsed < frame_interval {
            thread::sleep(frame_interval - elapsed);
        }
    }
}

fn emit_desktop_status(ev_tx: &mpsc::Sender<IpcMsg>, name: &str) {
    let state = if name.eq_ignore_ascii_case("Winlogon") {
        DesktopState::SecureWinlogon
    } else if name.eq_ignore_ascii_case("Disconnect") {
        DesktopState::Disconnected
    } else {
        DesktopState::Interactive
    };
    let _ = ev_tx.blocking_send(IpcMsg::DesktopStatus {
        state,
        desktop_name: name.to_string(),
    });
}

fn encode_loop(
    shared: Arc<SharedAgentState>,
    slot: Arc<LatestRawSlot>,
    ev_tx: mpsc::Sender<IpcMsg>,
    stop: Arc<AtomicBool>,
) {
    let mut seq: u64 = 0;
    let mut h264: Option<H264VideoEncoder> = None;
    let mut auto_tracker = AutoModeTracker::new();
    let mut last_frame_cache: Option<(u8, u32, u32, Vec<u8>)> = None;

    while !stop.load(Ordering::SeqCst) {
        let frame_opt = {
            let mut guard = slot.data.lock().unwrap();
            while guard.is_none() && !stop.load(Ordering::SeqCst) {
                let (g, timeout_res) = slot
                    .cv
                    .wait_timeout(guard, Duration::from_millis(250))
                    .unwrap();
                guard = g;
                if timeout_res.timed_out() && guard.is_none() {
                    if auto_tracker.on_idle_tick()
                        && let Some((mon, w, h, ref bgra)) = last_frame_cache
                    {
                        seq += 1;
                        let tiles = build_full_tiles(bgra, w, h);
                        let _ = ev_tx.blocking_send(IpcMsg::Video(VideoMsg::TileBatch {
                            monitor_id: mon,
                            frame_seq: seq,
                            timestamp_us: now_micros(),
                            width: w as u16,
                            height: h as u16,
                            is_full_refresh: true,
                            move_rects: Vec::new(),
                            tiles,
                        }));
                    }
                    break;
                }
            }
            guard.take()
        };

        let Some(raw) = frame_opt else {
            continue;
        };
        if raw.width == 0 || raw.height == 0 {
            continue;
        }

        let quality = *shared.quality_mode.lock().unwrap();
        let fps = *shared.target_fps.lock().unwrap() as f32;
        let bitrate = *shared.bitrate_kbps.lock().unwrap() * 1000;

        let dirty_area: u64 = if raw.force_full {
            (raw.width as u64) * (raw.height as u64)
        } else {
            raw.dirty_rects
                .iter()
                .map(|&(l, t, r, b)| ((r - l).max(0) as u64) * ((b - t).max(0) as u64))
                .sum()
        };
        let total_area = ((raw.width as u64) * (raw.height as u64)).max(1);
        let dirty_ratio = (dirty_area as f64) / (total_area as f64);

        let use_h264 = match quality {
            VideoQualityMode::CrispText => {
                auto_tracker.reset_to_crisp();
                false
            }
            VideoQualityMode::SmoothVideo => true,
            VideoQualityMode::Auto => auto_tracker.update(dirty_ratio),
        };

        last_frame_cache = Some((raw.monitor_id, raw.width, raw.height, raw.bgra.clone()));
        seq += 1;

        if use_h264 {
            let enc = match h264.as_mut() {
                Some(e) => {
                    let _ = e.reconfigure(raw.width, raw.height, bitrate, fps);
                    e
                }
                None => match H264VideoEncoder::new(raw.width, raw.height, bitrate, fps) {
                    Ok(e) => h264.insert(e),
                    Err(e) => {
                        warn!("H264 init error: {e}");
                        continue;
                    }
                },
            };
            if raw.force_full || auto_tracker.just_entered_h264() {
                enc.force_keyframe();
            }
            match enc.encode_bgra(&raw.bgra, raw.width, raw.height) {
                Ok((annex_b, is_keyframe)) => {
                    let msg = VideoMsg::H264Frame {
                        monitor_id: raw.monitor_id,
                        frame_seq: seq,
                        timestamp_us: now_micros(),
                        width: raw.width as u16,
                        height: raw.height as u16,
                        is_keyframe,
                        annex_b,
                    };
                    if ev_tx.blocking_send(IpcMsg::Video(msg)).is_err() {
                        break;
                    }
                }
                Err(e) => warn!("H264 encode failed: {e}"),
            }
        } else {
            let tiles = if raw.force_full || raw.dirty_rects.is_empty() && raw.move_rects.is_empty() {
                build_full_tiles(&raw.bgra, raw.width, raw.height)
            } else {
                dirty_rects_to_tiles(&raw.bgra, raw.width, raw.height, &raw.dirty_rects)
            };
            let msg = VideoMsg::TileBatch {
                monitor_id: raw.monitor_id,
                frame_seq: seq,
                timestamp_us: now_micros(),
                width: raw.width as u16,
                height: raw.height as u16,
                is_full_refresh: raw.force_full,
                move_rects: raw.move_rects,
                tiles,
            };
            if ev_tx.blocking_send(IpcMsg::Video(msg)).is_err() {
                break;
            }
        }
    }
}

/// Adaptive hysteresis tracker switching between Crisp Text (`zstd`) and Motion (`openh264`).
#[derive(Debug, Default)]
pub struct AutoModeTracker {
    in_h264: bool,
    high_motion_streak: u8,
    low_motion_streak: u8,
    entered_h264_this_frame: bool,
}

impl AutoModeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset_to_crisp(&mut self) {
        self.in_h264 = false;
        self.high_motion_streak = 0;
        self.low_motion_streak = 0;
        self.entered_h264_this_frame = false;
    }

    pub fn just_entered_h264(&self) -> bool {
        self.entered_h264_this_frame
    }

    /// Returns `true` when `openh264` should encode the current frame.
    pub fn update(&mut self, dirty_ratio: f64) -> bool {
        self.entered_h264_this_frame = false;
        if !self.in_h264 {
            if dirty_ratio >= 0.45 {
                self.high_motion_streak = self.high_motion_streak.saturating_add(1);
                if self.high_motion_streak >= 3 {
                    self.in_h264 = true;
                    self.high_motion_streak = 0;
                    self.low_motion_streak = 0;
                    self.entered_h264_this_frame = true;
                }
            } else {
                self.high_motion_streak = 0;
            }
        } else if dirty_ratio <= 0.10 {
            self.low_motion_streak = self.low_motion_streak.saturating_add(1);
            if self.low_motion_streak >= 15 {
                self.in_h264 = false;
                self.low_motion_streak = 0;
                self.high_motion_streak = 0;
            }
        } else {
            self.low_motion_streak = 0;
        }
        self.in_h264
    }

    /// Triggered after ~250-500ms idle in H264 mode to emit a crisp lossless settle refresh.
    pub fn on_idle_tick(&mut self) -> bool {
        if self.in_h264 {
            self.in_h264 = false;
            self.low_motion_streak = 0;
            self.high_motion_streak = 0;
            return true;
        }
        false
    }
}

fn build_full_tiles(bgra: &[u8], w: u32, h: u32) -> Vec<DirtyTile> {
    dirty_rects_to_tiles(bgra, w, h, &[(0, 0, w as i32, h as i32)])
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

/// Maps `[0.0, 1.0]` normalized monitor coordinates into virtual desktop `0..=65535` space.
pub fn map_norm_to_virtual_desktop(
    norm_x: f32,
    norm_y: f32,
    mon: &MonitorInfo,
    virt_left: i32,
    virt_top: i32,
    virt_w: i32,
    virt_h: i32,
) -> (i32, i32) {
    let nx = norm_x.clamp(0.0, 1.0) as f64;
    let ny = norm_y.clamp(0.0, 1.0) as f64;
    let mw = (mon.width.max(1) - 1) as f64;
    let mh = (mon.height.max(1) - 1) as f64;
    let px = mon.left as f64 + nx * mw;
    let py = mon.top as f64 + ny * mh;
    let vw = (virt_w.max(2) - 1) as f64;
    let vh = (virt_h.max(2) - 1) as f64;
    let wx = ((px - virt_left as f64) * 65535.0 / vw).round().clamp(0.0, 65535.0) as i32;
    let wy = ((py - virt_top as f64) * 65535.0 / vh).round().clamp(0.0, 65535.0) as i32;
    (wx, wy)
}

fn input_loop(
    shared: Arc<SharedAgentState>,
    rx: std::sync::mpsc::Receiver<InputMsg>,
    stop: Arc<AtomicBool>,
) {
    let mut injector = platform::create_injector();
    injector.sync_desktop();

    while !stop.load(Ordering::SeqCst) {
        let Ok(msg) = rx.recv() else {
            break;
        };
        if shared.desktop_switched.swap(false, Ordering::SeqCst) {
            injector.sync_desktop();
        }
        match msg {
            InputMsg::Pointer(ev) => {
                let mon_id = *shared.active_monitor.lock().unwrap();
                let mons = shared.monitors.lock().unwrap();
                let mon = mons
                    .iter()
                    .find(|m| m.id == mon_id)
                    .cloned()
                    .unwrap_or(MonitorInfo {
                        id: 0,
                        name: "Primary".into(),
                        is_primary: true,
                        left: 0,
                        top: 0,
                        width: 1920,
                        height: 1080,
                        dpi_scale: 1.0,
                    });
                drop(mons);
                injector.inject_pointer(ev, &mon);
            }
            InputMsg::Key(ev) => injector.inject_key(ev),
            InputMsg::ReleaseAllKeys => injector.release_all_keys(),
            InputMsg::Unicode(ch) if !ch.is_control() => injector.inject_unicode(ch),
            InputMsg::Unicode(_) => {}
            InputMsg::DeviceAction(action) => injector.device_action(action),
        }
    }
    injector.release_all_keys();
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_desktop_coordinate_mapping() {
        let primary = MonitorInfo {
            id: 0,
            name: "Primary".into(),
            is_primary: true,
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
            dpi_scale: 1.0,
        };
        let secondary_left = MonitorInfo {
            id: 1,
            name: "Left".into(),
            is_primary: false,
            left: -1920,
            top: 0,
            width: 1920,
            height: 1080,
            dpi_scale: 1.0,
        };

        assert_eq!(
            map_norm_to_virtual_desktop(0.0, 0.0, &secondary_left, -1920, 0, 3840, 1080),
            (0, 0)
        );
        assert_eq!(
            map_norm_to_virtual_desktop(1.0, 1.0, &primary, -1920, 0, 3840, 1080),
            (65535, 65535)
        );
        let (mid_x, _) = map_norm_to_virtual_desktop(0.0, 0.5, &primary, -1920, 0, 3840, 1080);
        assert!((mid_x - 32776).abs() < 20);
    }

    #[test]
    fn auto_mode_hysteresis_and_dirty_rect_coalescing() {
        let mut tracker = AutoModeTracker::new();
        assert!(!tracker.update(0.60));
        assert!(!tracker.update(0.60));
        assert!(tracker.update(0.60));
        assert!(tracker.just_entered_h264());

        for _ in 0..14 {
            assert!(tracker.update(0.05));
        }
        assert!(!tracker.update(0.05));

        let w = 64u32;
        let h = 64u32;
        let bgra = vec![200u8; (w * h * 4) as usize];
        let tiles = dirty_rects_to_tiles(&bgra, w, h, &[(0, 0, 10, 10), (8, 0, 20, 10)]);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].x, 0);
        assert_eq!(tiles[0].width, 20);
    }
}
