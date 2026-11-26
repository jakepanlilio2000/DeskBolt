use deskbolt_proto::video_pipeline::{
    VideoCapabilities, VideoCodec, VideoDecoder, ZstdVideo, platform_video_decoder,
    platform_video_decoders,
};
use deskbolt_proto::partial_file::{DEFAULT_PART_TTL_HOURS, PartialFile, offer, resume_source};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deskbolt_proto::{
    AsyncFrameReader, ChannelId, ClipboardSync, CommsMsg, ControlMsg, DriveEntry, FLAG_NONE, FLAG_URGENT, FileEntry,
    FileTransferMsg, Frame, H264VideoDecoder, InputMsg, MonitorInfo, NONCE_LEN, PROTO_VERSION,
    PowerMsg, SessionMode, SyncEntry, SysAdminMsg, SystemSnapshot, TerminalMsg, TofuPinPolicy,
    TofuServerCertVerifier, TransferDir, TunnelMsg, VOICE_SAMPLE_RATE, VideoMsg, VideoQualityMode,
    VoiceControls, VoiceEngine, compute_client_proof, connect_tls,
    derive_auth_key, format_fingerprint, pack_message, parse_fingerprint, random_bytes,
    read_frame_async, unpack_message, verify_server_proof, write_frame_async,
};
use ring::digest::Context;
#[cfg(test)]
use ring::digest::SHA256;
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
#[cfg(target_os = "windows")]
use windows::Win32::Storage::FileSystem::GetLogicalDrives;

