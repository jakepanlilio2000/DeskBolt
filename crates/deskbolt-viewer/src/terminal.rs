use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
use tokio::sync::mpsc;

pub const DEFAULT_TERM_COLS: u16 = 100;
pub const DEFAULT_TERM_ROWS: u16 = 30;
const SCROLLBACK_LINES: usize = 2000;

#[derive(Debug, Clone)]
pub struct SshConnectParams {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub key_path: Option<PathBuf>,
    pub key_passphrase: Option<String>,
    pub expected_fingerprint: Option<String>,
    pub trust_unpinned: bool,
}

#[derive(Debug)]
pub enum SshConnectError {
    TofuConfirmationRequired { fingerprint: String },
    TofuMismatch { expected: String, observed: String },
    Failed(String),
}

impl std::fmt::Display for SshConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TofuConfirmationRequired { fingerprint } => {
                write!(f, "Verify SSH host key fingerprint: {fingerprint}")
            }
            Self::TofuMismatch { expected, observed } => {
                write!(
                    f,
                    "SSH host key changed! Expected {expected}, got {observed}"
                )
            }
            Self::Failed(msg) => f.write_str(msg),
        }
    }
}

struct TofuSshState {
    expected: Option<String>,
    trust_unpinned: bool,
    observed: Option<String>,
    mismatch: bool,
    needs_confirm: bool,
}

pub struct TofuSshHandler {
    state: Arc<Mutex<TofuSshState>>,
}

impl client::Handler for TofuSshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fp = server_public_key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        let mut st = self.state.lock().unwrap();
        st.observed = Some(fp.clone());
        match st.expected.as_deref() {
            Some(exp) if exp == fp => Ok(true),
            Some(_) => {
                st.mismatch = true;
                Ok(false)
            }
            None if st.trust_unpinned => Ok(true),
            None => {
                st.needs_confirm = true;
                Ok(false)
            }
        }
    }
}

/// Establishes and authenticates an encrypted pure-Rust SSH2 (`russh`) session with TOFU pinning.
pub async fn connect_ssh2_session(
    params: &SshConnectParams,
) -> Result<(client::Handle<TofuSshHandler>, String), SshConnectError> {
    if params.host.trim().is_empty() || params.username.trim().is_empty() {
        return Err(SshConnectError::Failed(
            "Host address and SSH username are required.".into(),
        ));
    }
    let state = Arc::new(Mutex::new(TofuSshState {
        expected: params.expected_fingerprint.clone(),
        trust_unpinned: params.trust_unpinned,
        observed: None,
        mismatch: false,
        needs_confirm: false,
    }));
    let handler = TofuSshHandler {
        state: state.clone(),
    };
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        keepalive_interval: Some(Duration::from_secs(15)),
        ..Default::default()
    });

    let addr = (params.host.trim(), params.port.max(1));
    let mut handle = match tokio::time::timeout(
        Duration::from_secs(10),
        client::connect(config, addr, handler),
    )
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            let st = state.lock().unwrap();
            if st.mismatch {
                return Err(SshConnectError::TofuMismatch {
                    expected: st.expected.clone().unwrap_or_default(),
                    observed: st.observed.clone().unwrap_or_default(),
                });
            }
            if st.needs_confirm {
                return Err(SshConnectError::TofuConfirmationRequired {
                    fingerprint: st.observed.clone().unwrap_or_default(),
                });
            }
            return Err(SshConnectError::Failed(format!("SSH connect failed: {e}")));
        }
        Err(_) => {
            return Err(SshConnectError::Failed("SSH connection timed out.".into()));
        }
    };

    let fp = state
        .lock()
        .unwrap()
        .observed
        .clone()
        .unwrap_or_default();

    let mut authed = false;
    if let Some(key_path) = &params.key_path
        && !key_path.as_os_str().is_empty()
    {
        let priv_key = load_secret_key(
            key_path,
            params.key_passphrase.as_deref().filter(|p| !p.is_empty()),
        )
        .map_err(|e| SshConnectError::Failed(format!("Load private key failed: {e}")))?;

        let rsa_hash = handle
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten();
        let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(priv_key), rsa_hash);
        let auth_res = handle
            .authenticate_publickey(params.username.trim(), key_with_alg)
            .await
            .map_err(|e| SshConnectError::Failed(format!("SSH key auth error: {e}")))?;
        authed = auth_res.success();
    }

    if !authed && !params.password.is_empty() {
        let auth_res = handle
            .authenticate_password(params.username.trim(), &params.password)
            .await
            .map_err(|e| SshConnectError::Failed(format!("SSH password auth error: {e}")))?;
        authed = auth_res.success();
    }

    if !authed {
        return Err(SshConnectError::Failed(
            "SSH authentication rejected. Verify username, password, or private key.".into(),
        ));
    }

    Ok((handle, fp))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectTerminalKind {
    Ssh2,
    Serial {
        port: String,
        baud_rate: u32,
        data_bits: u8,
        parity: deskbolt_proto::SerialParity,
        stop_bits: u8,
    },
}

