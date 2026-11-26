use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use deskbolt_proto::video_pipeline::{
    VideoCapabilities, VideoCodec, VideoEncoder, VideoInput, ZstdVideo, platform_video_encoder,
    preferred_motion,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deskbolt_proto::{
    CursorDelta, DesktopState, DirtyTile, H264VideoEncoder, InputMsg, IpcMsg, MonitorInfo, VideoMsg,
    VideoQualityMode,
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
                IpcMsg::Video(VideoMsg::H264Frame { is_keyframe: true, .. } | VideoMsg::EncodedFrame { is_keyframe: true, .. })
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
    congestion: Mutex<HashMap<u64, (u8, Instant)>>,
    decoder_support: Mutex<HashMap<u64, VideoCapabilities>>,
    all_displays: AtomicBool,
    capture_reinit: AtomicBool,
    force_keyframe: AtomicBool,
    desktop_switched: AtomicBool,
    capture_cursor: AtomicBool,
}

struct LatestRawSlot {
    data: Mutex<VecDeque<RawCapturedFrame>>,
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
        congestion: Mutex::new(HashMap::new()),
    decoder_support: Mutex::new(HashMap::new()),
    all_displays: AtomicBool::new(false),
        capture_reinit: AtomicBool::new(false),
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
        data: Mutex::new(VecDeque::new()),
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

    let mut audio_viewers = std::collections::HashSet::new();
    let mut workspace_viewers = HashSet::new();
    let display_backend = crate::display_modes::Native;
    let mut display = crate::display_modes::DisplayController::new(crate::display_modes::recovery_path());
    if let Err(error) = display.recover(&display_backend) {
        warn!("Could not restore display mode: {error}");
    }
    let mut system_audio: Option<deskbolt_proto::system_audio::SystemAudioCapture> = None;
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
            IpcMsg::DesktopAction { viewer, action } => {
                use deskbolt_proto::SpecialAction;
                let result = match action {
                    SpecialAction::SetDisplayMode { monitor_id, width, height, refresh_hz } => {
                        let monitor = shared.monitors.lock().unwrap().iter().find(|m| m.id == monitor_id).cloned();
                        monitor.ok_or_else(|| "The display is no longer connected.".into())
                            .and_then(|m| display.set(&display_backend, viewer, &m, (width, height, refresh_hz)))
                            .map(|mode| format!("Display set to {} × {}.", mode.width, mode.height))
                    }
                    SpecialAction::RestoreDisplayMode => display.release(&display_backend, viewer)
                        .map(|()| "Host display mode restored.".into()),
                    _ => Err("The desktop action is unavailable.".into()),
                };
                let accepted = result.is_ok();
                if accepted {
                    shared.capture_reinit.store(true, Ordering::Release);
                }
                let message = result.unwrap_or_else(|error| error);
                let _ = ev_tx.blocking_send(IpcMsg::DesktopActionResult { viewer, action, accepted, message });
            }
            IpcMsg::ReleaseDesktopSettings(viewer) => {
                match display.disconnected(&display_backend, viewer) {
                    Ok(true) => shared.capture_reinit.store(true, Ordering::Release),
                    Ok(false) => {}
                    Err(error) => warn!("Could not restore display mode: {error}"),
                }
            }
            IpcMsg::MonitorWorkspace { viewer, enabled } => {
                let changed = if enabled { workspace_viewers.insert(viewer) } else { workspace_viewers.remove(&viewer) };
                shared.all_displays.store(!workspace_viewers.is_empty(), Ordering::Relaxed);
                if changed {
                    shared.force_keyframe.store(true, Ordering::Relaxed);
                }
            }
            IpcMsg::VideoDecoderSupport { viewer, capabilities } => {
                let mut viewers = shared.decoder_support.lock().unwrap();
                if let Some(capabilities) = capabilities {
                    if viewers.insert(viewer, capabilities) != Some(capabilities) {
                        shared.force_keyframe.store(true, Ordering::Relaxed);
                    }
                } else {
                    viewers.remove(&viewer);
                }
            }
            IpcMsg::VideoCongestion { viewer, level } => {
                let mut viewers = shared.congestion.lock().unwrap();
                if level == 0 {
                    viewers.remove(&viewer);
                } else {
                    viewers.insert(viewer, (level.min(2), Instant::now()));
                }
            }
            IpcMsg::MonitorSelect { monitor_id } => {
                *shared.active_monitor.lock().unwrap() = monitor_id;
                shared.force_keyframe.store(true, Ordering::SeqCst);
            }
            IpcMsg::KeyframeRequest { .. } => {
                shared.force_keyframe.store(true, Ordering::SeqCst);
            }
            IpcMsg::SetCurtainMode(enabled) => {
                if let Err(error) = platform::set_curtain_mode(enabled) {
                    warn!("Could not change screen blanking: {error}");
                }
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
            IpcMsg::SystemAudioStart(viewer) => {
                if system_audio.as_ref().is_some_and(|capture| capture.is_finished()) {
                    system_audio = None;
                    audio_viewers.clear();
                }
                let error = if system_audio.is_none() {
                    let packets = ev_tx.clone();
                    let errors = ev_tx.clone();
                    match deskbolt_proto::system_audio::SystemAudioCapture::start(
                        move |packet| { let _ = packets.try_send(IpcMsg::SystemAudioFrame(packet)); },
                        move |error| { let _ = errors.try_send(IpcMsg::SystemAudioFailed(error)); },
                    ) {
                        Ok(capture) => { system_audio = Some(capture); None }
                        Err(error) => Some(error),
                    }
                } else { None };
                if error.is_none() {
                    audio_viewers.insert(viewer);
                }
                let _ = ev_tx.blocking_send(IpcMsg::SystemAudioReady { viewer, error });
            }
            IpcMsg::SystemAudioStop(viewer) => {
                audio_viewers.remove(&viewer);
                if audio_viewers.is_empty() {
                    system_audio = None;
                }
            }
            IpcMsg::Shutdown => break,
            _ => {}
        }
    }

    if let Err(error) = display.shutdown(&display_backend) {
        warn!("Could not restore display mode: {error}");
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
    let mut last_level = 0;
    let mut secondary: HashMap<u8, SecondaryCapture> = HashMap::new();

    while !stop.load(Ordering::SeqCst) {
        if shared.capture_reinit.swap(false, Ordering::Acquire) {
            capturer = None;
            secondary.clear();
            slot.data.lock().unwrap().clear();
        }
        let wanted_mon = *shared.active_monitor.lock().unwrap();
        let level = shared.congestion_level();
        if level != last_level {
            shared.force_keyframe.store(true, Ordering::Relaxed);
            last_level = level;
        }
        let (fps, _, _) = adaptive_quality(
            level, *shared.target_fps.lock().unwrap(), *shared.bitrate_kbps.lock().unwrap(),
        );
        let frame_interval = Duration::from_micros(1_000_000 / u64::from(fps));
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
                secondary.remove(&active);
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
                    slot.publish(raw);
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
                    secondary.clear();
                    need_full = true;
                }
                CaptureOutput::Reacquire(_) => {
                    capturer = None;
                    need_full = true;
                    thread::sleep(Duration::from_millis(50));
                }
            }
            if shared.all_displays.load(Ordering::Relaxed) {
                let monitors = shared.monitors.lock().unwrap().clone();
                secondary.retain(|id, _| *id != active && monitors.iter().any(|m| m.id == *id));
                for monitor in monitors.into_iter().filter(|monitor| monitor.id != active) {
                    let track = secondary.entry(monitor.id).or_default();
                    track.capture(monitor.id, force_kf, &slot, &ev_tx, shared.capture_cursor.load(Ordering::Relaxed));
                }
            } else if !secondary.is_empty() {
                secondary.clear();
                slot.data.lock().unwrap().retain(|frame| frame.monitor_id == active);
            }
        }

        let elapsed = loop_start.elapsed();
        if elapsed < frame_interval {
            thread::sleep(frame_interval - elapsed);
        }
    }
}