pub const FILE_CHUNK_SIZE: usize = 64 * 1024;
pub const MAX_UNACKED_CHUNKS: u32 = 8;

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionPhase {
    Connecting,
    TofuPrompt {
        fingerprint: String,
    },
    TofuMismatch {
        expected: String,
        observed: String,
    },
    PasswordPrompt {
        error: Option<String>,
    },
    Connected,
    Disconnected(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwritePolicy {
    Ask,
    OverwriteAll,
    SkipAll,
}

#[derive(Debug, Clone)]
pub struct OverwritePrompt {
    pub transfer_id: u64,
    pub file_name: String,
    pub remote_path: String,
    pub local_size: u64,
    pub remote_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferItemStatus {
    Queued,
    WaitingOverwrite,
    Transferring,
    Verifying,
    Verified,
    Skipped,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct TransferItem {
    pub id: u64,
    pub direction: TransferDir,
    pub file_name: String,
    pub local_path: PathBuf,
    pub remote_path: String,
    pub total_bytes: u64,
    pub bytes_done: u64,
    pub speed_bps: u64,
    pub status: TransferItemStatus,
}

#[derive(Debug, Clone)]
pub struct FileTransferState {
    pub open: bool,
    pub local_roots: Vec<String>,
    pub local_path: PathBuf,
    pub local_entries: Vec<FileEntry>,
    pub local_selected: HashSet<String>,
    pub local_error: Option<String>,

    pub remote_roots: Vec<DriveEntry>,
    pub remote_path: String,
    pub remote_entries: Vec<FileEntry>,
    pub remote_selected: HashSet<String>,
    pub remote_error: Option<String>,

    pub queue: Vec<TransferItem>,
    pub pending_overwrite: Option<OverwritePrompt>,
    pub remote_hash_tree: Option<Vec<SyncEntry>>,
    pub remote_hash_error: Option<String>,
    pub remote_hash_pending: bool,
    pub hash_tree_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QuickEditBuffer {
    pub remote_path: String,
    pub content: String,
    pub dirty: bool,
    pub saving: bool,
    pub status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LogTailBuffer {
    pub remote_path: String,
    pub text: String,
    pub next_offset: u64,
    pub following: bool,
    pub status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TunnelRule {
    pub id: u32,
    pub local_port: u16,
    pub target_host: String,
    pub target_port: u16,
    pub reverse: bool,
    pub active: bool,
    pub status: String,
}

impl FileTransferState {
    pub fn new() -> Self {
        let local_roots = list_local_drives();
        let initial_local = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .unwrap_or_else(|| {
                if cfg!(target_os = "windows") {
                    PathBuf::from(r"C:\")
                } else {
                    PathBuf::from("/")
                }
            });
        let mut st = Self {
            open: false,
            local_roots,
            local_path: initial_local,
            local_entries: Vec::new(),
            local_selected: HashSet::new(),
            local_error: None,
            remote_roots: Vec::new(),
            remote_path: String::new(),
            remote_entries: Vec::new(),
            remote_selected: HashSet::new(),
            remote_error: None,
            queue: Vec::new(),
            pending_overwrite: None,
            remote_hash_tree: None,
            remote_hash_error: None,
            remote_hash_pending: false,
            hash_tree_error: None,
        };
        st.refresh_local();
        st
    }
}

impl Default for FileTransferState {
    fn default() -> Self {
        Self::new()
    }
}

impl FileTransferState {

    pub fn refresh_local(&mut self) {
        self.local_roots = list_local_drives();
        self.local_selected.clear();
        match read_local_dir(&self.local_path) {
            Ok(entries) => {
                self.local_entries = entries;
                self.local_error = None;
            }
            Err(e) => {
                self.local_entries.clear();
                self.local_error = Some(e);
            }
        }
    }
}

pub enum SessionCommand {
    TrustTofuAndContinue { save_pin: bool },
    SubmitPassword(String),
    CancelConnect,
    Input(InputMsg),
    TypeClipboard { text: String, delay_ms: u16 },
    StopTyping,
    Control(ControlMsg),
    Clipboard(ClipboardSync),
    File(FileTransferMsg),
    QueueUpload { local_path: PathBuf, remote_path: String },
    QueueDownload { remote_path: String, local_path: PathBuf, total_bytes: u64 },
    ResolveOverwrite { transfer_id: u64, overwrite: bool, apply_all: bool },
    CancelTransfer(u64),
    QuickEditFetch { remote_path: String },
    QuickEditSave { remote_path: String, content: String },
    Terminal(TerminalMsg),
    StartLocalForward { rule_id: u32, local_port: u16, target_host: String, target_port: u16 },
    StopLocalForward { rule_id: u32 },
    StartReverseForward { listener_id: u32, bind_port: u16, target_host: String, target_port: u16 },
    StopReverseForward { listener_id: u32 },
    SysAdmin(SysAdminMsg),
    Comms(CommsMsg),
    Power(PowerMsg),
    StartVoice {
        input_device: Option<String>,
        output_device: Option<String>,
    },
    StopVoice,
    SetSystemAudio { enabled: bool, output_device: String },
    Disconnect,
}

#[derive(Debug, Clone)]
pub struct ChatLine {
    pub sender: String,
    pub timestamp: u64,
    pub text: String,
    pub outgoing: bool,
}

#[derive(Debug)]
pub enum SessionUiEvent {
    PhaseChanged(ConnectionPhase),
    ModeUpdated(SessionMode),
    PinAccepted { host_id: Option<i64>, fingerprint: String },
    MonitorsUpdated { monitors: Vec<MonitorInfo>, active_id: u8 },
    RttUpdated(u64),
    StatusBanner(String),
    DesktopAction { action: deskbolt_proto::SpecialAction, accepted: bool, message: String },
    RemoteClipboard(ClipboardSync),
    AndroidControls,
    Cursor(VideoMsg),
    TypingChanged(bool),
    FileRoots(Vec<DriveEntry>),
    FileDirList { path: String, entries: Vec<FileEntry>, error: Option<String> },
    FileActionResult { ok: bool, error: Option<String> },
    TransferProgress(Vec<TransferItem>),
    OverwritePrompt(Option<OverwritePrompt>),
    HashTree { entries: Vec<SyncEntry>, error: Option<String> },
    TailChunk { path: String, next_offset: u64, data: Vec<u8>, truncated: bool, error: Option<String> },
    QuickEditLoaded { path: String, content: String, error: Option<String> },
    QuickEditSaved { path: String, ok: bool, error: Option<String> },
    Terminal(TerminalMsg),
    TunnelRuleStatus { rule_id: u32, active: bool, status: String },
    SysAdmin(SysAdminMsg),
    Comms(CommsMsg),
    VoiceState { active: bool, status: String },
    SystemAudioActive(bool),
    Power(PowerMsg),
}

/// Connection parameters bundled for clean session initialization.
#[derive(Debug, Clone)]
pub struct ConnectTarget {
    pub host_id: Option<i64>,
    pub title: String,
    pub host: String,
    pub port: u16,
    pub mode: SessionMode,
    pub quality: VideoQualityMode,
    pub pinned_fingerprint: Option<String>,
}

pub(crate) type MonitorFrames = Arc<Mutex<HashMap<u8, Arc<DecodedFrame>>>>;
pub(crate) type MonitorCursors = Arc<Mutex<HashMap<u8, crate::cursor::CursorOverlay>>>;
type LatestVideo = Arc<Mutex<Option<Arc<DecodedFrame>>>>;

#[derive(Clone, Default)]
struct VideoSurfaces {
    latest_frame: LatestVideo,
    monitors: MonitorFrames,
}

type SharedVideoStats = Arc<Mutex<crate::quality_hud::VideoQualityStats>>;

/// Live connection handle shared between UI tabs and background Tokio networking.
pub struct ViewerSession {
    pub quality_hud: bool,
    pub video_stats: SharedVideoStats,
    pub id: u64,
    pub title: String,
    pub host: String,
    pub port: u16,
    pub mode: SessionMode,
    pub quality: VideoQualityMode,
    pub fit_scale: bool,
    pub capture_system_keys: bool,
    pub auto_clipboard: bool,
    pub typing_clipboard: bool,
    pub android_controls: bool,
    pub headless_admin: bool,
    pub phase: ConnectionPhase,
    pub monitors: Vec<MonitorInfo>,
    pub active_monitor_id: u8,
    pub status_banner: String,
    pub last_rtt_ms: Option<u64>,
    pub cmd_tx: mpsc::Sender<SessionCommand>,
    pub event_rx: std::sync::mpsc::Receiver<SessionUiEvent>,
    pub latest_frame: LatestVideo,
    pub(crate) monitor_frames: MonitorFrames,
    pub(crate) cursors: MonitorCursors,
    pub(crate) workspace: crate::monitor_workspace::Workspace,
    pub(crate) window_match: crate::monitor_workspace::WindowMatch,
    pub file_transfer: FileTransferState,
    pub quick_edit: Option<QuickEditBuffer>,
    pub log_tail: Option<LogTailBuffer>,
    pub terminal_open: bool,
    pub terminal_emu: crate::terminal::TerminalEmulator,
    pub terminal_active: bool,
    pub terminal_status: String,
    pub remote_serial_ports: Vec<String>,
    pub tunnels_open: bool,
    pub tunnels: Vec<TunnelRule>,
    pub next_tunnel_rule_id: u32,
    pub sys_open: bool,
    pub sys_snapshot: Option<SystemSnapshot>,
    pub sys_status: String,
    pub broadcast_responses: Vec<(u64, i32, String, Option<String>)>,
    pub chat_open: bool,
    pub chat_history: Vec<ChatLine>,
    pub chat_unread: u32,
    pub chat_input: String,
    pub popup_open: bool,
    pub popup_title: String,
    pub popup_body: String,
    pub popup_urgent: bool,
    pub popup_status: String,
    pub voice_open: bool,
    pub voice_active: bool,
    pub voice_status: String,
    pub voice_controls: Arc<VoiceControls>,
    pub system_audio_controls: Arc<deskbolt_proto::system_audio::SystemAudioControls>,
    pub system_audio_active: bool,
    pub system_audio_pending: bool,
    pub system_audio_on_connect: Option<String>,
    pub power_open: bool,
    pub power_force: bool,
    pub power_delay_secs: u32,
    pub power_status: String,
}

impl ViewerSession {
    pub fn start(
        id: u64,
        target: ConnectTarget,
        runtime: &tokio::runtime::Handle,
        repaint_fn: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let surfaces = VideoSurfaces::default();
        let decode_surfaces = surfaces.clone();
        let frame_seq = Arc::new(AtomicU64::new(0));
        let video_stats = Arc::new(Mutex::new(crate::quality_hud::VideoQualityStats::default()));
        let task_stats = video_stats.clone();
        let voice_controls = Arc::new(VoiceControls::default());

        let mut ft = FileTransferState::new();
        if target.mode == SessionMode::FileTransfer {
            ft.open = true;
        }

        let lf = decode_surfaces;
        let ev_tx_c = ev_tx.clone();
        let rp = repaint_fn.clone();
        let target_c = target.clone();
        let vc_c = voice_controls.clone();
        let system_audio_controls = Arc::new(deskbolt_proto::system_audio::SystemAudioControls::default());
        let system_audio = system_audio_controls.clone();

        runtime.spawn(async move {
            run_session_task(target_c, cmd_rx, ev_tx_c, lf, (frame_seq, task_stats), (vc_c, system_audio), rp).await;
        });

        Self {
            id,
            quality_hud: false,
            video_stats,
            title: target.title,
            host: target.host,
            port: target.port,
            mode: target.mode,
            quality: target.quality,
            fit_scale: true,
            capture_system_keys: false,
            auto_clipboard: true,
            typing_clipboard: false,
            android_controls: false,
            headless_admin: false,
            phase: ConnectionPhase::Connecting,
            monitors: Vec::new(),
            active_monitor_id: 0,
            status_banner: "Connecting...".into(),
            last_rtt_ms: None,
            cmd_tx,
            event_rx: ev_rx,
            latest_frame: surfaces.latest_frame,
            monitor_frames: surfaces.monitors,
            cursors: Arc::default(),
            workspace: crate::monitor_workspace::Workspace::default(),
            window_match: crate::monitor_workspace::WindowMatch::default(),
            file_transfer: ft,
            quick_edit: None,
            log_tail: None,
            terminal_open: false,
            terminal_emu: crate::terminal::TerminalEmulator::new(
                crate::terminal::DEFAULT_TERM_COLS,
                crate::terminal::DEFAULT_TERM_ROWS,
            ),
            terminal_active: false,
            terminal_status: "Not started".into(),
            remote_serial_ports: Vec::new(),
            tunnels_open: false,
            tunnels: Vec::new(),
            next_tunnel_rule_id: 1,
            sys_open: false,
            sys_snapshot: None,
            sys_status: String::new(),
            broadcast_responses: Vec::new(),
            chat_open: false,
            chat_history: Vec::new(),
            chat_unread: 0,
            chat_input: String::new(),
            popup_open: false,
            popup_title: "Administrator Notice".into(),
            popup_body: String::new(),
            popup_urgent: false,
            popup_status: String::new(),
            voice_open: false,
            voice_active: false,
            voice_status: "Idle".into(),
            voice_controls,
            system_audio_controls,
            system_audio_active: false,
            system_audio_pending: false,
            system_audio_on_connect: None,
            power_open: false,
            power_force: false,
            power_delay_secs: 0,
            power_status: String::new(),
        }
    }

    pub fn send_cmd(&self, cmd: SessionCommand) {
        let _ = self.cmd_tx.try_send(cmd);
    }

    pub fn request_system_audio(&mut self, output_device: String) {
        if self.system_audio_pending
            || self.mode == SessionMode::FileTransfer
            || !matches!(self.phase, ConnectionPhase::Connected)
        {
            return;
        }
        self.system_audio_pending = self.cmd_tx.try_send(SessionCommand::SetSystemAudio {
            enabled: !self.system_audio_active,
            output_device,
        }).is_ok();
    }

    pub fn request_hash_scan(&mut self, root: String, exclude_globs: Vec<String>) -> bool {
        if self.file_transfer.remote_hash_pending || !matches!(self.phase, ConnectionPhase::Connected)
            || self.cmd_tx.try_send(SessionCommand::File(FileTransferMsg::HashTreeReq {
                root, cursor: None, page_size: deskbolt_proto::hash_tree::HASH_PAGE_SIZE, exclude_globs,
            })).is_err()
        {
            return false;
        }
        self.file_transfer.remote_hash_tree = None;
        self.file_transfer.remote_hash_error = None;
        self.file_transfer.remote_hash_pending = true;
        true
    }

    pub fn poll_events(&mut self) -> (Option<(Option<i64>, String)>, Option<ClipboardSync>) {
        let mut pin_to_save = None;
        let mut clip_to_copy = None;
        while let Ok(ev) = self.event_rx.try_recv() {
            match ev {
                SessionUiEvent::PhaseChanged(p) => {
                    self.phase = p.clone();
                    if !matches!(p, ConnectionPhase::Connected) {
                        self.file_transfer.remote_hash_pending = false;
                        self.typing_clipboard = false;
                        self.system_audio_active = false;
                        self.system_audio_pending = false;
                        self.android_controls = false;
                        self.cursors.lock().unwrap().clear();
                        self.monitor_frames.lock().unwrap().clear();
                        self.workspace = crate::monitor_workspace::Workspace::default();
                        self.window_match = crate::monitor_workspace::WindowMatch::default();
                    }
                    if matches!(p, ConnectionPhase::Connected) {
                        if let Some(device) = self.system_audio_on_connect.take() {
                            self.request_system_audio(device);
                        }
                        if self.file_transfer.open {
                            self.send_cmd(SessionCommand::File(FileTransferMsg::ListRootsReq));
                        }
                        if self.sys_open {
                            self.send_cmd(SessionCommand::SysAdmin(SysAdminMsg::SystemSnapshotReq));
                        }
                        if self.terminal_open && !self.terminal_active {
                            self.send_cmd(SessionCommand::Terminal(TerminalMsg::OpenPty {
                                cols: self.terminal_emu.cols,
                                rows: self.terminal_emu.rows,
                                shell_hint: None,
                            }));
                        }
                    }
                }
                SessionUiEvent::ModeUpdated(m) => {
                    self.mode = m;
                    if m != SessionMode::FullControl {
                        self.window_match.refused();
                    }
                }
                SessionUiEvent::PinAccepted { host_id, fingerprint } => {
                    pin_to_save = Some((host_id, fingerprint));
                }
                SessionUiEvent::MonitorsUpdated { monitors, active_id } => {
                    if !monitors.iter().any(|monitor| monitor.id == self.active_monitor_id)
                        || self.monitors.is_empty() || !self.workspace.subscribed()
                    {
                        self.active_monitor_id = active_id;
                    }
                    self.monitors = monitors;
                    self.monitor_frames.lock().unwrap().retain(|id, _| self.monitors.iter().any(|m| m.id == *id));
                    self.cursors.lock().unwrap().retain(|id, _| self.monitors.iter().any(|m| m.id == *id));
                }
                SessionUiEvent::RttUpdated(ms) => {
                    self.last_rtt_ms = Some(ms);
                }
                SessionUiEvent::StatusBanner(msg) => self.status_banner = msg,
                SessionUiEvent::DesktopAction { action, accepted, message } => {
                    if !accepted && matches!(action, deskbolt_proto::SpecialAction::SetDisplayMode { .. }) {
                        self.window_match.refused();
                    }
                    self.status_banner = message;
                }
                SessionUiEvent::RemoteClipboard(message) => {
                    if self.auto_clipboard && self.mode == SessionMode::FullControl {
                        clip_to_copy = Some(message);
                    }
                }
                SessionUiEvent::TypingChanged(active) => self.typing_clipboard = active,
                SessionUiEvent::AndroidControls => self.android_controls = true,
                SessionUiEvent::Cursor(update) => {
                    if let VideoMsg::CursorUpdate { monitor_id, .. } = &update
                        && self.monitors.iter().any(|monitor| monitor.id == *monitor_id)
                    {
                        self.cursors.lock().unwrap().entry(*monitor_id).or_default().update(update);
                    }
                }
                SessionUiEvent::FileRoots(roots) => {
                    let need_initial_dir = self.file_transfer.remote_path.is_empty();
                    if need_initial_dir && let Some(first) = roots.first() {
                        self.file_transfer.remote_path = first.letter.clone();
                        self.send_cmd(SessionCommand::File(FileTransferMsg::ListDirReq {
                            path: first.letter.clone(),
                        }));
                    }
                    self.file_transfer.remote_roots = roots;
                }
                SessionUiEvent::FileDirList { path, entries, error } => {
                    self.file_transfer.remote_path = path;
                    self.file_transfer.remote_entries = entries;
                    self.file_transfer.remote_selected.clear();
                    self.file_transfer.remote_error = error;
                }
                SessionUiEvent::FileActionResult { ok, error } => {
                    if ok {
                        self.file_transfer.remote_error = None;
                        let p = self.file_transfer.remote_path.clone();
                        if !p.is_empty() {
                            self.send_cmd(SessionCommand::File(FileTransferMsg::ListDirReq { path: p }));
                        }
                    } else if let Some(e) = error {
                        self.file_transfer.remote_error = Some(e);
                    }
                }
                SessionUiEvent::TransferProgress(q) => {
                    let any_new_done = q.iter().any(|it| it.status == TransferItemStatus::Verified)
                        && !self.file_transfer.queue.iter().all(|old| old.status == TransferItemStatus::Verified);
                    self.file_transfer.queue = q;
                    if any_new_done {
                        self.file_transfer.refresh_local();
                        let p = self.file_transfer.remote_path.clone();
                        if !p.is_empty() {
                            self.send_cmd(SessionCommand::File(FileTransferMsg::ListDirReq { path: p }));
                        }
                    }
                }
                SessionUiEvent::OverwritePrompt(prompt) => {
                    self.file_transfer.pending_overwrite = prompt;
                }
                SessionUiEvent::HashTree { entries, error } => {
                    self.file_transfer.remote_hash_tree = error.is_none().then_some(entries);
                    self.file_transfer.remote_hash_error = error.clone();
                    self.file_transfer.remote_hash_pending = false;
                    self.file_transfer.hash_tree_error = error;
                }
                SessionUiEvent::TailChunk {
                    path,
                    next_offset,
                    data,
                    truncated,
                    error,
                } => {
                    let tail = self.log_tail.get_or_insert_with(|| LogTailBuffer {
                        remote_path: path.clone(),
                        text: String::new(),
                        next_offset: 0,
                        following: true,
                        status: None,
                    });
                    if tail.remote_path == path {
                        if truncated {
                            tail.text.clear();
                            tail.text.push_str("--- Log truncated or rotated ---\n");
                        }
                        if !data.is_empty() {
                            tail.text.push_str(&String::from_utf8_lossy(&data));
                            if tail.text.len() > 512 * 1024 {
                                let cut = tail.text.len() - 256 * 1024;
                                if let Some(idx) = tail.text[cut..].find('\n') {
                                    tail.text.drain(..cut + idx + 1);
                                }
                            }
                        }
                        tail.next_offset = next_offset;
                        tail.status = error;
                    }
                }
                SessionUiEvent::QuickEditLoaded {
                    path,
                    content,
                    error,
                } => {
                    self.quick_edit = Some(QuickEditBuffer {
                        remote_path: path,
                        content,
                        dirty: false,
                        saving: false,
                        status: error,
                    });
                }
                SessionUiEvent::QuickEditSaved { path, ok, error } => {
                    if let Some(qe) = &mut self.quick_edit
                        && qe.remote_path == path
                    {
                        qe.saving = false;
                        if ok {
                            qe.dirty = false;
                            qe.status = Some("Saved and verified (SHA-256).".into());
                        } else {
                            qe.status = error;
                        }
                    }
                }
                SessionUiEvent::Terminal(tm) => match tm {
                    TerminalMsg::PtyOpened { ok, shell, error } => {
                        self.terminal_active = ok;
                        self.terminal_status = if ok {
                            format!("Active shell: {shell}")
                        } else {
                            error.unwrap_or_else(|| "Failed to open remote shell".into())
                        };
                    }
                    TerminalMsg::PtyData(bytes) | TerminalMsg::SerialData(bytes) => {
                        self.terminal_emu.feed(&bytes);
                    }
                    TerminalMsg::PtyExit(code) => {
                        self.terminal_active = false;
                        self.terminal_status = format!("Shell exited ({code})");
                    }
                    TerminalMsg::ListSerialPortsResp { ports, .. } => {
                        self.remote_serial_ports = ports;
                    }
                    TerminalMsg::SerialOpened { ok, port, error } => {
                        self.terminal_active = ok;
                        self.terminal_status = if ok {
                            format!("Connected serial: {port}")
                        } else {
                            error.unwrap_or_else(|| "Serial open failed".into())
                        };
                    }
                    _ => {}
                },
                SessionUiEvent::TunnelRuleStatus {
                    rule_id,
                    active,
                    status,
                } => {
                    if let Some(r) = self.tunnels.iter_mut().find(|t| t.id == rule_id) {
                        r.active = active;
                        r.status = status;
                    }
                }
                SessionUiEvent::SysAdmin(smsg) => match smsg {
                    SysAdminMsg::SystemSnapshotResp { snapshot, error } => {
                        self.sys_snapshot = Some(snapshot);
                        self.sys_status = error.unwrap_or_else(|| "Snapshot updated".into());
                    }
                    SysAdminMsg::ActionResult { ok, message } => {
                        self.sys_status = message;
                        if ok {
                            self.send_cmd(SessionCommand::SysAdmin(SysAdminMsg::SystemSnapshotReq));
                        }
                    }
                    SysAdminMsg::BroadcastExecResp {
                        command_id,
                        exit_code,
                        output,
                        error,
                    } => {
                        self.broadcast_responses
                            .push((command_id, exit_code, output, error));
                    }
                    _ => {}
                },
                SessionUiEvent::Comms(cmsg) => match cmsg {
                    CommsMsg::ChatMessage {
                        sender,
                        timestamp,
                        text,
                    } => {
                        self.chat_history.push(ChatLine {
                            sender,
                            timestamp,
                            text,
                            outgoing: false,
                        });
                        if !self.chat_open {
                            self.chat_unread = self.chat_unread.saturating_add(1);
                        }
                    }
                    CommsMsg::PopupAck { message, .. } => {
                        self.popup_status = message.clone();
                        self.status_banner = message;
                    }
                    CommsMsg::VoiceAccept => {
                        self.voice_active = true;
                        self.voice_status = "Voice connected".into();
                    }
                    CommsMsg::VoiceEnd => {
                        self.voice_active = false;
                        self.voice_status = "Voice call ended.".into();
                    }
                    _ => {}
                },
                SessionUiEvent::VoiceState { active, status } => {
                    self.voice_active = active;
                    self.voice_status = status;
                }
                SessionUiEvent::SystemAudioActive(active) => {
                    self.system_audio_active = active;
                    self.system_audio_pending = false;
                }
                SessionUiEvent::Power(PowerMsg::PowerAck { accepted, message }) => {
                    self.power_status = if accepted {
                        message.clone()
                    } else {
                        format!("Denied: {message}")
                    };
                    self.status_banner = message;
                }
                SessionUiEvent::Power(_) => {}
            }
        }
        (pin_to_save, clip_to_copy)
    }
}

#[derive(Clone)]
struct UiNotifier {
    tx: std::sync::mpsc::Sender<SessionUiEvent>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl UiNotifier {
    fn send(&self, ev: SessionUiEvent) {
        let _ = self.tx.send(ev);
        (self.repaint)();
    }
}

async fn run_session_task(
    target: ConnectTarget,
    mut cmd_rx: mpsc::Receiver<SessionCommand>,
    ev_tx: std::sync::mpsc::Sender<SessionUiEvent>,
    surfaces: VideoSurfaces,
    video: (Arc<AtomicU64>, SharedVideoStats),
    audio_controls: (Arc<VoiceControls>, Arc<deskbolt_proto::system_audio::SystemAudioControls>),
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    use deskbolt_proto::system_audio::SystemAudioPlayer;

    let (voice_controls, system_audio_controls) = audio_controls;
    let (frame_seq, video_stats) = video;
    let ConnectTarget {
        host_id,
        title: _,
        host,
        port,
        mut mode,
        quality: initial_quality,
        pinned_fingerprint: pinned_fp_str,
    } = target;
    let notifier = UiNotifier {
        tx: ev_tx,
        repaint: repaint.clone(),
    };
    let notify = |ev: SessionUiEvent| notifier.send(ev);

    let video_decoders = if mode == SessionMode::FileTransfer {
        VideoCapabilities::default()
    } else {
        tokio::task::spawn_blocking(platform_video_decoders).await.unwrap_or_default()
    };
    let mut expected_pin = pinned_fp_str.as_deref().and_then(|s| parse_fingerprint(s).ok());
    let addr = format!("{host}:{port}");

    // Step 1: Resolve/verify TLS 1.3 TOFU trust before sending any credentials
    let mut tls_stream = loop {
        let tcp = match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&addr)).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                s
            }
            Ok(Err(e)) => {
                notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                    format!("Could not connect to {addr}: {e}"),
                )));
                return;
            }
            Err(_) => {
                notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                    "Connection timed out after 5 seconds.".into(),
                )));
                return;
            }
        };

        let policy = match expected_pin {
            Some(pin) => TofuPinPolicy::ExpectPinned(pin),
            None => TofuPinPolicy::ProbeFirstUse,
        };
        let verifier = Arc::new(TofuServerCertVerifier::new(policy));

        match connect_tls(tcp, &host, verifier.clone()).await {
            Ok((stream, observed_fp)) => {
                if expected_pin.is_some() {
                    break (stream, observed_fp);
                }
                // First contact: close probe stream cleanly, present TOFU dialog, wait for trust decision
                drop(stream);
                let fp_hex = format_fingerprint(&observed_fp);
                notify(SessionUiEvent::PhaseChanged(ConnectionPhase::TofuPrompt {
                    fingerprint: fp_hex.clone(),
                }));
                match wait_tofu_decision(&mut cmd_rx).await {
                    Some((true, save_pin)) => {
                        expected_pin = Some(observed_fp);
                        if save_pin {
                            notify(SessionUiEvent::PinAccepted {
                                host_id,
                                fingerprint: fp_hex,
                            });
                        }
                        notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Connecting));
                        continue;
                    }
                    _ => {
                        notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                            "Connection cancelled.".into(),
                        )));
                        return;
                    }
                }
            }
            Err(e) => {
                if let (Some(exp), Some(obs)) = (expected_pin, verifier.observed_fingerprint())
                    && exp != obs
                {
                    let exp_hex = format_fingerprint(&exp);
                    let obs_hex = format_fingerprint(&obs);
                    notify(SessionUiEvent::PhaseChanged(ConnectionPhase::TofuMismatch {
                        expected: exp_hex,
                        observed: obs_hex.clone(),
                    }));
                    match wait_tofu_decision(&mut cmd_rx).await {
                        Some((true, save_pin)) => {
                            expected_pin = Some(obs);
                            if save_pin {
                                notify(SessionUiEvent::PinAccepted {
                                    host_id,
                                    fingerprint: obs_hex,
                                });
                            }
                            notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Connecting));
                            continue;
                        }
                        _ => {
                            notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                                "Disconnected due to host key mismatch.".into(),
                            )));
                            return;
                        }
                    }
                }
                notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                    format!("TLS handshake failed: {e}"),
                )));
                return;
            }
        }
    };

    // Step 2: Challenge-response auth (supports retry on wrong password by reconnecting if server closed)
    let mut auth_error: Option<String> = None;
    let mut tls = loop {
        let (mut stream, cert_fp) = tls_stream;
        let Ok(c_nonce) = random_bytes::<NONCE_LEN>() else {
            notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                "System random generator failed.".into(),
            )));
            return;
        };

        let Ok(hello) = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::ClientHello {
                proto_version: PROTO_VERSION,
                client_nonce: c_nonce,
                requested_mode: mode,
                client_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "DeskBoltViewer".into()),
                video_decoders,
            },
            None,
        ) else {
            return;
        };
        if write_frame_async(&mut stream, hello.channel, hello.flags, &hello.payload)
            .await
            .is_err()
        {
            notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                "Connection lost before authentication.".into(),
            )));
            return;
        }

        let ch_frame = match read_frame_async(&mut stream).await {
            Ok(Some(f)) => f,
            _ => {
                notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                    "Connection closed by host during authentication.".into(),
                )));
                return;
            }
        };
        let challenge = match unpack_message::<ControlMsg>(&ch_frame) {
            Ok(ControlMsg::ServerChallenge {
                server_nonce,
                argon2_salt,
                argon2_m_kib,
                argon2_t,
                argon2_p,
            }) => (server_nonce, argon2_salt, argon2_m_kib, argon2_t, argon2_p),
            Ok(ControlMsg::AuthResult { reason, .. }) => {
                notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(reason)));
                return;
            }
            _ => {
                notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                    "Unexpected response from host.".into(),
                )));
                return;
            }
        };

        notify(SessionUiEvent::PhaseChanged(ConnectionPhase::PasswordPrompt {
            error: auth_error.take(),
        }));
        let Some(password) = wait_password(&mut cmd_rx).await else {
            notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                "Connection cancelled.".into(),
            )));
            return;
        };

        let auth_key = match derive_auth_key(
            password.as_bytes(),
            &challenge.1,
            challenge.2,
            challenge.3,
            challenge.4,
        ) {
            Ok(k) => k,
            Err(e) => {
                notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                    format!("Key derivation error: {e}"),
                )));
                return;
            }
        };
        let c_proof = compute_client_proof(&auth_key, &c_nonce, &challenge.0, &cert_fp);
        let Ok(auth_frame) = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::ClientAuth {
                client_proof: c_proof,
            },
            None,
        ) else {
            return;
        };
        if write_frame_async(&mut stream, auth_frame.channel, auth_frame.flags, &auth_frame.payload)
            .await
            .is_err()
        {
            notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                "Authentication timed out. Please try again.".into(),
            )));
            return;
        }

        let res_frame = match read_frame_async(&mut stream).await {
            Ok(Some(f)) => f,
            _ => {
                notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                    "Host closed connection during authentication.".into(),
                )));
                return;
            }
        };
        match unpack_message::<ControlMsg>(&res_frame) {
            Ok(ControlMsg::AuthResult {
                accepted: true,
                server_proof,
                granted_mode,
                ..
            }) => {
                if !verify_server_proof(&auth_key, &challenge.0, &c_nonce, &cert_fp, &server_proof) {
                    notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                        "Host identity verification failed.".into(),
                    )));
                    return;
                }
                if granted_mode != mode {
                    mode = granted_mode;
                    notify(SessionUiEvent::ModeUpdated(granted_mode));
                    notify(SessionUiEvent::StatusBanner(
                        "View Only mode (control held by another viewer)".into(),
                    ));
                }
                break stream;
            }
            Ok(ControlMsg::AuthResult {
                accepted: false,
                retry_after_secs,
                reason,
                ..
            }) => {
                let err_text = if retry_after_secs > 0 {
                    format!("Too many failed attempts. Try again in {retry_after_secs} seconds.")
                } else if reason.is_empty() {
                    "Authentication failed. Check password and try again.".into()
                } else {
                    reason
                };
                auth_error = Some(err_text.clone());
                notify(SessionUiEvent::PhaseChanged(ConnectionPhase::PasswordPrompt {
                    error: Some(err_text),
                }));
                // Re-open fresh socket pinned to verified cert for the next retry
                let Some(pin) = expected_pin else {
                    return;
                };
                let Ok(Ok(tcp2)) =
                    tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&addr)).await
                else {
                    notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                        "Could not reconnect to host.".into(),
                    )));
                    return;
                };
                let _ = tcp2.set_nodelay(true);
                let v2 = Arc::new(TofuServerCertVerifier::new(TofuPinPolicy::ExpectPinned(pin)));
                match connect_tls(tcp2, &host, v2).await {
                    Ok(pair) => tls_stream = pair,
                    Err(e) => {
                        notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                            format!("TLS reconnect failed: {e}"),
                        )));
                        return;
                    }
                }
            }
            _ => {
                notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(
                    "Invalid authentication response from host.".into(),
                )));
                return;
            }
        }
    };

    notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Connected));
    notify(SessionUiEvent::StatusBanner("Connected".into()));

    if mode != SessionMode::FileTransfer
        && let Ok(qframe) = pack_message(
            ChannelId::AuthControl,
            FLAG_NONE,
            &ControlMsg::QualityPreset {
                mode: initial_quality,
                target_fps: 30,
                h264_bitrate_kbps: 4000,
            },
            None,
        )
    {
        let _ = write_frame_async(&mut tls, qframe.channel, qframe.flags, &qframe.payload).await;
    }

    // Spawn dedicated video decode worker thread
    let (vid_tx, vid_rx) = std::sync::mpsc::sync_channel::<(VideoMsg, Instant)>(4);
    let (ack_tx, mut ack_rx) = mpsc::channel::<Frame>(16);
    let dec_latest = surfaces;
    let dec_seq = frame_seq;
    let dec_repaint = repaint.clone();
    let dec_stats = video_stats.clone();
    let (cap_tx, mut cap_rx) = tokio::sync::watch::channel(video_decoders);
    let dec_handle = thread::spawn(move || {
        run_decode_worker(vid_rx, dec_latest, dec_seq, ack_tx, dec_repaint, (dec_stats, cap_tx));
    });

    let (mut reader, mut writer) = tokio::io::split(tls);
    let mut frames = AsyncFrameReader::default();
    let mut ft_client = FileClientQueue::new();
    let mut hash_scan: Option<HashScan> = None;
    let mut next_transfer_id: u64 = 1;
    let mut quick_edit_pending: Option<String> = None;
    let mut quick_edit_uploads: HashMap<u64, (String, PathBuf)> = HashMap::new();
    let (tun_out_tx, mut tun_out_rx) = mpsc::channel::<TunnelMsg>(128);
    let (tun_ev_tx, mut tun_ev_rx) = mpsc::channel::<SessionUiEvent>(32);
    let mut tunnel_bridge = ViewerTunnelBridge::new(tun_out_tx, tun_ev_tx);

    let mut ping_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(10),
        Duration::from_secs(10),
    );
    let mut upload_tick = tokio::time::interval(Duration::from_millis(15));
    let mut typing = VecDeque::new();
    let mut typing_tick = tokio::time::interval(Duration::from_millis(10));
    let mut overwrite_policy = OverwritePolicy::Ask;
    let (voice_tx, mut voice_rx) = mpsc::channel::<Vec<u8>>(64);
    let mut voice_engine: Option<VoiceEngine> = None;
    let mut system_audio_player: Option<SystemAudioPlayer> = None;
    let mut system_audio_active = false;
    let system_audio_timeout = tokio::time::sleep(Duration::ZERO);
    tokio::pin!(system_audio_timeout);

    'session: loop {
        tokio::select! {
            _ = typing_tick.tick(), if !typing.is_empty() => {
                if let Some(ch) = typing.pop_front() {
                    for key in crate::typing::character_events(ch) {
                        if let Ok(frame) = pack_message(ChannelId::Input, FLAG_URGENT, &key, None)
                            && write_frame_async(&mut writer, frame.channel, frame.flags, &frame.payload).await.is_err()
                        {
                            break 'session;
                        }
                    }
                }
                if typing.is_empty() {
                    notify(SessionUiEvent::TypingChanged(false));
                }
            }
            _ = ping_tick.tick() => {
                let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
                if let Ok(f) = pack_message(ChannelId::AuthControl, FLAG_NONE, &ControlMsg::Ping { timestamp_ms: ts }, None) {
                    let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                }
            }
            _ = &mut system_audio_timeout, if system_audio_player.is_some() && !system_audio_active => {
                system_audio_player = None;
                let frame = pack_message(ChannelId::Comms, FLAG_NONE, &CommsMsg::SystemAudioStop, None)
                    .expect("system audio stop has a fixed wire size");
                let _ = write_frame_async(&mut writer, frame.channel, frame.flags, &frame.payload).await;
                notify(SessionUiEvent::SystemAudioActive(false));
                notify(SessionUiEvent::StatusBanner("The host did not start system audio.".into()));
            }
            Ok(()) = cap_rx.changed() => {
                let capabilities = *cap_rx.borrow_and_update();
                if let Ok(frame) = pack_message(
                    ChannelId::AuthControl, FLAG_URGENT, &ControlMsg::VideoCapabilities(capabilities), None,
                ) && write_frame_async(&mut writer, frame.channel, frame.flags, &frame.payload).await.is_err() {
                    break 'session;
                }
            }
            Some(packet) = voice_rx.recv() => {
                if let Ok(f) = pack_message(ChannelId::Comms, FLAG_NONE, &CommsMsg::VoiceFrame(packet), None) {
                    let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                }
            }
            _ = upload_tick.tick() => {
                let msgs = ft_client.pump_uploads(&notifier).await;
                for m in msgs {
                    if let Ok(f) = ft_client.pack_upload(&m)
                        && write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await.is_err()
                    {
                        break;
                    }
                }
            }
            Some(ack) = ack_rx.recv() => {
                let _ = write_frame_async(&mut writer, ack.channel, ack.flags, &ack.payload).await;
            }
            Some(tmsg) = tun_out_rx.recv() => {
                if let Ok(f) = pack_message(ChannelId::Tunnel, FLAG_NONE, &tmsg, None)
                    && write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await.is_err()
                {
                    break;
                }
            }
            Some(ui_ev) = tun_ev_rx.recv() => {
                notify(ui_ev);
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    SessionCommand::Disconnect => {
                        if let Ok(f) = pack_message(ChannelId::AuthControl, FLAG_URGENT, &ControlMsg::Disconnect { reason: "User disconnected".into() }, None) {
                            let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                        }
                        break;
                    }
                    SessionCommand::Input(inp) => {
                        if !typing.is_empty()
                            && matches!(inp, InputMsg::ReleaseAllKeys | InputMsg::Key(deskbolt_proto::KeyEvent { down: true, .. })
                                | InputMsg::Pointer(deskbolt_proto::PointerEvent::Button { down: true, .. }))
                        {
                            typing.clear();
                            notify(SessionUiEvent::TypingChanged(false));
                        }
                        if let Ok(f) = pack_message(ChannelId::Input, FLAG_URGENT, &inp, None)
                            && write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await.is_err()
                        {
                            break;
                        }
                    }
                    SessionCommand::TypeClipboard { text, delay_ms } => {
                        if let Ok(frame) = pack_message(ChannelId::Input, FLAG_URGENT, &InputMsg::ReleaseAllKeys, None)
                            && write_frame_async(&mut writer, frame.channel, frame.flags, &frame.payload).await.is_err()
                        {
                            break;
                        }
                        typing = text.replace("\r\n", "\n").chars().collect();
                        let interval = Duration::from_millis(u64::from(delay_ms.clamp(1, 1000)));
                        typing_tick = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
                        typing_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        notify(SessionUiEvent::TypingChanged(!typing.is_empty()));
                    }
                    SessionCommand::StopTyping => {
                        typing.clear();
                        notify(SessionUiEvent::TypingChanged(false));
                    }
                    SessionCommand::Control(ctrl) => {
                        if matches!(ctrl, ControlMsg::SessionModeSwitch { mode } if mode != SessionMode::FullControl) {
                            typing.clear();
                            notify(SessionUiEvent::TypingChanged(false));
                        }
                        if let Ok(f) = pack_message(ChannelId::AuthControl, FLAG_URGENT, &ctrl, None)
                            && write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await.is_err()
                        {
                            break;
                        }
                    }
                    SessionCommand::Clipboard(clip) => {
                        if let Ok(f) = pack_message(ChannelId::Clipboard, FLAG_NONE, &clip, None)
                            && write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await.is_err()
                        {
                            break;
                        }
                    }
                    SessionCommand::File(ft) => {
                        if let FileTransferMsg::HashTreeReq { root, page_size, exclude_globs, .. } = &ft {
                            hash_scan = Some(HashScan {
                                root: root.clone(), page_size: *page_size, globs: exclude_globs.clone(),
                                entries: Vec::new(), cursor: None,
                            });
                        }
                        if let Ok(f) = pack_message(ChannelId::FileTransfer, FLAG_NONE, &ft, None)
                            && write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await.is_err()
                        {
                            break;
                        }
                    }
                    SessionCommand::QuickEditFetch { remote_path } => {
                        quick_edit_pending = Some(remote_path.clone());
                        let req = FileTransferMsg::TailChunkReq {
                            path: remote_path,
                            offset: 0,
                            max_bytes: 256 * 1024,
                        };
                        if let Ok(f) = pack_message(ChannelId::FileTransfer, FLAG_NONE, &req, None) {
                            let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                        }
                    }
                    SessionCommand::QuickEditSave { remote_path, content } => {
                        let id = next_transfer_id;
                        next_transfer_id += 1;
                        let temp_path = std::env::temp_dir().join(format!(
                            "deskbolt_qe_{}_{}.tmp",
                            std::process::id(),
                            id
                        ));
                        match fs::write(&temp_path, content.as_bytes()).await {
                            Ok(()) => {
                                quick_edit_uploads.insert(id, (remote_path.clone(), temp_path.clone()));
                                if let Some(start_msg) = ft_client
                                    .enqueue_upload(id, temp_path, remote_path, OverwritePolicy::OverwriteAll, &notifier)
                                    .await
                                    && let Ok(f) = pack_message(ChannelId::FileTransfer, FLAG_NONE, &start_msg, None)
                                {
                                    let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                                }
                            }
                            Err(e) => {
                                notify(SessionUiEvent::QuickEditSaved {
                                    path: remote_path,
                                    ok: false,
                                    error: Some(format!("write staging file failed: {e}")),
                                });
                            }
                        }
                    }
                    SessionCommand::Terminal(tm) => {
                        let flags = if matches!(tm, TerminalMsg::PtyData(_) | TerminalMsg::PtyResize { .. } | TerminalMsg::SerialData(_)) {
                            FLAG_URGENT
                        } else {
                            FLAG_NONE
                        };
                        if let Ok(f) = pack_message(ChannelId::Terminal, flags, &tm, None)
                            && write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await.is_err()
                        {
                            break;
                        }
                    }
                    SessionCommand::StartLocalForward { rule_id, local_port, target_host, target_port } => {
                        tunnel_bridge.start_local_forward(rule_id, local_port, target_host, target_port).await;
                    }
                    SessionCommand::StopLocalForward { rule_id } => {
                        tunnel_bridge.stop_local_forward(rule_id).await;
                    }
                    SessionCommand::StartReverseForward { listener_id, bind_port, target_host, target_port } => {
                        if let Some(req) = tunnel_bridge.register_reverse(listener_id, bind_port, target_host, target_port)
                            && let Ok(f) = pack_message(ChannelId::Tunnel, FLAG_NONE, &req, None)
                        {
                            let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                        }
                    }
                    SessionCommand::StopReverseForward { listener_id } => {
                        let req = tunnel_bridge.unregister_reverse(listener_id);
                        if let Ok(f) = pack_message(ChannelId::Tunnel, FLAG_NONE, &req, None) {
                            let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                        }
                    }
                    SessionCommand::SysAdmin(smsg) => {
                        if let Ok(f) = pack_message(ChannelId::SysAdmin, FLAG_NONE, &smsg, None)
                            && write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await.is_err()
                        {
                            break;
                        }
                    }
                    SessionCommand::Comms(cmsg) => {
                        if let Ok(f) = pack_message(ChannelId::Comms, FLAG_NONE, &cmsg, None)
                            && write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await.is_err()
                        {
                            break;
                        }
                    }
                    SessionCommand::Power(pmsg) => {
                        if let Ok(f) = pack_message(ChannelId::PowerControl, FLAG_URGENT, &pmsg, None)
                            && write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await.is_err()
                        {
                            break;
                        }
                    }
                    SessionCommand::StartVoice { input_device, output_device } => {
                        match VoiceEngine::start(
                            input_device,
                            output_device,
                            voice_tx.clone(),
                            voice_controls.clone(),
                        ) {
                            Ok(eng) => {
                                voice_engine = Some(eng);
                                notify(SessionUiEvent::VoiceState {
                                    active: true,
                                    status: "Calling remote host...".into(),
                                });
                                let offer = CommsMsg::VoiceOffer {
                                    sample_rate: VOICE_SAMPLE_RATE,
                                    channels: 1,
                                };
                                if let Ok(f) = pack_message(ChannelId::Comms, FLAG_NONE, &offer, None) {
                                    let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                                }
                            }
                            Err(e) => {
                                notify(SessionUiEvent::VoiceState {
                                    active: false,
                                    status: format!("Audio init failed: {e}"),
                                });
                            }
                        }
                    }
                    SessionCommand::StopVoice => {
                        voice_engine = None;
                        notify(SessionUiEvent::VoiceState {
                            active: false,
                            status: "Voice idle".into(),
                        });
                        if let Ok(f) = pack_message(ChannelId::Comms, FLAG_NONE, &CommsMsg::VoiceEnd, None) {
                            let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                        }
                    }
                    SessionCommand::SetSystemAudio { enabled, output_device } => {
                        let message = if enabled && mode != SessionMode::FileTransfer {
                            match SystemAudioPlayer::start(Some(&output_device), system_audio_controls.clone()) {
                                Ok(player) => {
                                    system_audio_player = Some(player);
                                    system_audio_timeout.as_mut().reset(
                                        tokio::time::Instant::now() + Duration::from_secs(6),
                                    );
                                    CommsMsg::SystemAudioOffer
                                }
                                Err(error) => {
                                    notify(SessionUiEvent::SystemAudioActive(false));
                                    notify(SessionUiEvent::StatusBanner(format!("Could not play system audio: {error}")));
                                    continue;
                                }
                            }
                        } else {
                            system_audio_player = None;
                            system_audio_active = false;
                            notify(SessionUiEvent::SystemAudioActive(false));
                            CommsMsg::SystemAudioStop
                        };
                        let frame = pack_message(ChannelId::Comms, FLAG_NONE, &message, None)
                            .expect("system audio negotiation has a fixed wire size");
                        if write_frame_async(&mut writer, frame.channel, frame.flags, &frame.payload).await.is_err() {
                            break 'session;
                        }
                    }
                    SessionCommand::QueueUpload { local_path, remote_path } => {
                        let id = next_transfer_id;
                        next_transfer_id += 1;
                        if let Some(start_msg) = ft_client.enqueue_upload(id, local_path, remote_path, overwrite_policy, &notifier).await
                            && let Ok(f) = pack_message(ChannelId::FileTransfer, FLAG_NONE, &start_msg, None)
                        {
                            let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                        }
                    }
                    SessionCommand::QueueDownload { remote_path, local_path, total_bytes } => {
                        let id = next_transfer_id;
                        next_transfer_id += 1;
                        if let Some(msg) = ft_client.enqueue_download(id, remote_path, local_path, total_bytes, overwrite_policy, &notifier).await
                            && let Ok(f) = pack_message(ChannelId::FileTransfer, FLAG_NONE, &msg, None)
                        {
                            let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                        }
                    }
                    SessionCommand::ResolveOverwrite { transfer_id, overwrite, apply_all } => {
                        if apply_all {
                            overwrite_policy = if overwrite { OverwritePolicy::OverwriteAll } else { OverwritePolicy::SkipAll };
                        }
                        if let Some(msg) = ft_client.resolve_overwrite(transfer_id, overwrite, &notifier).await
                            && let Ok(f) = pack_message(ChannelId::FileTransfer, FLAG_NONE, &msg, None)
                        {
                            let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                        }
                    }
                    SessionCommand::CancelTransfer(id) => {
                        if let Some(msg) = ft_client.cancel(id, &notifier).await
                            && let Ok(f) = pack_message(ChannelId::FileTransfer, FLAG_NONE, &msg, None)
                        {
                            let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                        }
                    }
                    _ => {}
                }
            }
            frame_res = frames.read(&mut reader) => {
                if let Ok(Some(frame)) = &frame_res {
                    video_stats.lock().unwrap().record_bytes(
                        frame.payload.len() + deskbolt_proto::frame::HEADER_LEN, Instant::now(),
                    );
                }
                match frame_res {
                    Ok(None) => {
                        notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected("Host closed connection.".into())));
                        break;
                    }
                    Ok(Some(frame)) => match frame.channel {
                        ChannelId::VideoTiles => {
                            if let Ok(v) = unpack_message::<VideoMsg>(&frame) {
                                if matches!(v, VideoMsg::CursorUpdate { .. }) {
                                    notify(SessionUiEvent::Cursor(v));
                                } else {
                                    let _ = vid_tx.try_send((v, Instant::now()));
                                }
                            }
                        }
                        ChannelId::AuthControl => {
                            if let Ok(ctrl) = unpack_message::<ControlMsg>(&frame) {
                                match ctrl {
                                    ControlMsg::MonitorList { monitors, active_monitor_id } => {
                                        notify(SessionUiEvent::MonitorsUpdated { monitors, active_id: active_monitor_id });
                                    }
                                    ControlMsg::AndroidControls => notify(SessionUiEvent::AndroidControls),
                                    ControlMsg::Ping { timestamp_ms } => {
                                        if let Ok(pong) = pack_message(ChannelId::AuthControl, FLAG_URGENT, &ControlMsg::Pong { timestamp_ms }, None) {
                                            let _ = write_frame_async(&mut writer, pong.channel, pong.flags, &pong.payload).await;
                                        }
                                    }
                                    ControlMsg::Pong { timestamp_ms } => {
                                        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
                                        if timestamp_ms > 0 && now_ms >= timestamp_ms {
                                            notify(SessionUiEvent::RttUpdated(now_ms - timestamp_ms));
                                        }
                                    }
                                    ControlMsg::DesktopActionResult { action, accepted, message } => {
                                        notify(SessionUiEvent::DesktopAction { action, accepted, message });
                                    }
                                    ControlMsg::ResolutionChange { width, height, .. } => {
                                        notify(SessionUiEvent::StatusBanner(format!("Display {width}x{height}")));
                                    }
                                    ControlMsg::SessionStatus { message, .. } => {
                                        notify(SessionUiEvent::StatusBanner(format!("Desktop: {message}")));
                                    }
                                    ControlMsg::Error { message } => {
                                        notify(SessionUiEvent::StatusBanner(message));
                                    }
                                    ControlMsg::Disconnect { reason } => {
                                        notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(reason)));
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        ChannelId::Clipboard => {
                            if let Ok(message) = unpack_message::<ClipboardSync>(&frame) {
                                notify(SessionUiEvent::RemoteClipboard(message));
                            }
                        }
                        ChannelId::FileTransfer => {
                            if let Ok(ft) = unpack_message::<FileTransferMsg>(&frame) {
                                match ft {
                                    FileTransferMsg::HashTreeResp { root, entries, next_cursor, mut error } => {
                                        if let Some(scan) = hash_scan.as_mut().filter(|scan| scan.root == root) {
                                            if entries.iter().any(|entry| !deskbolt_proto::hash_tree::safe_relative_path(&entry.rel_path))
                                                || next_cursor.as_ref().is_some_and(|next| scan.cursor.as_ref() == Some(next)
                                                    || entries.last().is_none_or(|entry| &entry.rel_path != next))
                                            {
                                                error = Some("Invalid directory manifest page.".into());
                                            }
                                            scan.entries.extend(entries);
                                            if let Some(cursor) = next_cursor.filter(|_| error.is_none()) {
                                                scan.cursor = Some(cursor.clone());
                                                let next = FileTransferMsg::HashTreeReq {
                                                    root, cursor: Some(cursor), page_size: scan.page_size,
                                                    exclude_globs: scan.globs.clone(),
                                                };
                                                if let Ok(f) = pack_message(ChannelId::FileTransfer, FLAG_NONE, &next, None) {
                                                    let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                                                }
                                            } else {
                                                let entries = std::mem::take(&mut scan.entries);
                                                hash_scan = None;
                                                notify(SessionUiEvent::HashTree { entries, error });
                                            }
                                        }
                                    }
                                    FileTransferMsg::TailChunkResp { path, next_offset, data, truncated, error } => {
                                        if quick_edit_pending.take().is_some() {
                                            let content = String::from_utf8_lossy(&data).into_owned();
                                            notify(SessionUiEvent::QuickEditLoaded { path, content, error });
                                        } else {
                                            notify(SessionUiEvent::TailChunk { path, next_offset, data, truncated, error });
                                        }
                                    }
                                    other => {
                                        if let FileTransferMsg::FileVerifyResult { transfer_id, verified, ref error } = other
                                            && let Some((remote_path, temp_path)) = quick_edit_uploads.remove(&transfer_id)
                                        {
                                            let _ = fs::remove_file(&temp_path).await;
                                            notify(SessionUiEvent::QuickEditSaved {
                                                path: remote_path,
                                                ok: verified,
                                                error: error.clone(),
                                            });
                                        }
                                        let replies = ft_client.handle_incoming(other, overwrite_policy, &notifier).await;
                                        for r in replies {
                                            if let Ok(f) = pack_message(ChannelId::FileTransfer, FLAG_NONE, &r, None) {
                                                let _ = write_frame_async(&mut writer, f.channel, f.flags, &f.payload).await;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        ChannelId::Terminal => {
                            if let Ok(tmsg) = unpack_message::<TerminalMsg>(&frame) {
                                notify(SessionUiEvent::Terminal(tmsg));
                            }
                        }
                        ChannelId::Tunnel => {
                            if let Ok(tun_msg) = unpack_message::<TunnelMsg>(&frame) {
                                tunnel_bridge.handle_incoming(tun_msg).await;
                            }
                        }
                        ChannelId::SysAdmin => {
                            if let Ok(smsg) = unpack_message::<SysAdminMsg>(&frame) {
                                notify(SessionUiEvent::SysAdmin(smsg));
                            }
                        }
                        ChannelId::Comms => {
                            if let Ok(cmsg) = unpack_message::<CommsMsg>(&frame) {
                                match cmsg {
                                    CommsMsg::VoiceFrame(bytes) => {
                                        if let Some(eng) = voice_engine.as_ref() {
                                            eng.push_remote_frame(&bytes);
                                        }
                                    }
                                    CommsMsg::VoiceEnd => {
                                        voice_engine = None;
                                        notify(SessionUiEvent::Comms(CommsMsg::VoiceEnd));
                                    }
                                    CommsMsg::SystemAudioOffer => {
                                        if system_audio_player.is_some() {
                                            system_audio_active = true;
                                            notify(SessionUiEvent::SystemAudioActive(true));
                                        }
                                    }
                                    CommsMsg::SystemAudioFrame(packet) => {
                                        if system_audio_active && let Some(player) = &mut system_audio_player {
                                            player.push(&packet);
                                        }
                                    }
                                    CommsMsg::SystemAudioStop => {
                                        system_audio_player = None;
                                        system_audio_active = false;
                                        notify(SessionUiEvent::SystemAudioActive(false));
                                    }
                                    other => notify(SessionUiEvent::Comms(other)),
                                }
                            }
                        }
                        ChannelId::PowerControl => {
                            if let Ok(pmsg) = unpack_message::<PowerMsg>(&frame) {
                                notify(SessionUiEvent::Power(pmsg));
                            }
                        }
                        ChannelId::Input => {}
                    }
                    Err(e) => {
                        notify(SessionUiEvent::PhaseChanged(ConnectionPhase::Disconnected(format!("Connection error: {e}"))));
                        break;
                    }
                }
            }
        }
    }

    drop(voice_engine);
    tunnel_bridge.cleanup_all().await;
    for (_, (_, tmp)) in quick_edit_uploads.drain() {
        let _ = fs::remove_file(tmp).await;
    }
    drop(vid_tx);
    let _ = dec_handle.join();
}

type TunnelWriters = Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>;
type TunnelTasks = Arc<Mutex<HashMap<u32, Vec<JoinHandle<()>>>>>;

struct ViewerTunnelBridge {
    tun_tx: mpsc::Sender<TunnelMsg>,
    ui_tx: mpsc::Sender<SessionUiEvent>,
    listeners: HashMap<u32, JoinHandle<()>>,
    reverse_targets: HashMap<u32, (String, u16)>,
    pending_local: Arc<Mutex<HashMap<u32, TcpStream>>>,
    writers: TunnelWriters,
    tasks: TunnelTasks,
    next_tunnel_id: Arc<std::sync::atomic::AtomicU32>,
}

impl ViewerTunnelBridge {
    fn new(tun_tx: mpsc::Sender<TunnelMsg>, ui_tx: mpsc::Sender<SessionUiEvent>) -> Self {
        Self {
            tun_tx,
            ui_tx,
            listeners: HashMap::new(),
            reverse_targets: HashMap::new(),
            pending_local: Arc::new(Mutex::new(HashMap::new())),
            writers: Arc::new(Mutex::new(HashMap::new())),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            next_tunnel_id: Arc::new(std::sync::atomic::AtomicU32::new(1)),
        }
    }

    async fn start_local_forward(
        &mut self,
        rule_id: u32,
        local_port: u16,
        target_host: String,
        target_port: u16,
    ) {
        self.stop_local_forward(rule_id).await;
        if local_port == 0 || target_host.trim().is_empty() || target_port == 0 {
            let _ = self
                .ui_tx
                .send(SessionUiEvent::TunnelRuleStatus {
                    rule_id,
                    active: false,
                    status: "Invalid local port or remote target".into(),
                })
                .await;
            return;
        }
        match TcpListener::bind(("127.0.0.1", local_port)).await {
            Ok(listener) => {
                let bound = listener.local_addr().map(|a| a.port()).unwrap_or(local_port);
                let _ = self
                    .ui_tx
                    .send(SessionUiEvent::TunnelRuleStatus {
                        rule_id,
                        active: true,
                        status: format!("Listening on 127.0.0.1:{bound}"),
                    })
                    .await;
                let tun_tx = self.tun_tx.clone();
                let pending = self.pending_local.clone();
                let id_gen = self.next_tunnel_id.clone();
                let handle = tokio::spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        let _ = stream.set_nodelay(true);
                        let tunnel_id = id_gen.fetch_add(1, Ordering::Relaxed) & 0x7FFF_FFFF;
                        let tid = tunnel_id.max(1);
                        pending.lock().unwrap().insert(tid, stream);
                        if tun_tx
                            .send(TunnelMsg::TunnelOpen {
                                tunnel_id: tid,
                                target_host: target_host.clone(),
                                target_port,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                self.listeners.insert(rule_id, handle);
            }
            Err(e) => {
                let _ = self
                    .ui_tx
                    .send(SessionUiEvent::TunnelRuleStatus {
                        rule_id,
                        active: false,
                        status: format!("Bind 127.0.0.1:{local_port} failed: {e}"),
                    })
                    .await;
            }
        }
    }

    async fn stop_local_forward(&mut self, rule_id: u32) {
        if let Some(h) = self.listeners.remove(&rule_id) {
            h.abort();
            let _ = self
                .ui_tx
                .send(SessionUiEvent::TunnelRuleStatus {
                    rule_id,
                    active: false,
                    status: "Stopped".into(),
                })
                .await;
        }
    }

    fn register_reverse(
        &mut self,
        listener_id: u32,
        bind_port: u16,
        target_host: String,
        target_port: u16,
    ) -> Option<TunnelMsg> {
        if target_host.trim().is_empty() || target_port == 0 {
            return None;
        }
        self.reverse_targets
            .insert(listener_id, (target_host.clone(), target_port));
        Some(TunnelMsg::ReverseListenReq {
            listener_id,
            bind_port,
            target_host,
            target_port,
        })
    }

    fn unregister_reverse(&mut self, listener_id: u32) -> TunnelMsg {
        self.reverse_targets.remove(&listener_id);
        TunnelMsg::ReverseUnlisten { listener_id }
    }

    async fn handle_incoming(&mut self, msg: TunnelMsg) {
        match msg {
            TunnelMsg::TunnelOpenAck {
                tunnel_id,
                ok,
                error: _,
            } => {
                let stream_opt = self.pending_local.lock().unwrap().remove(&tunnel_id);
                if let Some(stream) = stream_opt
                    && ok
                {
                    attach_viewer_tunnel_stream(
                        tunnel_id,
                        stream,
                        &self.tun_tx,
                        &self.writers,
                        &self.tasks,
                    );
                }
            }
            TunnelMsg::TunnelData { tunnel_id, bytes } => {
                let wr_tx = self.writers.lock().unwrap().get(&tunnel_id).cloned();
                if let Some(tx) = wr_tx
                    && tx.send(bytes).await.is_err()
                {
                    self.close_stream(tunnel_id);
                }
            }
            TunnelMsg::TunnelClose { tunnel_id, .. } => {
                self.close_stream(tunnel_id);
            }
            TunnelMsg::ReverseListenAck {
                listener_id,
                bound_port,
                ok,
                error,
            } => {
                let status = if ok {
                    format!("Remote listening on 127.0.0.1:{bound_port}")
                } else {
                    error.unwrap_or_else(|| "Reverse bind failed".into())
                };
                let _ = self
                    .ui_tx
                    .send(SessionUiEvent::TunnelRuleStatus {
                        rule_id: listener_id,
                        active: ok,
                        status,
                    })
                    .await;
            }
            TunnelMsg::ReverseIncoming {
                tunnel_id,
                listener_id,
            } => {
                if let Some((host, port)) = self.reverse_targets.get(&listener_id).cloned() {
                    let tun_tx = self.tun_tx.clone();
                    let writers = self.writers.clone();
                    let tasks = self.tasks.clone();
                    tokio::spawn(async move {
                        match tokio::time::timeout(
                            Duration::from_secs(8),
                            TcpStream::connect((host.as_str(), port)),
                        )
                        .await
                        {
                            Ok(Ok(stream)) => {
                                let _ = stream.set_nodelay(true);
                                attach_viewer_tunnel_stream(tunnel_id, stream, &tun_tx, &writers, &tasks);
                                let _ = tun_tx
                                    .send(TunnelMsg::TunnelOpenAck {
                                        tunnel_id,
                                        ok: true,
                                        error: None,
                                    })
                                    .await;
                            }
                            _ => {
                                let _ = tun_tx
                                    .send(TunnelMsg::TunnelOpenAck {
                                        tunnel_id,
                                        ok: false,
                                        error: Some(format!("local target {host}:{port} unreachable")),
                                    })
                                    .await;
                            }
                        }
                    });
                }
            }
            _ => {}
        }
    }

    fn close_stream(&self, tunnel_id: u32) {
        self.writers.lock().unwrap().remove(&tunnel_id);
        if let Some(handles) = self.tasks.lock().unwrap().remove(&tunnel_id) {
            for h in handles {
                h.abort();
            }
        }
    }

    async fn cleanup_all(&mut self) {
        for (_, h) in self.listeners.drain() {
            h.abort();
        }
        self.pending_local.lock().unwrap().clear();
        self.writers.lock().unwrap().clear();
        let mut t = self.tasks.lock().unwrap();
        for (_, handles) in t.drain() {
            for h in handles {
                h.abort();
            }
        }
    }
}

fn attach_viewer_tunnel_stream(
    tunnel_id: u32,
    stream: TcpStream,
    tun_tx: &mpsc::Sender<TunnelMsg>,
    writers: &TunnelWriters,
    tasks: &TunnelTasks,
) {
    let (mut rd, mut wr) = stream.into_split();
    let (wr_tx, mut wr_rx) = mpsc::channel::<Vec<u8>>(64);
    writers.lock().unwrap().insert(tunnel_id, wr_tx);

    let tx_rd = tun_tx.clone();
    let writers_rd = writers.clone();
    let tasks_rd = tasks.clone();
    let rd_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        let reason = loop {
            match rd.read(&mut buf).await {
                Ok(0) => break "local EOF".to_string(),
                Ok(n) => {
                    if tx_rd
                        .send(TunnelMsg::TunnelData {
                            tunnel_id,
                            bytes: buf[..n].to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        break "session closed".to_string();
                    }
                }
                Err(e) => break format!("read error: {e}"),
            }
        };
        writers_rd.lock().unwrap().remove(&tunnel_id);
        if let Some(handles) = tasks_rd.lock().unwrap().remove(&tunnel_id) {
            for h in handles {
                h.abort();
            }
        }
        let _ = tx_rd
            .send(TunnelMsg::TunnelClose { tunnel_id, reason })
            .await;
    });

    let wr_task = tokio::spawn(async move {
        while let Some(bytes) = wr_rx.recv().await {
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    tasks.lock().unwrap().insert(tunnel_id, vec![rd_task, wr_task]);
}

async fn wait_tofu_decision(cmd_rx: &mut mpsc::Receiver<SessionCommand>) -> Option<(bool, bool)> {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SessionCommand::TrustTofuAndContinue { save_pin } => return Some((true, save_pin)),
            SessionCommand::CancelConnect | SessionCommand::Disconnect => return Some((false, false)),
            _ => {}
        }
    }
    None
}

async fn wait_password(cmd_rx: &mut mpsc::Receiver<SessionCommand>) -> Option<String> {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            SessionCommand::SubmitPassword(pw) => return Some(pw),
            SessionCommand::CancelConnect | SessionCommand::Disconnect => return None,
            _ => {}
        }
    }
    None
}

struct NativeDecoder {
    spec: (VideoCodec, u16, u16),
    decoder: Box<dyn VideoDecoder>,
}

#[derive(Default)]
struct MonitorDecoder {
    native: Option<NativeDecoder>,
    h264: Option<H264VideoDecoder>,
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

fn run_decode_worker(
    rx: std::sync::mpsc::Receiver<(VideoMsg, Instant)>,
    surfaces: VideoSurfaces,
    frame_seq: Arc<AtomicU64>,
    ack_tx: mpsc::Sender<Frame>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    video: (SharedVideoStats, tokio::sync::watch::Sender<VideoCapabilities>),
) {
    let (stats, supported) = video;
    let mut capabilities = *supported.borrow();
    let mut monitors: HashMap<u8, MonitorDecoder> = HashMap::new();

    while let Ok((msg, queued_at)) = rx.recv() {
        let decode_start = Instant::now();
        let (id, seq, codec, reset) = match &msg {
            VideoMsg::TileBatch { monitor_id, frame_seq, is_full_refresh, .. } =>
                (*monitor_id, *frame_seq, VideoCodec::ZstdTiles, *is_full_refresh),
            VideoMsg::H264Frame { monitor_id, frame_seq, is_keyframe, .. } =>
                (*monitor_id, *frame_seq, VideoCodec::H264Sw, *is_keyframe),
            VideoMsg::EncodedFrame { monitor_id, frame_seq, codec, is_keyframe, .. } =>
                (*monitor_id, *frame_seq, *codec, *is_keyframe),
            VideoMsg::CursorUpdate { .. } => continue,
        };
        if !monitors.contains_key(&id) {
            if !reset {
                continue;
            }
            if monitors.len() >= crate::monitor_workspace::MAX_SURFACES {
                let mut displayed = surfaces.monitors.lock().unwrap();
                let oldest = monitors.keys().copied()
                    .min_by_key(|id| displayed.get(id).map_or(0, |frame| frame.seq));
                if let Some(oldest) = oldest {
                    displayed.remove(&oldest);
                    monitors.remove(&oldest);
                }
            }
        }
        let MonitorDecoder { native, h264, rgba, width: buf_w, height: buf_h } =
            monitors.entry(id).or_default();
        let mut actual = codec;
        let mut updated = false;
        if let VideoMsg::EncodedFrame { width, height, codec, .. } = &msg
            && VideoCodec::HARDWARE.contains(codec)
            && capabilities.includes(*codec)
        {
            let spec = (*codec, *width, *height);
            if native.as_ref().is_none_or(|active| active.spec != spec) {
                *native = platform_video_decoder(*codec, *width, *height).map(|decoder| NativeDecoder { spec, decoder });
            }
            if let Some(Ok(rendered)) = native.as_mut().map(|active| {
                active.decoder.decode(&msg, rgba, buf_w, buf_h)
            }) {
                updated = rendered;
            } else {
                *native = None;
                capabilities.remove(*codec);
                supported.send_replace(capabilities);
            }
        }
        if !updated {
            updated = match codec {
                VideoCodec::ZstdTiles => {
                    *native = None;
                    ZstdVideo.decode(&msg, rgba, buf_w, buf_h).unwrap_or(false)
                }
                VideoCodec::H264Sw | VideoCodec::H264Hw if codec == VideoCodec::H264Sw
                    || !capabilities.includes(VideoCodec::H264Hw) =>
                {
                    *native = None;
                    actual = VideoCodec::H264Sw;
                    if h264.is_none() {
                        *h264 = H264VideoDecoder::new().ok();
                    }
                    h264.as_mut().is_some_and(|decoder| {
                        decoder.decode(&msg, rgba, buf_w, buf_h).unwrap_or(false)
                    })
                }
                _ => false,
            };
        }

        if updated && *buf_w > 0 && *buf_h > 0 {
            let latency = decode_start.elapsed().as_millis().min(u128::from(u16::MAX)) as u16;
            stats.lock().unwrap().decoded(seq, actual.label(), latency);
            let next_seq = frame_seq.fetch_add(1, Ordering::SeqCst) + 1;
            let frame = Arc::new(DecodedFrame { seq: next_seq, width: *buf_w, height: *buf_h, rgba: rgba.clone() });
            surfaces.monitors.lock().unwrap().insert(id, frame.clone());
            *surfaces.latest_frame.lock().unwrap() = Some(frame);
            repaint();

            let decode_ms = queued_at.elapsed().as_millis().min(u16::MAX as u128) as u16;
            if let Ok(ack) = pack_message(
                ChannelId::AuthControl,
                FLAG_NONE,
                &ControlMsg::FrameAck {
                    last_seq: seq,
                    decode_ms,
                },
                None,
            ) {
                let _ = ack_tx.try_send(ack);
            }
        }
    }
}

async fn download_offer(path: &Path, size: u64) -> (u64, Option<[u8; 32]>, u64) {
    match offer(path, DEFAULT_PART_TTL_HOURS).await {
        Some((offset, hash, expected)) if size == 0 || size == expected => (offset, Some(hash), expected),
        _ => (0, None, size),
    }
}

struct HashScan {
    root: String,
    page_size: u32,
    globs: Vec<String>,
    entries: Vec<SyncEntry>,
    cursor: Option<String>,
}

struct ActiveUploadJob {
    path: PathBuf,
    file: File,
    hasher: Context,
    bytes_sent: u64,
    next_seq: u32,
    acked_seq: u32,
    completed_sent: bool,
    started_at: Instant,
}

struct ActiveDownloadJob {
    part: PartialFile,
    started_at: Instant,
}

struct FileClientQueue {
    items: Vec<TransferItem>,
    uploads: HashMap<u64, ActiveUploadJob>,
    downloads: HashMap<u64, ActiveDownloadJob>,
    pending_overwrites: VecDeque<OverwritePrompt>,
    restarted_uploads: std::collections::HashSet<u64>,
}

impl FileClientQueue {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            uploads: HashMap::new(),
            downloads: HashMap::new(),
            pending_overwrites: VecDeque::new(),
            restarted_uploads: std::collections::HashSet::new(),
        }
    }

    fn pack_upload(&self, message: &FileTransferMsg) -> Result<Frame, deskbolt_proto::ProtoError> {
        let source = match message {
            FileTransferMsg::FileChunk { transfer_id, .. } =>
                self.uploads.get(transfer_id).map(|upload| upload.path.as_path()),
            _ => None,
        };
        deskbolt_proto::frame::pack_file_message(message, source)
    }

    fn emit_progress(&self, notify: &UiNotifier) {
        notify.send(SessionUiEvent::TransferProgress(self.items.clone()));
    }

    async fn enqueue_upload(
        &mut self,
        id: u64,
        local_path: PathBuf,
        remote_path: String,
        policy: OverwritePolicy,
        notify: &UiNotifier,
    ) -> Option<FileTransferMsg> {
        let name = local_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let meta = match fs::metadata(&local_path).await {
            Ok(m) if m.is_file() => m,
            _ => {
                self.items.push(TransferItem {
                    id,
                    direction: TransferDir::Upload,
                    file_name: name,
                    local_path,
                    remote_path,
                    total_bytes: 0,
                    bytes_done: 0,
                    speed_bps: 0,
                    status: TransferItemStatus::Failed("Local file unreadable".into()),
                });
                self.emit_progress(notify);
                return None;
            }
        };
        let total = meta.len();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.items.push(TransferItem {
            id,
            direction: TransferDir::Upload,
            file_name: name,
            local_path,
            remote_path: remote_path.clone(),
            total_bytes: total,
            bytes_done: 0,
            speed_bps: 0,
            status: TransferItemStatus::Queued,
        });
        self.emit_progress(notify);
        Some(FileTransferMsg::TransferStart {
            transfer_id: id,
            direction: TransferDir::Upload,
            remote_path,
            total_bytes: total,
            modified_unix: mtime,
            overwrite: policy == OverwritePolicy::OverwriteAll,
            resume_offset: 0,
            prefix_sha256: None,
        })
    }

    async fn enqueue_download(
        &mut self,
        id: u64,
        remote_path: String,
        local_path: PathBuf,
        total_bytes: u64,
        policy: OverwritePolicy,
        notify: &UiNotifier,
    ) -> Option<FileTransferMsg> {
        let name = local_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let exists_locally = local_path.exists();
        self.items.push(TransferItem {
            id,
            direction: TransferDir::Download,
            file_name: name.clone(),
            local_path: local_path.clone(),
            remote_path: remote_path.clone(),
            total_bytes,
            bytes_done: 0,
            speed_bps: 0,
            status: if exists_locally && policy == OverwritePolicy::Ask {
                TransferItemStatus::WaitingOverwrite
            } else {
                TransferItemStatus::Queued
            },
        });
        if exists_locally {
            match policy {
                OverwritePolicy::SkipAll => {
                    if let Some(it) = self.items.iter_mut().find(|i| i.id == id) {
                        it.status = TransferItemStatus::Skipped;
                    }
                    self.emit_progress(notify);
                    return None;
                }
                OverwritePolicy::Ask => {
                    let local_size = fs::metadata(&local_path).await.map(|m| m.len()).unwrap_or(0);
                    let p = OverwritePrompt {
                        transfer_id: id,
                        file_name: name,
                        remote_path,
                        local_size,
                        remote_size: total_bytes,
                    };
                    self.pending_overwrites.push_back(p.clone());
                    if self.pending_overwrites.len() == 1 {
                        notify.send(SessionUiEvent::OverwritePrompt(Some(p)));
                    }
                    self.emit_progress(notify);
                    return None;
                }
                OverwritePolicy::OverwriteAll => {}
            }
        }
        self.emit_progress(notify);
        let (resume_offset, prefix_sha256, total_bytes) = download_offer(&local_path, total_bytes).await;
        Some(FileTransferMsg::TransferStart {
            transfer_id: id,
            direction: TransferDir::Download,
            remote_path,
            total_bytes,
            modified_unix: 0,
            overwrite: true,
            resume_offset,
            prefix_sha256,
        })
    }

    async fn resolve_overwrite(
        &mut self,
        transfer_id: u64,
        overwrite: bool,
        notify: &UiNotifier,
    ) -> Option<FileTransferMsg> {
        self.pending_overwrites.retain(|p| p.transfer_id != transfer_id);
        notify.send(SessionUiEvent::OverwritePrompt(self.pending_overwrites.front().cloned()));

        let item = self.items.iter_mut().find(|i| i.id == transfer_id)?;
        if !overwrite {
            item.status = TransferItemStatus::Skipped;
            self.emit_progress(notify);
            return None;
        }
        item.status = TransferItemStatus::Queued;
        let (resume_offset, prefix_sha256, total_bytes) = if item.direction == TransferDir::Download {
            download_offer(&item.local_path, item.total_bytes).await
        } else { (0, None, item.total_bytes) };
        let msg = FileTransferMsg::TransferStart {
            transfer_id,
            direction: item.direction,
            remote_path: item.remote_path.clone(),
            total_bytes,
            modified_unix: 0,
            overwrite: true,
            resume_offset,
            prefix_sha256,
        };
        self.emit_progress(notify);
        Some(msg)
    }

    async fn cancel(&mut self, transfer_id: u64, notify: &UiNotifier) -> Option<FileTransferMsg> {
        self.uploads.remove(&transfer_id);
        if let Some(dl) = self.downloads.remove(&transfer_id) {
            dl.part.discard().await;
        }
        if let Some(it) = self.items.iter_mut().find(|i| i.id == transfer_id) {
            it.status = TransferItemStatus::Failed("Cancelled".into());
        }
        self.emit_progress(notify);
        Some(FileTransferMsg::FileAbort {
            transfer_id,
            reason: "Cancelled by user".into(),
        })
    }

    async fn pump_uploads(&mut self, notify: &UiNotifier) -> Vec<FileTransferMsg> {
        let mut out = Vec::new();
        let ids: Vec<u64> = self.uploads.keys().copied().collect();
        let mut changed = false;

        for id in ids {
            let Some(up) = self.uploads.get_mut(&id) else {
                continue;
            };
            while !up.completed_sent && up.next_seq.saturating_sub(up.acked_seq) < MAX_UNACKED_CHUNKS {
                let mut buf = vec![0u8; FILE_CHUNK_SIZE];
                match up.file.read(&mut buf).await {
                    Ok(0) => {
                        let d = up.hasher.clone().finish();
                        let mut sha256 = [0u8; 32];
                        sha256.copy_from_slice(d.as_ref());
                        out.push(FileTransferMsg::FileComplete {
                            transfer_id: id,
                            total_bytes: up.bytes_sent,
                            sha256,
                        });
                        up.completed_sent = true;
                        if let Some(it) = self.items.iter_mut().find(|i| i.id == id) {
                            it.status = TransferItemStatus::Verifying;
                        }
                        changed = true;
                    }
                    Ok(n) => {
                        buf.truncate(n);
                        up.hasher.update(&buf);
                        let offset = up.bytes_sent;
                        up.bytes_sent += n as u64;
                        up.next_seq += 1;
                        let elapsed = up.started_at.elapsed().as_secs_f64().max(0.01);
                        let bps = (up.bytes_sent as f64 / elapsed) as u64;
                        if let Some(it) = self.items.iter_mut().find(|i| i.id == id) {
                            it.bytes_done = up.bytes_sent;
                            it.speed_bps = bps;
                            it.status = TransferItemStatus::Transferring;
                        }
                        changed = true;
                        out.push(FileTransferMsg::FileChunk {
                            transfer_id: id,
                            offset,
                            chunk_seq: up.next_seq,
                            data: buf,
                        });
                    }
                    Err(e) => {
                        if let Some(it) = self.items.iter_mut().find(|i| i.id == id) {
                            it.status = TransferItemStatus::Failed(format!("Read error: {e}"));
                        }
                        changed = true;
                        self.uploads.remove(&id);
                        break;
                    }
                }
            }
        }
        if changed {
            self.emit_progress(notify);
        }
        out
    }

    async fn handle_incoming(
        &mut self,
        msg: FileTransferMsg,
        policy: OverwritePolicy,
        notify: &UiNotifier,
    ) -> Vec<FileTransferMsg> {
        let mut replies = Vec::new();
        match msg {
            FileTransferMsg::ListRootsResp { roots } => notify.send(SessionUiEvent::FileRoots(roots)),
            FileTransferMsg::ListDirResp { path, entries, error } => {
                notify.send(SessionUiEvent::FileDirList { path, entries, error });
            }
            FileTransferMsg::MkdirResp { ok, error }
            | FileTransferMsg::DeleteResp { ok, error }
            | FileTransferMsg::RenameResp { ok, error } => {
                notify.send(SessionUiEvent::FileActionResult { ok, error });
            }
            FileTransferMsg::TransferStartAck {
                transfer_id,
                accepted,
                exists,
                existing_size,
                resume_offset,
                prefix_sha256,
                error,
                ..
            } => {
                if let Some(it) = self.items.iter_mut().find(|i| i.id == transfer_id) {
                    if accepted {
                        let result = match it.direction {
                            TransferDir::Upload => {
                                async {
                                    let mut file = File::open(&it.local_path).await?;
                                    let (offset, prefix, hasher) = resume_source(
                                        &mut file, (resume_offset, prefix_sha256), it.total_bytes,
                                    ).await?;
                                    if (offset, prefix) != (resume_offset, prefix_sha256) {
                                        if self.restarted_uploads.insert(transfer_id) {
                                            replies.push(FileTransferMsg::FileAbort {
                                                transfer_id, reason: "Upload prefix changed.".into(),
                                            });
                                            replies.push(FileTransferMsg::TransferStart {
                                                transfer_id, direction: TransferDir::Upload,
                                                remote_path: it.remote_path.clone(), total_bytes: it.total_bytes,
                                                modified_unix: 0, overwrite: policy == OverwritePolicy::OverwriteAll,
                                                resume_offset: 0, prefix_sha256: None,
                                            });
                                            return Ok(());
                                        }
                                        return Err(std::io::Error::other("Host rejected a fresh upload."));
                                    }
                                    it.bytes_done = offset;
                                    it.status = TransferItemStatus::Transferring;
                                    self.uploads.insert(transfer_id, ActiveUploadJob {
                                        path: it.local_path.clone(), file, hasher, bytes_sent: offset,
                                        next_seq: 0, acked_seq: 0, completed_sent: false, started_at: Instant::now(),
                                    });
                                    Ok(())
                                }.await
                            }
                            TransferDir::Download => {
                                async {
                                    if let Some(parent) = it.local_path.parent() {
                                        fs::create_dir_all(parent).await?;
                                    }
                                    let mut part = PartialFile::open(&it.local_path, existing_size, DEFAULT_PART_TTL_HOURS).await?;
                                    if resume_offset == 0 && prefix_sha256.is_none() {
                                        if part.offset() != 0 { part.restart().await?; }
                                    } else if (resume_offset, prefix_sha256) != (part.offset(), part.prefix()) {
                                        return Err(std::io::Error::other("Local partial file changed. Retry the download."));
                                    }
                                    it.total_bytes = existing_size;
                                    it.bytes_done = part.offset();
                                    it.status = TransferItemStatus::Transferring;
                                    self.downloads.insert(transfer_id, ActiveDownloadJob { part, started_at: Instant::now() });
                                    Ok(())
                                }.await
                            }
                        };
                        if let Err(error) = result {
                            it.status = TransferItemStatus::Failed(error.to_string());
                            replies.push(FileTransferMsg::FileAbort { transfer_id, reason: error.to_string() });
                        }
                    } else if exists && it.direction == TransferDir::Upload {
                        match policy {
                            OverwritePolicy::SkipAll => it.status = TransferItemStatus::Skipped,
                            _ => {
                                it.status = TransferItemStatus::WaitingOverwrite;
                                let p = OverwritePrompt {
                                    transfer_id,
                                    file_name: it.file_name.clone(),
                                    remote_path: it.remote_path.clone(),
                                    local_size: it.total_bytes,
                                    remote_size: existing_size,
                                };
                                self.pending_overwrites.push_back(p.clone());
                                if self.pending_overwrites.len() == 1 {
                                    notify.send(SessionUiEvent::OverwritePrompt(Some(p)));
                                }
                            }
                        }
                    } else {
                        it.status = TransferItemStatus::Failed(
                            error.unwrap_or_else(|| "Transfer rejected".into()),
                        );
                    }
                    self.emit_progress(notify);
                }
            }
            FileTransferMsg::FileAck {
                transfer_id,
                acked_seq,
            } => {
                if let Some(up) = self.uploads.get_mut(&transfer_id) {
                    up.acked_seq = up.acked_seq.max(acked_seq);
                }
            }
            FileTransferMsg::FileChunk {
                transfer_id,
                offset,
                chunk_seq,
                data,
            } => {
                if let Some(dl) = self.downloads.get_mut(&transfer_id) {
                    if dl.part.append(offset, &data).await.is_ok() {
                        let bytes_written = dl.part.offset();
                        let elapsed = dl.started_at.elapsed().as_secs_f64().max(0.01);
                        let bps = (bytes_written as f64 / elapsed) as u64;
                        if let Some(it) = self.items.iter_mut().find(|i| i.id == transfer_id) {
                            it.bytes_done = bytes_written;
                            it.speed_bps = bps;
                            it.status = TransferItemStatus::Transferring;
                        }
                        self.emit_progress(notify);
                        replies.push(FileTransferMsg::FileAck {
                            transfer_id,
                            acked_seq: chunk_seq,
                        });
                    } else {
                        let _ = self.cancel(transfer_id, notify).await;
                        replies.push(FileTransferMsg::FileAbort {
                            transfer_id,
                            reason: "chunk write failed".into(),
                        });
                    }
                }
            }
            FileTransferMsg::FileComplete {
                transfer_id,
                total_bytes,
                sha256,
            } => {
                if let Some(dl) = self.downloads.remove(&transfer_id) {
                    let ok = dl.part.finish(total_bytes, sha256).await.is_ok();
                    if let Some(it) = self.items.iter_mut().find(|i| i.id == transfer_id) {
                        it.status = if ok {
                            TransferItemStatus::Verified
                        } else {
                            TransferItemStatus::Failed("SHA-256 verification failed".into())
                        };
                    }
                    self.emit_progress(notify);
                    replies.push(FileTransferMsg::FileVerifyResult {
                        transfer_id,
                        verified: ok,
                        error: (!ok).then(|| "checksum mismatch".into()),
                    });
                }
            }
            FileTransferMsg::FileVerifyResult {
                transfer_id,
                verified,
                error,
            } => {
                self.uploads.remove(&transfer_id);
                if let Some(it) = self.items.iter_mut().find(|i| i.id == transfer_id) {
                    it.status = if verified {
                        TransferItemStatus::Verified
                    } else {
                        TransferItemStatus::Failed(
                            error.unwrap_or_else(|| "SHA-256 verification failed".into()),
                        )
                    };
                }
                self.emit_progress(notify);
            }
            FileTransferMsg::FileAbort {
                transfer_id,
                reason,
            } => {
                self.uploads.remove(&transfer_id);
                if let Some(dl) = self.downloads.remove(&transfer_id) {
                    dl.part.discard().await;
                }
                if let Some(it) = self.items.iter_mut().find(|i| i.id == transfer_id) {
                    it.status = TransferItemStatus::Failed(reason);
                }
                self.emit_progress(notify);
            }
            _ => {}
        }
        replies
    }
}

pub fn list_local_drives() -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        let mut drives = Vec::new();
        // SAFETY: bitmask of mounted drive letters.
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u8 {
            if (mask & (1 << i)) != 0 {
                drives.push(format!("{}:\\", (b'A' + i) as char));
            }
        }
        drives
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut roots = vec!["/".to_string()];
        if let Some(home) = std::env::var_os("HOME") {
            let p = PathBuf::from(home);
            if p.exists() {
                roots.push(p.to_string_lossy().into_owned());
            }
        }
        roots
    }
}

