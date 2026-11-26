use serde::{Deserialize, Serialize};

/// Session access mode negotiated after authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum SessionMode {
    FullControl = 0,
    ViewOnly = 1,
    FileTransfer = 2,
}

/// Video encoding strategy requested by the Viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum VideoQualityMode {
    Auto = 0,
    CrispText = 1,
    SmoothVideo = 2,
}

/// Current Windows desktop security state reported by the host agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum DesktopState {
    Interactive = 0,
    SecureWinlogon = 1,
    Disconnected = 2,
}

/// Display geometry and DPI descriptor for multi-monitor hosts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: u8,
    pub name: String,
    pub is_primary: bool,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub dpi_scale: f32,
}

/// Privileged host actions invoked over Channel `0x01`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpecialAction {
    SendCtrlAltDel,
    ReleaseAllKeys,
}

/// Channel `0x01` authentication, session lifecycle, and control messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMsg {
    ClientHello {
        proto_version: u16,
        client_nonce: [u8; 32],
        requested_mode: SessionMode,
        client_name: String,
    },
    ServerChallenge {
        server_nonce: [u8; 32],
        argon2_salt: [u8; 16],
        argon2_m_kib: u32,
        argon2_t: u32,
        argon2_p: u32,
    },
    ClientAuth {
        client_proof: [u8; 32],
    },
    AuthResult {
        accepted: bool,
        server_proof: [u8; 32],
        granted_mode: SessionMode,
        retry_after_secs: u16,
        reason: String,
    },
    MonitorList {
        monitors: Vec<MonitorInfo>,
        active_monitor_id: u8,
    },
    MonitorSelect {
        monitor_id: u8,
    },
    ResolutionChange {
        monitor_id: u8,
        width: u16,
        height: u16,
        dpi_scale: f32,
    },
    QualityPreset {
        mode: VideoQualityMode,
        target_fps: u8,
        h264_bitrate_kbps: u32,
    },
    SpecialAction(SpecialAction),
    KeyframeRequest {
        monitor_id: u8,
    },
    SessionStatus {
        desktop_state: DesktopState,
        message: String,
    },
    Ping {
        timestamp_ms: u64,
    },
    Pong {
        timestamp_ms: u64,
    },
    FrameAck {
        last_seq: u64,
        decode_ms: u16,
    },
    SessionModeSwitch {
        mode: SessionMode,
    },
    Error {
        message: String,
    },
    Disconnect {
        reason: String,
    },
    AndroidControls,
}

/// Screen region copied from `(src_x, src_y)` to `(dst_x, dst_y)` before dirty tiles paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveRect {
    pub src_x: u16,
    pub src_y: u16,
    pub dst_x: u16,
    pub dst_y: u16,
    pub width: u16,
    pub height: u16,
}

/// Zstd-compressed tightly packed BGRA8 pixel rectangle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyTile {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub zstd_bgra: Vec<u8>,
}

/// Channel `0x02` hybrid video and tile frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoMsg {
    H264Frame {
        monitor_id: u8,
        frame_seq: u64,
        timestamp_us: u64,
        width: u16,
        height: u16,
        is_keyframe: bool,
        annex_b: Vec<u8>,
    },
    TileBatch {
        monitor_id: u8,
        frame_seq: u64,
        timestamp_us: u64,
        width: u16,
        height: u16,
        is_full_refresh: bool,
        move_rects: Vec<MoveRect>,
        tiles: Vec<DirtyTile>,
    },
    CursorUpdate {
        monitor_id: u8,
        visible: bool,
        x: i16,
        y: i16,
        hotspot_x: u16,
        hotspot_y: u16,
        width: u16,
        height: u16,
        zstd_rgba_shape: Option<Vec<u8>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseBtn {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

/// Mouse/pointer events carrying `[0.0, 1.0]` normalized monitor coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PointerEvent {
    MoveAbs {
        norm_x: f32,
        norm_y: f32,
    },
    Button {
        button: MouseBtn,
        down: bool,
        norm_x: f32,
        norm_y: f32,
    },
    Wheel {
        delta_x: i16,
        delta_y: i16,
    },
}

/// Hardware scan-code accurate keyboard event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub scan_code: u16,
    pub vk_code: u16,
    pub down: bool,
    pub extended: bool,
}