pub enum DirectTerminalCmd {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

#[derive(Debug)]
pub enum DirectTerminalEvent {
    Connecting,
    TofuConfirm(String),
    TofuMismatch { expected: String, observed: String },
    NeedCredentials(Option<String>),
    Ready(String),
    Data(Vec<u8>),
    Exited(String),
}

/// Interactive VT100 screen parser with optional timestamped session logging.
pub struct TerminalEmulator {
    pub parser: vt100::Parser,
    pub cols: u16,
    pub rows: u16,
    pub scroll_offset: usize,
    pub log_path: Option<PathBuf>,
    log_file: Option<std::fs::File>,
}

impl TerminalEmulator {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_LINES),
            cols,
            rows,
            scroll_offset: 0,
            log_path: None,
            log_file: None,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        if let Some(file) = &mut self.log_file {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let text = String::from_utf8_lossy(bytes);
            let _ = writeln!(file, "[{now}] {text}");
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let cols = cols.clamp(20, 400);
        let rows = rows.clamp(4, 200);
        if self.cols == cols && self.rows == rows {
            return false;
        }
        self.cols = cols;
        self.rows = rows;
        self.parser.set_size(rows, cols);
        true
    }

    pub fn set_scrollback(&mut self, rows: usize) {
        self.scroll_offset = rows.min(SCROLLBACK_LINES);
        self.parser.set_scrollback(self.scroll_offset);
    }

    pub fn toggle_session_log(&mut self, path: &Path) -> Result<bool, String> {
        if self.log_file.is_some() {
            self.log_file = None;
            self.log_path = None;
            return Ok(false);
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| format!("create log dir: {e}"))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("open session log failed: {e}"))?;
        self.log_path = Some(path.to_path_buf());
        self.log_file = Some(file);
        Ok(true)
    }

    pub fn screen_text(&self) -> String {
        self.parser.screen().contents()
    }
}

/// Converts a `vt100::Color` cell attribute into an `egui::Color32`.
pub fn vt_color_to_egui(c: vt100::Color, is_bg: bool) -> egui::Color32 {
    match c {
        vt100::Color::Default => {
            if is_bg {
                egui::Color32::from_rgb(16, 19, 24)
            } else {
                egui::Color32::from_rgb(228, 232, 238)
            }
        }
        vt100::Color::Rgb(r, g, b) => egui::Color32::from_rgb(r, g, b),
        vt100::Color::Idx(idx) => ansi_idx_to_color(idx),
    }
}

fn ansi_idx_to_color(idx: u8) -> egui::Color32 {
    const PALETTE: [(u8, u8, u8); 16] = [
        (24, 26, 31),
        (224, 108, 117),
        (152, 195, 121),
        (229, 192, 123),
        (97, 175, 239),
        (198, 120, 221),
        (86, 182, 194),
        (215, 218, 224),
        (92, 99, 112),
        (244, 135, 113),
        (181, 232, 144),
        (247, 214, 142),
        (130, 198, 255),
        (218, 152, 242),
        (124, 214, 226),
        (255, 255, 255),
    ];
    if (idx as usize) < PALETTE.len() {
        let (r, g, b) = PALETTE[idx as usize];
        return egui::Color32::from_rgb(r, g, b);
    }
    if idx < 232 {
        let n = idx - 16;
        let r = n / 36;
        let g = (n % 36) / 6;
        let b = n % 6;
        let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        return egui::Color32::from_rgb(scale(r), scale(g), scale(b));
    }
    let gray = 8 + (idx - 232) * 10;
    egui::Color32::from_rgb(gray, gray, gray)
}