impl LatestRawSlot {
    fn publish(&self, mut frame: RawCapturedFrame) {
        let mut pending = self.data.lock().unwrap();
        if let Some(position) = pending.iter().position(|data| data.monitor_id == frame.monitor_id) {
            pending.remove(position);
            frame.force_full = true;
        }
        pending.push_back(frame);
        self.cv.notify_one();
    }
}

#[derive(Default)]
struct SecondaryCapture {
    capturer: Option<Box<dyn ScreenCapturer>>,
    cursor: CursorDelta,
    retry: Option<Instant>,
    refresh: bool,
}

impl SecondaryCapture {
    fn capture(&mut self, id: u8, force: bool, slot: &LatestRawSlot, events: &mpsc::Sender<IpcMsg>, cursor_enabled: bool) {
        if self.capturer.is_none() {
            if self.retry.is_some_and(|at| at.elapsed() < Duration::from_secs(2)) {
                return;
            }
            self.retry = Some(Instant::now());
            self.capturer = platform::create_capturer(id).ok();
            self.refresh = true;
        }
        let Some(capturer) = &mut self.capturer else { return };
        self.refresh |= force;
        let output = capturer.capture_next(self.refresh, 1);
        if cursor_enabled && matches!(output, CaptureOutput::Frame(_) | CaptureOutput::Timeout)
            && let Some(update) = self.cursor.update(id, capturer.capture_cursor(), force)
        {
            let _ = events.blocking_send(IpcMsg::Video(update));
        }
        match output {
            CaptureOutput::Frame(mut frame) if frame.monitor_id == id => {
                frame.force_full |= self.refresh;
                self.refresh = false;
                slot.publish(frame);
            }
            CaptureOutput::Reacquire(_) | CaptureOutput::DesktopSwitched { .. } => self.capturer = None,
            _ => {}
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
    let mut encoder: Option<Box<dyn VideoEncoder>> = None;
    let mut format = None;
    let mut viable = VideoCapabilities::default();
    let mut auto_tracker = AutoModeTracker::new();
    let mut sent_motion = false;
    let mut scaled = Vec::new();
    let mut last_frame_cache: Option<(u8, u32, u32, Vec<u8>)> = None;

    while !stop.load(Ordering::SeqCst) {
        let frame_opt = {
            let mut guard = slot.data.lock().unwrap();
            while guard.is_empty() && !stop.load(Ordering::SeqCst) {
                let (g, timeout_res) = slot
                    .cv
                    .wait_timeout(guard, Duration::from_millis(250))
                    .unwrap();
                guard = g;
                if timeout_res.timed_out() && guard.is_empty() {
                    if auto_tracker.on_idle_tick()
                        && let Some((mon, w, h, ref bgra)) = last_frame_cache
                    {
                        seq += 1;
                        let tiles = build_full_tiles(bgra, w, h);
                        sent_motion = false;
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
            guard.pop_front()
        };

        let Some(raw) = frame_opt else {
            continue;
        };
        if raw.width == 0 || raw.height == 0 {
            continue;
        }

        let selected = *shared.active_monitor.lock().unwrap();
        if raw.monitor_id != selected {
            if shared.all_displays.load(Ordering::Relaxed) {
                seq += 1;
                let input = VideoInput {
                    bgra: &raw.bgra, width: raw.width, height: raw.height, monitor: raw.monitor_id,
                    sequence: seq, timestamp_us: now_micros(), bitrate: 0, fps: 0,
                    refresh: raw.force_full, dirty_rects: &raw.dirty_rects, moves: &raw.move_rects,
                };
                if let Ok(Some(frame)) = ZstdVideo.encode(input)
                    && ev_tx.blocking_send(IpcMsg::Video(frame)).is_err()
                {
                    break;
                }
            }
            continue;
        }
        let quality = *shared.quality_mode.lock().unwrap();
        let (fps, bitrate, downscale) = adaptive_quality(
            shared.congestion_level(), *shared.target_fps.lock().unwrap(), *shared.bitrate_kbps.lock().unwrap(),
        );
        let bitrate = bitrate * 1000;

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
            VideoQualityMode::Auto => auto_tracker.update(dirty_ratio) || downscale,
        };

        last_frame_cache = Some((raw.monitor_id, raw.width, raw.height, raw.bgra.clone()));
        seq += 1;

        let input = VideoInput {
            bgra: &raw.bgra,
            width: raw.width,
            height: raw.height,
            monitor: raw.monitor_id,
            sequence: seq,
            timestamp_us: now_micros(),
            bitrate,
            fps,
            refresh: raw.force_full,
            dirty_rects: &raw.dirty_rects,
            moves: &raw.move_rects,
        };

        if use_h264 {
            let mut motion = VideoInput {
                refresh: input.refresh || !sent_motion || auto_tracker.just_entered_h264(),
                ..input
            };
            if downscale && input.width >= 64 && input.height >= 64 {
                (motion.width, motion.height) = scale_motion_frame(input.bgra, input.width, input.height, &mut scaled);
                motion.bgra = &scaled;
            }
            let caps = shared.common_decoders();
            let config = (caps, motion.width, motion.height, motion.fps, motion.bitrate);
            if format != Some(config) {
                encoder = None;
                format = Some(config);
                viable = caps;
            }
            let result = (|| {
                if encoder.is_none() {
                    preferred_motion(viable, |codec| {
                        encoder = platform_video_encoder(codec, motion);
                        if encoder.is_none() {
                            viable.remove(codec);
                        }
                        encoder.is_some()
                    });
                    motion.refresh = true;
                }
                if let Some(active) = encoder.as_mut() {
                    match active.encode(motion) {
                        Ok(frame) => return Ok(frame),
                        Err(error) if active.codec() != VideoCodec::H264Sw => {
                            warn!("Accelerated video unavailable: {error}");
                            viable.remove(active.codec());
                            encoder = None;
                            motion.refresh = true;
                        }
                        Err(error) => return Err(error),
                    }
                }
                if encoder.is_none() {
                    encoder = Some(Box::new(H264VideoEncoder::new(
                        motion.width, motion.height, bitrate, f32::from(fps),
                    )?));
                }
                encoder.as_mut().unwrap().encode(motion)
            })();
            match result {
                Ok(Some(msg)) => {
                    if ev_tx.blocking_send(IpcMsg::Video(msg)).is_err() {
                        break;
                    }
                    sent_motion = true;
                    continue;
                }
                Ok(None) => {
                    seq -= 1;
                    continue;
                }
                Err(error) => {
                    warn!("Motion encoding failed: {error}");
                    encoder = None;
                }
            }
        }

        let tiles = VideoInput { refresh: input.refresh || sent_motion || use_h264, ..input };
        sent_motion = false;
        if let Ok(Some(msg)) = ZstdVideo.encode(tiles)
            && ev_tx.blocking_send(IpcMsg::Video(msg)).is_err()
        {
            break;
        }
    }
}

impl SharedAgentState {
    fn common_decoders(&self) -> VideoCapabilities {
        self.decoder_support.lock().unwrap().values().copied()
            .reduce(VideoCapabilities::intersect).unwrap_or_default()
    }

    fn congestion_level(&self) -> u8 {
        let mut viewers = self.congestion.lock().unwrap();
        viewers.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(8));
        viewers.values().map(|(level, _)| *level).max().unwrap_or_default()
    }
}

fn adaptive_quality(level: u8, fps: u8, bitrate_kbps: u32) -> (u8, u32, bool) {
    let bitrate = bitrate_kbps.clamp(100, 100_000);
    match level {
        0 => (fps.clamp(15, 120), bitrate, false),
        1 => (((u16::from(fps) * 3 / 4) as u8).max(15), (bitrate * 3 / 4).max(100), false),
        _ => ((fps / 2).max(15), (bitrate / 2).max(100), true),
    }
}

fn scale_motion_frame(bgra: &[u8], width: u32, height: u32, dst: &mut Vec<u8>) -> (u32, u32) {
    let w = (width * 3 / 4) & !1;
    let h = (height * 3 / 4) & !1;
    dst.resize((w * h * 4) as usize, 0);
    for y in 0..h {
        for x in 0..w {
            let source = ((y * height / h * width + x * width / w) * 4) as usize;
            let dest = ((y * w + x) * 4) as usize;
            dst[dest..dest + 4].copy_from_slice(&bgra[source..source + 4]);
        }
    }
    (w, h)
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
    deskbolt_proto::video_pipeline::dirty_rects_to_tiles(bgra, w, h, &[(0, 0, w as i32, h as i32)])
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
            InputMsg::MonitorPointer { monitor_id, event } => {
                let monitor = shared.monitors.lock().unwrap().iter().find(|m| m.id == monitor_id).cloned();
                if let Some(monitor) = monitor {
                    injector.inject_pointer(event, &monitor);
                }
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
    fn congestion_reduces_motion_fps_bitrate_and_dimensions() {
        assert_eq!(adaptive_quality(0, 120, 8_000), (120, 8_000, false));
        assert_eq!(adaptive_quality(1, 120, 8_000), (90, 6_000, false));
        assert_eq!(adaptive_quality(2, 30, 4_000), (15, 2_000, true));
        assert_eq!(adaptive_quality(2, 15, 100), (15, 100, true));
        let mut output = Vec::new();
        assert_eq!(scale_motion_frame(&vec![255; 65 * 67 * 4], 65, 67, &mut output), (48, 50));
        assert_eq!(output, vec![255; 48 * 50 * 4]);
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
        let tiles = deskbolt_proto::video_pipeline::dirty_rects_to_tiles(&bgra, w, h, &[(0, 0, 10, 10), (8, 0, 20, 10)]);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].x, 0);
        assert_eq!(tiles[0].width, 20);
    }
}