/// Channel `0x03` remote input events.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum InputMsg {
    Pointer(PointerEvent),
    Key(KeyEvent),
    ReleaseAllKeys,
    Unicode(char),
    DeviceAction(DeviceAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceAction {
    Home,
    Back,
    Recents,
    Notifications,
    QuickSettings,
    LockScreen,
}

pub const DEFAULT_CLIPBOARD_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_CLIPBOARD_BYTES: usize = crate::frame::MAX_FRAME_PAYLOAD - 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardMsg {
    Utf8Text(String),
    PngImage(Vec<u8>),
    Html(String),
    FileDropList(Vec<String>),
}

impl ClipboardMsg {
    pub fn payload_bytes(&self) -> usize {
        match self {
            Self::Utf8Text(text) | Self::Html(text) => text.len(),
            Self::PngImage(png) => png.len(),
            Self::FileDropList(paths) => paths.iter().fold(0usize, |bytes, path| {
                bytes.saturating_add(8).saturating_add(path.len())
            }),
        }
    }
}

/// Channel `0x04` bidirectional clipboard synchronization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardSync {
    SetText { seq: u64, utf8_text: String },
    Clear { seq: u64 },
    SetData { seq: u64, data: ClipboardMsg },
}

impl ClipboardSync {
    pub fn set(seq: u64, data: ClipboardMsg) -> Self {
        match data {
            ClipboardMsg::Utf8Text(utf8_text) => Self::SetText { seq, utf8_text },
            data => Self::SetData { seq, data },
        }
    }

    pub fn within_limit(&self, limit: usize) -> bool {
        let bytes = match self {
            Self::SetText { utf8_text, .. } => utf8_text.len(),
            Self::Clear { .. } => 0,
            Self::SetData { data, .. } => data.payload_bytes(),
        };
        bytes <= limit.min(MAX_CLIPBOARD_BYTES)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferDir {
    Upload,
    Download,
}

pub const DESKTOP_UPLOAD_PREFIX: &str = "desktop:";

pub fn valid_drop_filename(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with([' ', '.'])
        || name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return false;
    }
    let stem = name.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return false;
    }
    !["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"))
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveEntry {
    pub letter: String,
    pub label: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_unix: i64,
    pub readonly: bool,
}

/// Channel `0x05` dual-pane file browsing and SHA-256 verified chunk transfer messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileTransferMsg {
    ListRootsReq,
    ListRootsResp {
        roots: Vec<DriveEntry>,
    },
    ListDirReq {
        path: String,
    },
    ListDirResp {
        path: String,
        entries: Vec<FileEntry>,
        error: Option<String>,
    },
    MkdirReq {
        path: String,
    },
    MkdirResp {
        ok: bool,
        error: Option<String>,
    },
    DeleteReq {
        path: String,
        recursive: bool,
    },
    DeleteResp {
        ok: bool,
        error: Option<String>,
    },
    RenameReq {
        from_path: String,
        to_path: String,
    },
    RenameResp {
        ok: bool,
        error: Option<String>,
    },
    TransferStart {
        transfer_id: u64,
        direction: TransferDir,
        remote_path: String,
        total_bytes: u64,
        modified_unix: i64,
        overwrite: bool,
    },
    TransferStartAck {
        transfer_id: u64,
        accepted: bool,
        exists: bool,
        existing_size: u64,
        existing_modified_unix: i64,
        error: Option<String>,
    },
    FileChunk {
        transfer_id: u64,
        offset: u64,
        chunk_seq: u32,
        data: Vec<u8>,
    },
    FileAck {
        transfer_id: u64,
        acked_seq: u32,
    },
    FileComplete {
        transfer_id: u64,
        total_bytes: u64,
        sha256: [u8; 32],
    },
    FileVerifyResult {
        transfer_id: u64,
        verified: bool,
        error: Option<String>,
    },
    FileAbort {
        transfer_id: u64,
        reason: String,
    },
    HashTreeReq {
        path: String,
    },
    HashTreeResp {
        path: String,
        entries: Vec<SyncEntry>,
        error: Option<String>,
    },
    TailChunkReq {
        path: String,
        offset: u64,
        max_bytes: u32,
    },
    TailChunkResp {
        path: String,
        next_offset: u64,
        data: Vec<u8>,
        truncated: bool,
        error: Option<String>,
    },
}