/// Maps `egui::Key` + modifiers into ANSI/VT100 terminal byte sequences.
pub fn egui_key_to_vt_bytes(key: egui::Key, modifiers: &egui::Modifiers) -> Option<Vec<u8>> {
    if modifiers.ctrl && !modifiers.alt {
        let ctrl_byte = match key {
            egui::Key::A => Some(0x01),
            egui::Key::B => Some(0x02),
            egui::Key::C => Some(0x03),
            egui::Key::D => Some(0x04),
            egui::Key::E => Some(0x05),
            egui::Key::F => Some(0x06),
            egui::Key::K => Some(0x0B),
            egui::Key::L => Some(0x0C),
            egui::Key::N => Some(0x0E),
            egui::Key::P => Some(0x10),
            egui::Key::R => Some(0x12),
            egui::Key::U => Some(0x15),
            egui::Key::W => Some(0x17),
            egui::Key::Z => Some(0x1A),
            _ => None,
        };
        if let Some(b) = ctrl_byte {
            return Some(vec![b]);
        }
    }

    let bytes: &[u8] = match key {
        egui::Key::Enter => b"\r",
        egui::Key::Tab => b"\t",
        egui::Key::Backspace => b"\x7f",
        egui::Key::Escape => b"\x1b",
        egui::Key::ArrowUp => b"\x1b[A",
        egui::Key::ArrowDown => b"\x1b[B",
        egui::Key::ArrowRight => b"\x1b[C",
        egui::Key::ArrowLeft => b"\x1b[D",
        egui::Key::Home => b"\x1b[H",
        egui::Key::End => b"\x1b[F",
        egui::Key::Insert => b"\x1b[2~",
        egui::Key::Delete => b"\x1b[3~",
        egui::Key::PageUp => b"\x1b[5~",
        egui::Key::PageDown => b"\x1b[6~",
        egui::Key::F1 => b"\x1bOP",
        egui::Key::F2 => b"\x1bOQ",
        egui::Key::F3 => b"\x1bOR",
        egui::Key::F4 => b"\x1bOS",
        egui::Key::F5 => b"\x1b[15~",
        egui::Key::F6 => b"\x1b[17~",
        egui::Key::F7 => b"\x1b[18~",
        egui::Key::F8 => b"\x1b[19~",
        egui::Key::F9 => b"\x1b[20~",
        egui::Key::F10 => b"\x1b[21~",
        egui::Key::F11 => b"\x1b[23~",
        egui::Key::F12 => b"\x1b[24~",
        _ => return None,
    };
    Some(bytes.to_vec())
}