pub fn read_local_dir(path: &Path) -> Result<Vec<FileEntry>, String> {
    let rd = std::fs::read_dir(path).map_err(|e| format!("Cannot read folder: {e}"))?;
    let mut entries = Vec::new();
    for item in rd.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if let Ok(meta) = item.metadata() {
            let modified_unix = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            entries.push(FileEntry {
                name,
                is_dir: meta.is_dir(),
                size: if meta.is_dir() { 0 } else { meta.len() },
                modified_unix,
                readonly: meta.permissions().readonly(),
            });
        }
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

pub fn read_local_clipboard_utf8() -> Option<String> {
    #[cfg(not(target_os = "android"))]
    return deskbolt_proto::native_clipboard::NativeClipboard::open()?
        .text(deskbolt_proto::MAX_CLIPBOARD_BYTES);

    #[cfg(target_os = "android")]
    None
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicBool;

    use deskbolt_proto::{
        H264VideoEncoder, SALT_LEN, TileRect, accept_tls, build_server_config, compute_server_proof,
        derive_auth_key, encode_dirty_tile, generate_self_signed_cert, verify_client_proof,
    };
    use tokio::net::TcpListener;

    use super::*;

    async fn spawn_loopback_host(
        password: &'static str,
        mut post_auth: Option<tokio::sync::oneshot::Sender<deskbolt_proto::ServerTlsStream<TcpStream>>>,
    ) -> (SocketAddr, [u8; 32], tokio::task::JoinHandle<()>) {
        let id = generate_self_signed_cert(&[]).unwrap();
        let cfg = build_server_config(id.cert_der, id.key_der).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fp = id.fingerprint;
        let salt = [0x42u8; SALT_LEN];
        let auth_key = derive_auth_key(password.as_bytes(), &salt, 1024, 1, 1).unwrap();

        let task = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                let Ok(mut tls) = accept_tls(tcp, cfg.clone()).await else {
                    continue;
                };
                let Some(Ok(Some(hello_f))) =
                    tokio::time::timeout(Duration::from_secs(3), read_frame_async(&mut tls)).await.ok()
                else {
                    // TOFU probe closes stream immediately after TLS handshake
                    continue;
                };
                let Ok(ControlMsg::ClientHello { client_nonce, requested_mode, .. }) =
                    unpack_message::<ControlMsg>(&hello_f)
                else {
                    break;
                };
                let s_nonce = random_bytes::<NONCE_LEN>().unwrap();
                let ch = pack_message(
                    ChannelId::AuthControl,
                    FLAG_URGENT,
                    &ControlMsg::ServerChallenge {
                        server_nonce: s_nonce,
                        argon2_salt: salt,
                        argon2_m_kib: 1024,
                        argon2_t: 1,
                        argon2_p: 1,
                    },
                    None,
                )
                .unwrap();
                let _ = write_frame_async(&mut tls, ch.channel, ch.flags, &ch.payload).await;
                let Ok(Some(auth_f)) = read_frame_async(&mut tls).await else {
                    break;
                };
                let Ok(ControlMsg::ClientAuth { client_proof }) = unpack_message::<ControlMsg>(&auth_f) else {
                    break;
                };
                let accepted = verify_client_proof(&auth_key, &client_nonce, &s_nonce, &fp, &client_proof);
                let s_proof = if accepted {
                    compute_server_proof(&auth_key, &s_nonce, &client_nonce, &fp)
                } else {
                    [0u8; 32]
                };
                let res = pack_message(
                    ChannelId::AuthControl,
                    FLAG_URGENT,
                    &ControlMsg::AuthResult {
                        accepted,
                        retry_after_secs: 0,
                        reason: if accepted { String::new() } else { "Authentication failed.".into() },
                        server_proof: s_proof,
                        granted_mode: requested_mode,
                    },
                    None,
                )
                .unwrap();
                let _ = write_frame_async(&mut tls, res.channel, res.flags, &res.payload).await;
                if accepted {
                    if let Some(tx) = post_auth.take() {
                        let _ = tx.send(tls);
                    } else {
                        while let Ok(Some(f)) = read_frame_async(&mut tls).await {
                            if let Ok(ControlMsg::Disconnect { .. }) = unpack_message(&f) {
                                break;
                            }
                        }
                    }
                    break;
                }
            }
        });
        (addr, fp, task)
    }

    async fn wait_phase<F: FnMut(&ConnectionPhase, Option<(Option<i64>, String)>) -> bool>(
        sess: &mut ViewerSession,
        mut pred: F,
    ) -> Option<(Option<i64>, String)> {
        let mut saved = None;
        for _ in 0..200 {
            let (pin, _) = sess.poll_events();
            if pin.is_some() {
                saved = pin.clone();
            }
            if pred(&sess.phase, saved.clone()) {
                return saved;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for phase; current phase={:?}", sess.phase);
    }

    #[tokio::test]
    async fn e2e_viewer_tofu_handshake_mismatch_and_auth_cycle() {
        let (addr, fp, host_task) = spawn_loopback_host("ViewerPass#4", None).await;
        let fp_hex = format_fingerprint(&fp);
        let rt = tokio::runtime::Handle::current();
        let repainted = Arc::new(AtomicBool::new(false));
        let r_flag = repainted.clone();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            r_flag.store(true, Ordering::Relaxed);
        });

        // 1. First contact: TOFU prompt -> Trust + pin -> Wrong password -> Error preserved -> Right password -> Connected
        let mut sess = ViewerSession::start(
            1,
            ConnectTarget {
                host_id: Some(7),
                title: "Host".into(),
                host: "127.0.0.1".into(),
                port: addr.port(),
                mode: SessionMode::FullControl,
                quality: VideoQualityMode::Auto,
                pinned_fingerprint: None,
            },
            &rt,
            repaint.clone(),
        );

        wait_phase(&mut sess, |p, _| matches!(p, ConnectionPhase::TofuPrompt { fingerprint } if fingerprint == &fp_hex)).await;
        sess.send_cmd(SessionCommand::TrustTofuAndContinue { save_pin: true });
        let saved = wait_phase(&mut sess, |p, pin| {
            pin.is_some() && matches!(p, ConnectionPhase::PasswordPrompt { error: None })
        })
        .await;
        assert_eq!(saved, Some((Some(7), fp_hex)));

        sess.send_cmd(SessionCommand::SubmitPassword("WrongPass".into()));
        wait_phase(&mut sess, |p, _| matches!(p, ConnectionPhase::PasswordPrompt { error: Some(e) } if e.contains("failed"))).await;

        sess.send_cmd(SessionCommand::SubmitPassword("ViewerPass#4".into()));
        wait_phase(&mut sess, |p, _| matches!(p, ConnectionPhase::Connected)).await;
        assert!(repainted.load(Ordering::Relaxed));
        sess.send_cmd(SessionCommand::Disconnect);
        let _ = host_task.await;

        // 2. Pinned mismatch must trigger TofuMismatch and allow clean cancel
        let (addr2, _, host_task2) = spawn_loopback_host("ViewerPass#4", None).await;
        let bad_fp = format_fingerprint(&[0xFFu8; 32]);
        let mut sess2 = ViewerSession::start(
            2,
            ConnectTarget {
                host_id: Some(8),
                title: "Mismatch".into(),
                host: "127.0.0.1".into(),
                port: addr2.port(),
                mode: SessionMode::ViewOnly,
                quality: VideoQualityMode::CrispText,
                pinned_fingerprint: Some(bad_fp.clone()),
            },
            &rt,
            repaint,
        );
        wait_phase(&mut sess2, |p, _| matches!(p, ConnectionPhase::TofuMismatch { expected, .. } if expected == &bad_fp)).await;
        sess2.send_cmd(SessionCommand::CancelConnect);
        wait_phase(&mut sess2, |p, _| matches!(p, ConnectionPhase::Disconnected(_))).await;
        host_task2.abort();
    }

    #[tokio::test]
    async fn e2e_viewer_zstd_h264_and_sha256_file_roundtrip() {
        let payload: Vec<u8> = (0..150_000u32).map(|i| ((i * 13 + 5) % 251) as u8).collect();
        let expected_sha = {
            let mut ctx = Context::new(&SHA256);
            ctx.update(&payload);
            let mut out = [0u8; 32];
            out.copy_from_slice(ctx.finish().as_ref());
            out
        };
        let expected_payload = payload.clone();

        let (stream_tx, stream_rx) = tokio::sync::oneshot::channel();
        let (addr, fp, host_task) = spawn_loopback_host("MediaFile#4", Some(stream_tx)).await;
        let expected_payload_c = expected_payload.clone();
        let media_server_task = tokio::spawn(async move {
            let mut tls = stream_rx.await.unwrap();
            let expected_payload = expected_payload_c;
            // Consume QualityPreset sent immediately after auth
            let _ = read_frame_async(&mut tls).await;

            // Send Zstd TileBatch (64x64 BGRA solid [10, 40, 200, 255] => RGBA [200, 40, 10, 255])
            let mut bgra = vec![0u8; 64 * 64 * 4];
            for px in bgra.as_chunks_mut::<4>().0 {
                *px = [10, 40, 200, 255];
            }
            let tile = encode_dirty_tile(
                &bgra,
                64,
                64,
                TileRect { x: 0, y: 0, width: 64, height: 64 },
                3,
            )
            .unwrap();
            let vf1 = pack_message(
                ChannelId::VideoTiles,
                FLAG_NONE,
                &VideoMsg::TileBatch {
                    monitor_id: 0,
                    frame_seq: 1,
                    timestamp_us: 0,
                    width: 64,
                    height: 64,
                    is_full_refresh: true,
                    move_rects: Vec::new(),
                    tiles: vec![tile],
                },
                None,
            )
            .unwrap();
            write_frame_async(&mut tls, vf1.channel, vf1.flags, &vf1.payload).await.unwrap();

            // Wait for viewer FrameAck for seq 1
            loop {
                let f = read_frame_async(&mut tls).await.unwrap().unwrap();
                if let Ok(ControlMsg::FrameAck { last_seq: 1, .. }) = unpack_message(&f) {
                    break;
                }
            }

            // Send OpenH264 frame (64x64 bright green [0, 220, 0, 255])
            for px in bgra.as_chunks_mut::<4>().0 {
                *px = [0, 220, 0, 255];
            }
            let mut enc = H264VideoEncoder::new(64, 64, 1_500_000, 30.0).unwrap();
            enc.force_keyframe();
            let (annex_b, is_kf) = enc.encode_bgra(&bgra, 64, 64).unwrap();
            let vf2 = pack_message(
                ChannelId::VideoTiles,
                FLAG_NONE,
                &VideoMsg::H264Frame {
                    monitor_id: 0,
                    frame_seq: 2,
                    timestamp_us: 1,
                    width: 64,
                    height: 64,
                    is_keyframe: is_kf,
                    annex_b,
                },
                None,
            )
            .unwrap();
            write_frame_async(&mut tls, vf2.channel, vf2.flags, &vf2.payload).await.unwrap();

            // Serve upload + download FileTransfer loop until Disconnect
            let mut uploaded = Vec::new();
            let mut bad_offer = true;
            let mut dl_data: Option<Vec<u8>> = None;
            let mut dl_off = 0usize;
            let mut dl_seq = 0u32;
            let mut dl_id = 0u64;
            loop {
                let Ok(Some(f)) = read_frame_async(&mut tls).await else {
                    break;
                };
                match f.channel {
                    ChannelId::AuthControl => {
                        if let Ok(ControlMsg::Disconnect { .. }) = unpack_message(&f) {
                            break;
                        }
                    }
                    ChannelId::FileTransfer => match unpack_message::<FileTransferMsg>(&f).unwrap() {
                        FileTransferMsg::TransferStart {
                            transfer_id,
                            direction: TransferDir::Upload,
                            ..
                        } => {
                            let ack = pack_message(
                                ChannelId::FileTransfer,
                                FLAG_NONE,
                                &FileTransferMsg::TransferStartAck {
                                    transfer_id,
                                    accepted: true,
                                    exists: false,
                                    existing_size: 0,
                                    existing_modified_unix: 0,
                                    resume_offset: if bad_offer { FILE_CHUNK_SIZE as u64 } else { 0 },
                                    prefix_sha256: bad_offer.then_some([0; 32]),
                                    error: None,
                                },
                                None,
                            )
                            .unwrap();
                            write_frame_async(&mut tls, ack.channel, ack.flags, &ack.payload).await.unwrap();
                            bad_offer = false;
                        }
                        FileTransferMsg::FileChunk { transfer_id, chunk_seq, data, .. } => {
                            uploaded.extend_from_slice(&data);
                            let ack = pack_message(
                                ChannelId::FileTransfer,
                                FLAG_NONE,
                                &FileTransferMsg::FileAck {
                                    transfer_id,
                                    acked_seq: chunk_seq,
                                },
                                None,
                            )
                            .unwrap();
                            write_frame_async(&mut tls, ack.channel, ack.flags, &ack.payload).await.unwrap();
                        }
                        FileTransferMsg::FileComplete { transfer_id, total_bytes, sha256 } => {
                            let mut ctx = Context::new(&SHA256);
                            ctx.update(&uploaded);
                            let ok = uploaded.len() as u64 == total_bytes
                                && ctx.finish().as_ref() == sha256
                                && sha256 == expected_sha
                                && uploaded == expected_payload;
                            let vr = pack_message(
                                ChannelId::FileTransfer,
                                FLAG_NONE,
                                &FileTransferMsg::FileVerifyResult {
                                    transfer_id,
                                    verified: ok,
                                    error: (!ok).then(|| "hash mismatch".into()),
                                },
                                None,
                            )
                            .unwrap();
                            write_frame_async(&mut tls, vr.channel, vr.flags, &vr.payload).await.unwrap();
                        }
                        FileTransferMsg::TransferStart {
                            transfer_id,
                            direction: TransferDir::Download,
                            resume_offset,
                            prefix_sha256,
                            ..
                        } => {
                            dl_id = transfer_id;
                            dl_data = Some(uploaded.clone());
                            assert_eq!(resume_offset, FILE_CHUNK_SIZE as u64);
                            dl_off = resume_offset as usize;
                            assert_eq!(prefix_sha256.unwrap(), ring::digest::digest(&SHA256, &uploaded[..dl_off]).as_ref());
                            dl_seq = 0;
                            let ack = pack_message(
                                ChannelId::FileTransfer,
                                FLAG_NONE,
                                &FileTransferMsg::TransferStartAck {
                                    transfer_id,
                                    accepted: true,
                                    exists: true,
                                    existing_size: uploaded.len() as u64,
                                    existing_modified_unix: 0,
                                    resume_offset,
                                    prefix_sha256,
                                    error: None,
                                },
                                None,
                            )
                            .unwrap();
                            write_frame_async(&mut tls, ack.channel, ack.flags, &ack.payload).await.unwrap();
                            let first = &uploaded[dl_off..uploaded.len().min(dl_off + FILE_CHUNK_SIZE)];
                            dl_off += first.len();
                            dl_seq += 1;
                            let cf = pack_message(
                                ChannelId::FileTransfer,
                                FLAG_NONE,
                                &FileTransferMsg::FileChunk {
                                    transfer_id,
                                    offset: resume_offset,
                                    chunk_seq: dl_seq,
                                    data: first.to_vec(),
                                },
                                None,
                            )
                            .unwrap();
                            write_frame_async(&mut tls, cf.channel, cf.flags, &cf.payload).await.unwrap();
                        }
                        FileTransferMsg::FileAck { .. } => {
                            if let Some(buf) = dl_data.as_ref() {
                                if dl_off < buf.len() {
                                    let end = (dl_off + FILE_CHUNK_SIZE).min(buf.len());
                                    dl_seq += 1;
                                    let cf = pack_message(
                                        ChannelId::FileTransfer,
                                        FLAG_NONE,
                                        &FileTransferMsg::FileChunk {
                                            transfer_id: dl_id,
                                            offset: dl_off as u64,
                                            chunk_seq: dl_seq,
                                            data: buf[dl_off..end].to_vec(),
                                        },
                                        None,
                                    )
                                    .unwrap();
                                    dl_off = end;
                                    write_frame_async(&mut tls, cf.channel, cf.flags, &cf.payload).await.unwrap();
                                } else {
                                    let done = pack_message(
                                        ChannelId::FileTransfer,
                                        FLAG_NONE,
                                        &FileTransferMsg::FileComplete {
                                            transfer_id: dl_id,
                                            total_bytes: buf.len() as u64,
                                            sha256: expected_sha,
                                        },
                                        None,
                                    )
                                    .unwrap();
                                    write_frame_async(&mut tls, done.channel, done.flags, &done.payload).await.unwrap();
                                }
                            }
                        }
                        FileTransferMsg::FileVerifyResult { verified, .. } => {
                            assert!(verified);
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        });
        let rt = tokio::runtime::Handle::current();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});

        let mut sess = ViewerSession::start(
            10,
            ConnectTarget {
                host_id: None,
                title: "MediaTest".into(),
                host: "127.0.0.1".into(),
                port: addr.port(),
                mode: SessionMode::FullControl,
                quality: VideoQualityMode::Auto,
                pinned_fingerprint: Some(format_fingerprint(&fp)),
            },
            &rt,
            repaint,
        );

        wait_phase(&mut sess, |p, _| matches!(p, ConnectionPhase::PasswordPrompt { .. })).await;
        sess.send_cmd(SessionCommand::SubmitPassword("MediaFile#4".into()));
        wait_phase(&mut sess, |p, _| matches!(p, ConnectionPhase::Connected)).await;

        // Verify Zstd TileBatch pixel correctness + H264 decode
        for _ in 0..200 {
            let _ = sess.poll_events();
            if let Some(frame) = sess.latest_frame.lock().unwrap().as_ref()
                && frame.seq == 1
            {
                assert_eq!((frame.width, frame.height), (64, 64));
                assert_eq!(&frame.rgba[0..4], &[200, 40, 10, 255]);
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let mut got_h264 = false;
        for _ in 0..200 {
            let _ = sess.poll_events();
            if let Some(frame) = sess.latest_frame.lock().unwrap().as_ref()
                && frame.seq >= 2
            {
                assert_eq!((frame.width, frame.height), (64, 64));
                assert!(frame.rgba[1] > frame.rgba[0].saturating_add(80));
                assert!(frame.rgba[1] > frame.rgba[2].saturating_add(80));
                got_h264 = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert!(got_h264, "expected H264 decoded frame after Zstd frame");

        // Temp directory for upload & download verification
        let dir = std::env::temp_dir().join(format!("deskbolt_viewer_ft_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src_path = dir.join("upload.bin");
        let dl_path = dir.join("download.bin");
        std::fs::write(&src_path, &payload).unwrap();
        let mut partial = PartialFile::open(&dl_path, payload.len() as u64, DEFAULT_PART_TTL_HOURS).await.unwrap();
        partial.append(0, &payload[..FILE_CHUNK_SIZE]).await.unwrap();
        drop(partial);

        sess.send_cmd(SessionCommand::QueueUpload {
            local_path: src_path,
            remote_path: r"C:\Temp\upload.bin".into(),
        });
        for _ in 0..250 {
            let _ = sess.poll_events();
            if sess
                .file_transfer
                .queue
                .iter()
                .any(|it| it.direction == TransferDir::Upload && it.status == TransferItemStatus::Verified)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            sess.file_transfer
                .queue
                .iter()
                .any(|it| it.direction == TransferDir::Upload && it.status == TransferItemStatus::Verified)
        );

        sess.send_cmd(SessionCommand::QueueDownload {
            remote_path: r"C:\Temp\upload.bin".into(),
            local_path: dl_path.clone(),
            total_bytes: payload.len() as u64,
        });
        for _ in 0..250 {
            let _ = sess.poll_events();
            if sess
                .file_transfer
                .queue
                .iter()
                .any(|it| it.direction == TransferDir::Download && it.status == TransferItemStatus::Verified)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            sess.file_transfer
                .queue
                .iter()
                .any(|it| it.direction == TransferDir::Download && it.status == TransferItemStatus::Verified)
        );
        assert_eq!(std::fs::read(&dl_path).unwrap(), payload);

        let _ = std::fs::remove_dir_all(&dir);
        sess.send_cmd(SessionCommand::Disconnect);
        let _ = host_task.await;
        let _ = media_server_task.await;
    }

    #[tokio::test]
    async fn e2e_viewer_tunnel_forward_hash_tree_and_log_tail() {
        // Real TCP echo target sitting behind the simulated remote host
        let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo_listener.local_addr().unwrap();
        let echo_task = tokio::spawn(async move {
            while let Ok((mut sock, _)) = echo_listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    if let Ok(n) = sock.read(&mut buf).await
                        && n > 0
                    {
                        let _ = sock.write_all(&buf[..n]).await;
                    }
                });
            }
        });

        let (stream_tx, stream_rx) = tokio::sync::oneshot::channel();
        let (addr, fp, host_task) = spawn_loopback_host("TunnelPass!7", Some(stream_tx)).await;
        let server_task = tokio::spawn(async move {
            let tls = stream_rx.await.unwrap();
            let (mut reader, mut writer) = tokio::io::split(tls);
            let (tun_data_tx, mut tun_data_rx) = mpsc::channel::<(u32, Vec<u8>)>(16);

            loop {
                tokio::select! {
                    Some((tid, bytes)) = tun_data_rx.recv() => {
                        let df = pack_message(
                            ChannelId::Tunnel,
                            FLAG_NONE,
                            &TunnelMsg::TunnelData { tunnel_id: tid, bytes },
                            None,
                        ).unwrap();
                        if write_frame_async(&mut writer, df.channel, df.flags, &df.payload).await.is_err() {
                            break;
                        }
                    }
                    res = read_frame_async(&mut reader) => {
                        let Ok(Some(f)) = res else { break };
                        match f.channel {
                            ChannelId::AuthControl => {
                                if let Ok(ControlMsg::Disconnect { .. }) = unpack_message(&f) {
                                    break;
                                }
                            }
                            ChannelId::Tunnel => match unpack_message::<TunnelMsg>(&f).unwrap() {
                                TunnelMsg::TunnelOpen { tunnel_id, target_host, target_port } => {
                                    let sock = TcpStream::connect((target_host.as_str(), target_port)).await.unwrap();
                                    drop(sock);
                                    let ack = pack_message(
                                        ChannelId::Tunnel,
                                        FLAG_NONE,
                                        &TunnelMsg::TunnelOpenAck { tunnel_id, ok: true, error: None },
                                        None,
                                    ).unwrap();
                                    write_frame_async(&mut writer, ack.channel, ack.flags, &ack.payload).await.unwrap();
                                }
                                TunnelMsg::TunnelData { tunnel_id, bytes } => {
                                    let mut sock = TcpStream::connect(echo_addr).await.unwrap();
                                    sock.write_all(&bytes).await.unwrap();
                                    let mut rbuf = vec![0u8; bytes.len()];
                                    sock.read_exact(&mut rbuf).await.unwrap();
                                    let _ = tun_data_tx.send((tunnel_id, rbuf)).await;
                                }
                                _ => {}
                            },
                            ChannelId::FileTransfer => match unpack_message::<FileTransferMsg>(&f).unwrap() {
                                FileTransferMsg::HashTreeReq { root, .. } => {
                                    let resp = pack_message(
                                        ChannelId::FileTransfer,
                                        FLAG_NONE,
                                        &FileTransferMsg::HashTreeResp {
                                            root,
                                            next_cursor: None,
                                            entries: vec![SyncEntry {
                                                rel_path: "app.conf".into(),
                                                is_dir: false,
                                                size: 12,
                                                modified_unix: 100,
                                                sha256: Some([0x33; 32]),
                                            }],
                                            error: None,
                                        },
                                        None,
                                    ).unwrap();
                                    write_frame_async(&mut writer, resp.channel, resp.flags, &resp.payload).await.unwrap();
                                }
                                FileTransferMsg::TailChunkReq { path, offset, .. } => {
                                    let resp = pack_message(
                                        ChannelId::FileTransfer,
                                        FLAG_NONE,
                                        &FileTransferMsg::TailChunkResp {
                                            path,
                                            next_offset: offset + 10,
                                            data: b"tail-line\n".to_vec(),
                                            truncated: false,
                                            error: None,
                                        },
                                        None,
                                    ).unwrap();
                                    write_frame_async(&mut writer, resp.channel, resp.flags, &resp.payload).await.unwrap();
                                }
                                _ => {}
                            },
                            ChannelId::Comms => match unpack_message::<CommsMsg>(&f).unwrap() {
                                CommsMsg::ChatMessage { sender, timestamp, text } => {
                                    let resp = pack_message(
                                        ChannelId::Comms,
                                        FLAG_NONE,
                                        &CommsMsg::ChatMessage {
                                            sender: format!("Host:{sender}"),
                                            timestamp,
                                            text,
                                        },
                                        None,
                                    ).unwrap();
                                    write_frame_async(&mut writer, resp.channel, resp.flags, &resp.payload).await.unwrap();
                                }
                                CommsMsg::PopupMessage { title, .. } => {
                                    let resp = pack_message(
                                        ChannelId::Comms,
                                        FLAG_NONE,
                                        &CommsMsg::PopupAck {
                                            shown: true,
                                            message: format!("Shown: {title}"),
                                        },
                                        None,
                                    ).unwrap();
                                    write_frame_async(&mut writer, resp.channel, resp.flags, &resp.payload).await.unwrap();
                                }
                                _ => {}
                            },
                            ChannelId::PowerControl => {
                                if let PowerMsg::PowerCommand { action, .. } =
                                    unpack_message::<PowerMsg>(&f).unwrap()
                                {
                                    let resp = pack_message(
                                        ChannelId::PowerControl,
                                        FLAG_NONE,
                                        &PowerMsg::PowerAck {
                                            accepted: true,
                                            message: format!("Executed {action:?}"),
                                        },
                                        None,
                                    )
                                    .unwrap();
                                    write_frame_async(&mut writer, resp.channel, resp.flags, &resp.payload)
                                        .await
                                        .unwrap();
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        let rt = tokio::runtime::Handle::current();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let mut sess = ViewerSession::start(
            42,
            ConnectTarget {
                host_id: None,
                title: "TunnelTest".into(),
                host: "127.0.0.1".into(),
                port: addr.port(),
                mode: SessionMode::FullControl,
                quality: VideoQualityMode::Auto,
                pinned_fingerprint: Some(format_fingerprint(&fp)),
            },
            &rt,
            repaint,
        );

        wait_phase(&mut sess, |p, _| matches!(p, ConnectionPhase::PasswordPrompt { .. })).await;
        sess.send_cmd(SessionCommand::SubmitPassword("TunnelPass!7".into()));
        wait_phase(&mut sess, |p, _| matches!(p, ConnectionPhase::Connected)).await;

        // Allocate ephemeral local forward port by binding & releasing or passing fixed free port
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_port = probe.local_addr().unwrap().port();
        drop(probe);

        sess.tunnels.push(TunnelRule {
            id: 1,
            local_port,
            target_host: echo_addr.ip().to_string(),
            target_port: echo_addr.port(),
            reverse: false,
            active: false,
            status: String::new(),
        });
        sess.send_cmd(SessionCommand::StartLocalForward {
            rule_id: 1,
            local_port,
            target_host: echo_addr.ip().to_string(),
            target_port: echo_addr.port(),
        });
        for _ in 0..100 {
            let _ = sess.poll_events();
            if sess.tunnels[0].active {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert!(sess.tunnels[0].active, "local forwarder should become active");

        let mut client_sock = TcpStream::connect(("127.0.0.1", local_port)).await.unwrap();
        client_sock.write_all(b"e2e-tun").await.unwrap();
        let mut got = [0u8; 7];
        tokio::time::timeout(Duration::from_secs(3), client_sock.read_exact(&mut got))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&got, b"e2e-tun");

        // HashTree & TailChunk verify
        sess.send_cmd(SessionCommand::File(FileTransferMsg::HashTreeReq { root: "/etc".into(), cursor: None, page_size: 1, exclude_globs: Vec::new() }));
        sess.send_cmd(SessionCommand::File(FileTransferMsg::TailChunkReq {
            path: "/var/log/app.log".into(),
            offset: 0,
            max_bytes: 1024,
        }));
        for _ in 0..100 {
            let _ = sess.poll_events();
            if sess.file_transfer.remote_hash_tree.is_some() && sess.log_tail.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(sess.file_transfer.remote_hash_tree.as_ref().unwrap()[0].rel_path, "app.conf");
        assert_eq!(sess.log_tail.as_ref().unwrap().text, "tail-line\n");

        // Comms (Chat + Popup) & PowerControl E2E verify
        sess.send_cmd(SessionCommand::Comms(CommsMsg::ChatMessage {
            sender: "Admin".into(),
            timestamp: 1_700_000_000,
            text: "Checking status".into(),
        }));
        sess.send_cmd(SessionCommand::Comms(CommsMsg::PopupMessage {
            title: "Notice".into(),
            body: "Maintenance soon".into(),
            urgent: false,
        }));
        sess.send_cmd(SessionCommand::Power(PowerMsg::PowerCommand {
            action: deskbolt_proto::PowerAction::Lock,
            force: false,
            delay_secs: 0,
        }));
        for _ in 0..100 {
            let _ = sess.poll_events();
            if sess.chat_unread > 0
                && !sess.popup_status.is_empty()
                && !sess.power_status.is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(sess.chat_unread, 1);
        assert_eq!(sess.chat_history[0].sender, "Host:Admin");
        assert_eq!(sess.chat_history[0].text, "Checking status");
        assert_eq!(sess.popup_status, "Shown: Notice");
        assert_eq!(sess.power_status, "Executed Lock");

        sess.send_cmd(SessionCommand::Disconnect);
        let _ = host_task.await;
        let _ = server_task.await;
        echo_task.abort();
    }
}