/// Directory item metadata with optional SHA-256 hash for differential sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncEntry {
    pub rel_path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_unix: i64,
    pub sha256: Option<[u8; 32]>,
}

/// Serial port parity mode for remote or local console links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SerialParity {
    None = 0,
    Odd = 1,
    Even = 2,
}

/// Channel `0x06` interactive PTY shell and serial port console messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalMsg {
    OpenPty {
        cols: u16,
        rows: u16,
        shell_hint: Option<String>,
    },
    PtyOpened {
        ok: bool,
        shell: String,
        error: Option<String>,
    },
    PtyData(Vec<u8>),
    PtyResize {
        cols: u16,
        rows: u16,
    },
    PtyExit(i32),
    ClosePty,
    ListSerialPortsReq,
    ListSerialPortsResp {
        ports: Vec<String>,
        error: Option<String>,
    },
    SerialOpen {
        port: String,
        baud_rate: u32,
        data_bits: u8,
        parity: SerialParity,
        stop_bits: u8,
    },
    SerialOpened {
        ok: bool,
        port: String,
        error: Option<String>,
    },
    SerialData(Vec<u8>),
    SerialClose,
}

/// Channel `0x07` encrypted TCP port forwarding (local and remote/reverse) messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TunnelMsg {
    TunnelOpen {
        tunnel_id: u32,
        target_host: String,
        target_port: u16,
    },
    TunnelOpenAck {
        tunnel_id: u32,
        ok: bool,
        error: Option<String>,
    },
    TunnelData {
        tunnel_id: u32,
        bytes: Vec<u8>,
    },
    TunnelClose {
        tunnel_id: u32,
        reason: String,
    },
    ReverseListenReq {
        listener_id: u32,
        bind_port: u16,
        target_host: String,
        target_port: u16,
    },
    ReverseListenAck {
        listener_id: u32,
        bound_port: u16,
        ok: bool,
        error: Option<String>,
    },
    ReverseUnlisten {
        listener_id: u32,
    },
    ReverseIncoming {
        tunnel_id: u32,
        listener_id: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceStateKind {
    Running,
    Stopped,
    Paused,
    Pending,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceOp {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiskStat {
    pub name: String,
    pub mount_point: String,
    pub fs_type: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessStat {
    pub pid: u32,
    pub name: String,
    pub cpu_pct: f32,
    pub memory_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStat {
    pub name: String,
    pub display_name: String,
    pub state: ServiceStateKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemSnapshot {
    pub hostname: String,
    pub os_version: String,
    pub uptime_secs: u64,
    pub cpu_usage_pct: f32,
    pub cpu_cores: u16,
    pub total_mem_bytes: u64,
    pub used_mem_bytes: u64,
    pub disks: Vec<DiskStat>,
    pub processes: Vec<ProcessStat>,
    pub services: Vec<ServiceStat>,
}

/// Channel `0x08` headless system monitoring, process/service actions, and broadcast commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SysAdminMsg {
    SystemSnapshotReq,
    SystemSnapshotResp {
        snapshot: SystemSnapshot,
        error: Option<String>,
    },
    KillProcess(u32),
    ServiceAction {
        name: String,
        action: ServiceOp,
    },
    ActionResult {
        ok: bool,
        message: String,
    },
    BroadcastExecReq {
        command_id: u64,
        command: String,
    },
    BroadcastExecResp {
        command_id: u64,
        exit_code: i32,
        output: String,
        error: Option<String>,
    },
}

/// Channel `0x09` text chat, one-shot desktop popup messages, and bidirectional voice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommsMsg {
    ChatMessage {
        sender: String,
        timestamp: u64,
        text: String,
    },
    PopupMessage {
        title: String,
        body: String,
        urgent: bool,
    },
    PopupAck {
        shown: bool,
        message: String,
    },
    VoiceOffer {
        sample_rate: u32,
        channels: u16,
    },
    VoiceAccept,
    VoiceFrame(Vec<u8>),
    VoiceEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PowerAction {
    Shutdown,
    Restart,
    ColdPowerOff,
    LogOff,
    Sleep,
    Hibernate,
    Lock,
}

/// Channel `0x0A` remote host power state and session lock control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PowerMsg {
    PowerCommand {
        action: PowerAction,
        force: bool,
        delay_secs: u32,
    },
    PowerAck {
        accepted: bool,
        message: String,
    },
    #[cfg(all(feature = "dev-mcp", not(target_os = "android")))]
    SimulatePowerAction(PowerAction),
}

/// Local IPC messages exchanged over `\\.\pipe\DeskBolt_IPC` between Session-0 and `--agent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IpcMsg {
    AgentReady {
        session_id: u32,
        monitors: Vec<MonitorInfo>,
        active_monitor_id: u8,
    },
    MonitorList {
        monitors: Vec<MonitorInfo>,
        active_monitor_id: u8,
    },
    ResolutionChange {
        monitor_id: u8,
        width: u16,
        height: u16,
        dpi_scale: f32,
    },
    DesktopStatus {
        state: DesktopState,
        desktop_name: String,
    },
    Video(VideoMsg),
    Input(InputMsg),
    Clipboard(ClipboardSync),
    QualityPreset {
        mode: VideoQualityMode,
        target_fps: u8,
        h264_bitrate_kbps: u32,
    },
    MonitorSelect {
        monitor_id: u8,
    },
    KeyframeRequest {
        monitor_id: u8,
    },
    SetCurtainMode(bool),
    SetCaptureCursor(bool),
    ShowPopup {
        title: String,
        body: String,
        urgent: bool,
    },
    Shutdown,
    SetClipboardLimit(u32),
}

#[cfg(test)]
mod tests {
    #[test]
    fn android_actions_round_trip_on_input_channel() {
        use crate::{ChannelId, FLAG_NONE, pack_message, unpack_message};
        use super::{DeviceAction, InputMsg};

        for action in [
            DeviceAction::Home,
            DeviceAction::Back,
            DeviceAction::Recents,
            DeviceAction::Notifications,
            DeviceAction::QuickSettings,
            DeviceAction::LockScreen,
        ] {
            let input = InputMsg::DeviceAction(action);
            let frame = pack_message(ChannelId::Input, FLAG_NONE, &input, None).unwrap();
            assert_eq!(unpack_message::<InputMsg>(&frame).unwrap(), input);
        }
    }
    use super::*;
    use crate::frame::{ChannelId, FLAG_NONE, decode_bincode, encode_bincode, pack_message, unpack_message};

    #[test]
    fn rich_clipboard_framing_and_limits() {
        for data in [
            ClipboardMsg::Utf8Text("naïve".into()),
            ClipboardMsg::PngImage(b"\x89PNG\r\n\x1a\n".to_vec()),
            ClipboardMsg::Html("<p>Résumé</p>".into()),
            ClipboardMsg::FileDropList(vec!["/home/user/file.txt".into()]),
        ] {
            let size = data.payload_bytes();
            let message = ClipboardSync::SetData { seq: 1, data };
            assert!(message.within_limit(size));
            assert!(!message.within_limit(size - 1));
            let frame = pack_message(ChannelId::Clipboard, FLAG_NONE, &message, Some(1)).unwrap();
            assert_eq!(unpack_message::<ClipboardSync>(&frame).unwrap(), message);
        }

        let oversized = ClipboardSync::SetData {
            seq: 2,
            data: ClipboardMsg::PngImage(vec![0; MAX_CLIPBOARD_BYTES + 1]),
        };
        assert!(!oversized.within_limit(usize::MAX));
    }

    #[test]
    fn desktop_drop_names_do_not_target_paths_or_devices() {
        assert!(valid_drop_filename("Résumé 2026.png"));
        assert!(valid_drop_filename("report.final.txt"));
        for bad in ["", "..", "../test", r"folder\test", "a.txt:stream", "nul.txt", "COM1", "LPT².txt", "file."] {
            assert!(!valid_drop_filename(bad), "{bad}");
        }
    }

    #[cfg(not(all(feature = "dev-mcp", not(target_os = "android"))))]
    #[test]
    fn default_power_codec_rejects_simulation() {
        let simulated_restart = [2, 0, 0, 0, 1, 0, 0, 0];
        assert!(decode_bincode::<PowerMsg>(&simulated_restart).is_err());
    }

    #[test]
    fn message_bincode_roundtrips() {
        let ctrl = ControlMsg::MonitorList {
            monitors: vec![MonitorInfo {
                id: 0,
                name: "DISPLAY1".into(),
                is_primary: true,
                left: 0,
                top: 0,
                width: 1920,
                height: 1080,
                dpi_scale: 1.0,
            }],
            active_monitor_id: 0,
        };
        assert_eq!(decode_bincode::<ControlMsg>(&encode_bincode(&ctrl).unwrap()).unwrap(), ctrl);

        let inp = InputMsg::Pointer(PointerEvent::Button {
            button: MouseBtn::Left,
            down: true,
            norm_x: 0.5,
            norm_y: 0.25,
        });
        assert_eq!(decode_bincode::<InputMsg>(&encode_bincode(&inp).unwrap()).unwrap(), inp);

        let ft = FileTransferMsg::FileComplete {
            transfer_id: 7,
            total_bytes: 12345,
            sha256: [0xAB; 32],
        };
        assert_eq!(decode_bincode::<FileTransferMsg>(&encode_bincode(&ft).unwrap()).unwrap(), ft);

        let clip = ClipboardSync::SetText {
            seq: 9,
            utf8_text: "hello deskbolt".into(),
        };
        assert_eq!(decode_bincode::<ClipboardSync>(&encode_bincode(&clip).unwrap()).unwrap(), clip);

        let term = TerminalMsg::OpenPty {
            cols: 120,
            rows: 36,
            shell_hint: Some("cmd.exe".into()),
        };
        assert_eq!(decode_bincode::<TerminalMsg>(&encode_bincode(&term).unwrap()).unwrap(), term);

        let tun = TunnelMsg::TunnelData {
            tunnel_id: 42,
            bytes: vec![1, 2, 3, 4],
        };
        assert_eq!(decode_bincode::<TunnelMsg>(&encode_bincode(&tun).unwrap()).unwrap(), tun);

        let sys = SysAdminMsg::ServiceAction {
            name: "DeskBoltSvc".into(),
            action: ServiceOp::Restart,
        };
        assert_eq!(decode_bincode::<SysAdminMsg>(&encode_bincode(&sys).unwrap()).unwrap(), sys);

        for comms in [
            CommsMsg::ChatMessage {
                sender: "Admin".into(),
                timestamp: 1_735_000_000,
                text: "Maintenance starting".into(),
            },
            CommsMsg::PopupMessage {
                title: "Notice".into(),
                body: "Saving work".into(),
                urgent: true,
            },
            CommsMsg::PopupAck {
                shown: true,
                message: "Displayed".into(),
            },
            CommsMsg::VoiceOffer {
                sample_rate: 16_000,
                channels: 1,
            },
            CommsMsg::VoiceAccept,
            CommsMsg::VoiceFrame(vec![0, 1, 255, 128]),
            CommsMsg::VoiceEnd,
        ] {
            assert_eq!(decode_bincode::<CommsMsg>(&encode_bincode(&comms).unwrap()).unwrap(), comms);
        }

        for action in [
            PowerAction::Shutdown,
            PowerAction::Restart,
            PowerAction::ColdPowerOff,
            PowerAction::LogOff,
            PowerAction::Sleep,
            PowerAction::Hibernate,
            PowerAction::Lock,
        ] {
            let p = PowerMsg::PowerCommand {
                action,
                force: true,
                delay_secs: 10,
            };
            assert_eq!(decode_bincode::<PowerMsg>(&encode_bincode(&p).unwrap()).unwrap(), p);
        }
        let ack = PowerMsg::PowerAck {
            accepted: true,
            message: "Scheduled".into(),
        };
        assert_eq!(decode_bincode::<PowerMsg>(&encode_bincode(&ack).unwrap()).unwrap(), ack);
    }
}