/// Spawns an independent pure-Rust SSH2 interactive shell session over Tokio.
pub fn spawn_direct_ssh_terminal(
    runtime: &tokio::runtime::Handle,
    params: SshConnectParams,
    cols: u16,
    rows: u16,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> (
    mpsc::Sender<DirectTerminalCmd>,
    std::sync::mpsc::Receiver<DirectTerminalEvent>,
) {
    let (cmd_tx, mut cmd_rx) = mpsc::channel(64);
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    let notify = move |ev: DirectTerminalEvent| {
        let _ = ev_tx.send(ev);
        repaint();
    };

    runtime.spawn(async move {
        notify(DirectTerminalEvent::Connecting);
        let (handle, fp) = match connect_ssh2_session(&params).await {
            Ok(pair) => pair,
            Err(SshConnectError::TofuConfirmationRequired { fingerprint }) => {
                notify(DirectTerminalEvent::TofuConfirm(fingerprint));
                return;
            }
            Err(SshConnectError::TofuMismatch { expected, observed }) => {
                notify(DirectTerminalEvent::TofuMismatch { expected, observed });
                return;
            }
            Err(SshConnectError::Failed(msg)) => {
                notify(DirectTerminalEvent::Exited(msg));
                return;
            }
        };

        let mut channel = match handle.channel_open_session().await {
            Ok(c) => c,
            Err(e) => {
                notify(DirectTerminalEvent::Exited(format!(
                    "Open SSH session channel failed: {e}"
                )));
                return;
            }
        };
        if let Err(e) = channel
            .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
            .await
        {
            notify(DirectTerminalEvent::Exited(format!(
                "SSH PTY request failed: {e}"
            )));
            return;
        }
        if let Err(e) = channel.request_shell(false).await {
            notify(DirectTerminalEvent::Exited(format!(
                "SSH shell request failed: {e}"
            )));
            return;
        }
        notify(DirectTerminalEvent::Ready(fp));

        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    match cmd {
                        DirectTerminalCmd::Write(bytes) => {
                            if channel.data(bytes.as_slice()).await.is_err() {
                                break;
                            }
                        }
                        DirectTerminalCmd::Resize { cols, rows } => {
                            let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
                        }
                        DirectTerminalCmd::Close => {
                            let _ = channel.eof().await;
                            break;
                        }
                    }
                }
                msg = channel.wait() => {
                    match msg {
                        Some(russh::ChannelMsg::Data { data }) => {
                            notify(DirectTerminalEvent::Data(data.to_vec()));
                        }
                        Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                            notify(DirectTerminalEvent::Data(data.to_vec()));
                        }
                        Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                            notify(DirectTerminalEvent::Exited(format!(
                                "SSH session exited ({exit_status})."
                            )));
                            break;
                        }
                        Some(russh::ChannelMsg::Close | russh::ChannelMsg::Eof) | None => {
                            notify(DirectTerminalEvent::Exited("SSH session closed.".into()));
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    });

    (cmd_tx, ev_rx)
}

/// Spawns a local serial port console thread (desktop targets) or reports unsupported on Android.
pub fn spawn_local_serial_terminal(
    port_name: String,
    baud_rate: u32,
    data_bits: u8,
    parity: deskbolt_proto::SerialParity,
    stop_bits: u8,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> (
    mpsc::Sender<DirectTerminalCmd>,
    std::sync::mpsc::Receiver<DirectTerminalEvent>,
) {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<DirectTerminalCmd>(64);
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();

    #[cfg(target_os = "android")]
    {
        let _ = (port_name, baud_rate, data_bits, parity, stop_bits, &mut cmd_rx);
        let _ = ev_tx.send(DirectTerminalEvent::Exited(
            "Direct local serial ports are not supported on Android.".into(),
        ));
        repaint();
    }

    #[cfg(not(target_os = "android"))]
    {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicBool, Ordering};

        std::thread::spawn(move || {
            let db = match data_bits {
                5 => serialport::DataBits::Five,
                6 => serialport::DataBits::Six,
                7 => serialport::DataBits::Seven,
                _ => serialport::DataBits::Eight,
            };
            let par = match parity {
                deskbolt_proto::SerialParity::Odd => serialport::Parity::Odd,
                deskbolt_proto::SerialParity::Even => serialport::Parity::Even,
                deskbolt_proto::SerialParity::None => serialport::Parity::None,
            };
            let sb = if stop_bits == 2 {
                serialport::StopBits::Two
            } else {
                serialport::StopBits::One
            };

            let mut sp = match serialport::new(port_name.trim(), baud_rate.max(300))
                .data_bits(db)
                .parity(par)
                .stop_bits(sb)
                .timeout(Duration::from_millis(40))
                .open()
            {
                Ok(p) => p,
                Err(e) => {
                    let _ = ev_tx.send(DirectTerminalEvent::Exited(format!(
                        "Open serial {port_name} failed: {e}"
                    )));
                    repaint();
                    return;
                }
            };
            let mut reader = match sp.try_clone() {
                Ok(r) => r,
                Err(e) => {
                    let _ = ev_tx.send(DirectTerminalEvent::Exited(format!(
                        "Serial clone failed: {e}"
                    )));
                    repaint();
                    return;
                }
            };
            let _ = ev_tx.send(DirectTerminalEvent::Ready(format!(
                "{} ({baud_rate} baud)",
                port_name.trim()
            )));
            repaint();

            let stop = Arc::new(AtomicBool::new(false));
            let stop_rd = stop.clone();
            let ev_rd = ev_tx.clone();
            let repaint_rd = repaint.clone();
            let rd_thread = std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop_rd.load(Ordering::Relaxed) {
                    match reader.read(&mut buf) {
                        Ok(0) => continue,
                        Ok(n) => {
                            if ev_rd.send(DirectTerminalEvent::Data(buf[..n].to_vec())).is_err() {
                                break;
                            }
                            repaint_rd();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                        Err(_) => break,
                    }
                }
            });

            while let Some(cmd) = cmd_rx.blocking_recv() {
                match cmd {
                    DirectTerminalCmd::Write(bytes) => {
                        let _ = sp.write_all(&bytes);
                        let _ = sp.flush();
                    }
                    DirectTerminalCmd::Resize { .. } => {}
                    DirectTerminalCmd::Close => break,
                }
            }
            stop.store(true, Ordering::Relaxed);
            let _ = rd_thread.join();
            let _ = ev_tx.send(DirectTerminalEvent::Exited("Serial port closed.".into()));
            repaint();
        });
    }

    (cmd_tx, ev_rx)
}

#[cfg(not(target_os = "android"))]
pub fn list_local_serial_ports() -> Vec<String> {
    serialport::available_ports()
        .map(|v| v.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default()
}

#[cfg(target_os = "android")]
pub fn list_local_serial_ports() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vt100_emulator_processes_ansi_resize_and_keys() {
        let mut emu = TerminalEmulator::new(40, 10);
        emu.feed(b"\x1b[32mhello\x1b[0m\r\nworld");
        let txt = emu.screen_text();
        assert!(txt.contains("hello"));
        assert!(txt.contains("world"));

        assert!(emu.resize(80, 24));
        assert!(!emu.resize(80, 24));

        assert_eq!(
            egui_key_to_vt_bytes(
                egui::Key::C,
                &egui::Modifiers {
                    ctrl: true,
                    ..Default::default()
                }
            ),
            Some(vec![0x03])
        );
        assert_eq!(
            egui_key_to_vt_bytes(egui::Key::ArrowUp, &egui::Modifiers::NONE),
            Some(b"\x1b[A".to_vec())
        );
    }
}
