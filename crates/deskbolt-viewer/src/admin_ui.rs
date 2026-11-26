use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deskbolt_proto::{
    FileEntry, FileTransferMsg, SerialParity, ServiceOp, ServiceStateKind, SyncEntry, SysAdminMsg,
    TerminalMsg,
};
use egui::{Color32, Context, Key, RichText, ScrollArea, Vec2, ViewportBuilder, ViewportId, Window};
use tokio::sync::mpsc;

use crate::db::{CommandSnippet, HostInput, HostProtocol, HostRow};
use crate::net_tools::{
    ArpEntry, DEFAULT_SCAN_PORTS, DnsQueryKind, HostLatencyStats, ScanHit, TracerouteHop,
    dns_lookup, expand_cidr_v4, probe_host_latency, read_arp_table, run_traceroute, scan_subnet,
    send_wol_packet, try_create_icmp_client,
};
use crate::session::{
    ConnectTarget, ConnectionPhase, LogTailBuffer, QuickEditBuffer, SessionCommand, TunnelRule,
    ViewerSession, list_local_drives, read_local_dir,
};
use crate::sftp_sync::{
    DiffEntry, DiffStatus, SftpClient, SyncAction, SyncMode, compute_tree_diff, plan_sync_actions,
    scan_local_hash_tree,
};
use crate::terminal::{
    DEFAULT_TERM_COLS, DEFAULT_TERM_ROWS, DirectTerminalCmd, DirectTerminalEvent, SshConnectError,
    SshConnectParams, TerminalEmulator, egui_key_to_vt_bytes, list_local_serial_ports,
    spawn_direct_ssh_terminal, spawn_local_serial_terminal, vt_color_to_egui,
};
use crate::oob::{OobTarget, spawn_oob_sol_terminal};
use crate::ui::{DeskBoltViewerApp, format_bytes, join_remote_path, parent_remote_path};

pub struct DirectTerminalTab {
    pub id: u64,
    pub host_id: Option<i64>,
    pub title: String,
    pub is_serial: bool,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub key_path: String,
    pub passphrase: String,
    pub pinned_fp: Option<String>,
    pub pending_tofu_fp: Option<String>,
    pub mismatch_fps: Option<(String, String)>,
    pub need_auth: bool,
    pub status: String,
    pub connected: bool,
    pub emu: TerminalEmulator,
    pub cmd_tx: Option<mpsc::Sender<DirectTerminalCmd>>,
    pub ev_rx: Option<std::sync::mpsc::Receiver<DirectTerminalEvent>>,
    pub serial_port: String,
    pub baud_rate: u32,
}

enum SftpWorkerCmd {
    ListDir(String),
    Mkdir(String),
    Delete { path: String, is_dir: bool },
    Upload { local: PathBuf, remote: String },
    Download { remote: String, local: PathBuf },
    FetchText(String),
    SaveText { remote: String, content: String },
    Tail { path: String, offset: u64 },
    HashTree(String),
    Disconnect,
}

enum SftpWorkerEvent {
    Connected(String),
    TofuConfirm(String),
    TofuMismatch { expected: String, observed: String },
    DirList { path: String, entries: Vec<FileEntry>, error: Option<String> },
    ActionResult { ok: bool, msg: String },
    QuickEditLoaded { path: String, content: String, error: Option<String> },
    QuickEditSaved { path: String, ok: bool, error: Option<String> },
    TailChunk { path: String, next_offset: u64, data: Vec<u8>, truncated: bool, error: Option<String> },
    HashTree { entries: Vec<SyncEntry>, error: Option<String> },
    Disconnected(String),
}

pub struct DirectSftpWindow {
    pub id: u64,
    pub host_id: Option<i64>,
    pub title: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub key_path: String,
    pub passphrase: String,
    pub pinned_fp: Option<String>,
    pub pending_tofu_fp: Option<String>,
    pub mismatch_fps: Option<(String, String)>,
    pub need_auth: bool,
    pub connected: bool,
    pub status: String,
    pub local_roots: Vec<String>,
    pub local_path: PathBuf,
    pub local_entries: Vec<FileEntry>,
    pub local_selected: std::collections::HashSet<String>,
    pub remote_path: String,
    pub remote_entries: Vec<FileEntry>,
    pub remote_selected: std::collections::HashSet<String>,
    pub local_mkdir: String,
    pub remote_mkdir: String,
    pub quick_edit: Option<QuickEditBuffer>,
    pub log_tail: Option<LogTailBuffer>,
    pub last_tail_poll: Instant,
    pub remote_hash_tree: Option<Vec<SyncEntry>>,
    cmd_tx: Option<mpsc::Sender<SftpWorkerCmd>>,
    ev_rx: Option<std::sync::mpsc::Receiver<SftpWorkerEvent>>,
}

pub struct SyncDiffModal {
    pub title: String,
    pub deskbolt_session_id: Option<u64>,
    pub sftp_id: Option<u64>,
    pub local_dir: PathBuf,
    pub remote_dir: String,
    pub mode: SyncMode,
    pub local_tree: Option<Vec<SyncEntry>>,
    pub remote_tree: Option<Vec<SyncEntry>>,
    pub diffs: Vec<DiffEntry>,
    pub status: String,
    local_rx: Option<std::sync::mpsc::Receiver<Result<Vec<SyncEntry>, String>>>,
}

pub struct BroadcastModalState {
    pub folder_name: String,
    pub hosts: Vec<HostRow>,
    pub command: String,
    pub results: Vec<BroadcastHostRow>,
    pub running: bool,
    pub next_cmd_id: u64,
}

pub struct BroadcastHostRow {
    pub host_id: i64,
    pub name: String,
    pub address: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub output: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NetToolTab {
    Scanner,
    DnsArpTrace,
    Wol,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SysManagerTab {
    OverviewDisks,
    Processes,
    Services,
}

pub type OobResultRx = std::sync::mpsc::Receiver<(Option<i64>, Result<String, String>)>;

pub struct AdminUiState {
    pub snippets: Vec<CommandSnippet>,
    pub snippets_open: bool,
    pub snippet_name: String,
    pub snippet_cmd: String,
    pub snippet_cat: String,

    pub direct_terminals: Vec<DirectTerminalTab>,
    pub direct_sftps: Vec<DirectSftpWindow>,
    pub sync_modal: Option<SyncDiffModal>,
    pub broadcast_modal: Option<BroadcastModalState>,
    pub next_local_id: u64,

    // Live ping monitor across Address Book hosts
    pub ping_enabled: bool,
    pub latencies: HashMap<i64, HostLatencyStats>,
    last_ping_sweep: Instant,
    ping_tx: mpsc::UnboundedSender<(i64, String, u16)>,
    ping_rx: std::sync::mpsc::Receiver<(i64, Option<Duration>, bool)>,

    // Network Tools window
    pub net_tools_open: bool,
    pub net_tab: NetToolTab,
    pub scan_cidr: String,
    pub scan_ports: String,
    pub scan_running: bool,
    pub scan_results: Vec<ScanHit>,
    pub scan_error: Option<String>,
    scan_rx: Option<std::sync::mpsc::Receiver<Result<Vec<ScanHit>, String>>>,

    pub dns_input: String,
    pub dns_kind: DnsQueryKind,
    pub dns_results: Vec<String>,
    pub dns_error: Option<String>,
    dns_rx: Option<std::sync::mpsc::Receiver<Result<Vec<String>, String>>>,

    pub arp_entries: Vec<ArpEntry>,
    pub arp_error: Option<String>,

    pub trace_target: String,
    pub trace_hops: Vec<TracerouteHop>,
    pub trace_error: Option<String>,
    trace_rx: Option<std::sync::mpsc::Receiver<Result<Vec<TracerouteHop>, String>>>,

    pub wol_mac: String,
    pub wol_broadcast: String,
    pub wol_port: String,
    pub wol_status: String,

    // DeskBolt session UI inputs
    pub term_serial_port: String,
    pub term_serial_baud: String,
    pub term_show_serial: bool,
    pub tun_local_port: String,
    pub tun_target_host: String,
    pub tun_target_port: String,
    pub tun_reverse: bool,
    pub sys_tab: SysManagerTab,
    pub sys_filter: String,
    pub last_tail_poll: Instant,
    pub oob_rx: Option<OobResultRx>,
    pub oob_status: String,
}

impl AdminUiState {
    pub fn new(runtime: &tokio::runtime::Handle, snippets: Vec<CommandSnippet>) -> Self {
        let (ping_req_tx, mut ping_req_rx) = mpsc::unbounded_channel::<(i64, String, u16)>();
        let (ping_res_tx, ping_res_rx) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let icmp_client = try_create_icmp_client().map(Arc::new);
            let mut seq: u16 = 1;
            while let Some((hid, host, port)) = ping_req_rx.recv().await {
                seq = seq.wrapping_add(1).max(1);
                let c = icmp_client.clone();
                let res_tx = ping_res_tx.clone();
                let cur_seq = seq;
                tokio::spawn(async move {
                    let (rtt, tcp_fb) = probe_host_latency(c.as_deref(), &host, port, cur_seq).await;
                    let _ = res_tx.send((hid, rtt, tcp_fb));
                });
            }
        });

        Self {
            snippets,
            snippets_open: true,
            snippet_name: String::new(),
            snippet_cmd: String::new(),
            snippet_cat: "General".into(),
            direct_terminals: Vec::new(),
            direct_sftps: Vec::new(),
            sync_modal: None,
            broadcast_modal: None,
            next_local_id: 1,
            ping_enabled: true,
            latencies: HashMap::new(),
            last_ping_sweep: Instant::now() - Duration::from_secs(10),
            ping_tx: ping_req_tx,
            ping_rx: ping_res_rx,
            net_tools_open: false,
            net_tab: NetToolTab::Scanner,
            scan_cidr: "192.168.1.0/24".into(),
            scan_ports: DEFAULT_SCAN_PORTS
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(","),
            scan_running: false,
            scan_results: Vec::new(),
            scan_error: None,
            scan_rx: None,
            dns_input: String::new(),
            dns_kind: DnsQueryKind::A,
            dns_results: Vec::new(),
            dns_error: None,
            dns_rx: None,
            arp_entries: read_arp_table().unwrap_or_default(),
            arp_error: None,
            trace_target: String::new(),
            trace_hops: Vec::new(),
            trace_error: None,
            trace_rx: None,
            wol_mac: String::new(),
            wol_broadcast: "255.255.255.255".into(),
            wol_port: "9".into(),
            wol_status: String::new(),
            term_serial_port: list_local_serial_ports().into_iter().next().unwrap_or_default(),
            term_serial_baud: "115200".into(),
            term_show_serial: false,
            tun_local_port: "8080".into(),
            tun_target_host: "127.0.0.1".into(),
            tun_target_port: "80".into(),
            tun_reverse: false,
            sys_tab: SysManagerTab::OverviewDisks,
            sys_filter: String::new(),
            last_tail_poll: Instant::now(),
            oob_rx: None,
            oob_status: String::new(),
        }
    }
}

impl DeskBoltViewerApp {
    pub fn reload_snippets(&mut self) {
        self.admin.snippets = self.db.list_snippets().unwrap_or_default();
    }

    pub fn launch_host_protocol(&mut self, ctx: &Context, host: &HostRow) {
        match host.protocol_type {
            HostProtocol::DeskBolt => {
                self.launch_connection(
                    ctx,
                    ConnectTarget {
                        host_id: Some(host.id),
                        title: host.name.clone(),
                        host: host.host.clone(),
                        port: host.port,
                        mode: host.default_mode,
                        quality: host.quality_mode,
                        pinned_fingerprint: host.pinned_fingerprint.clone(),
                    },
                );
            }
            HostProtocol::Ssh => self.open_ssh_terminal_for_host(host),
            HostProtocol::Sftp => self.open_sftp_for_host(host),
            HostProtocol::Serial => self.open_local_serial_tab(
                &host.name,
                &host.host,
                host.port as u32,
            ),
        }
    }

    pub fn open_headless_deskbolt_tool(&mut self, ctx: &Context, host: &HostRow, tool: &str) {
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|s| s.host == host.host && s.port == host.port && s.mode == deskbolt_proto::SessionMode::FullControl)
        {
            match tool {
                "terminal" => existing.terminal_open = true,
                "tunnels" => existing.tunnels_open = true,
                "sys" => {
                    existing.sys_open = true;
                    existing.send_cmd(SessionCommand::SysAdmin(SysAdminMsg::SystemSnapshotReq));
                }
                "chat" => {
                    existing.chat_open = true;
                    existing.chat_unread = 0;
                }
                "voice" => existing.voice_open = true,
                "popup" => existing.popup_open = true,
                "power" => existing.power_open = true,
                _ => {}
            }
            return;
        }

        let id = self.next_session_id;
        self.next_session_id += 1;
        let ctx_clone = ctx.clone();
        let repaint = Arc::new(move || ctx_clone.request_repaint());
        let mut sess = ViewerSession::start(
            id,
            ConnectTarget {
                host_id: Some(host.id),
                title: host.name.clone(),
                host: host.host.clone(),
                port: host.port,
                mode: deskbolt_proto::SessionMode::FullControl,
                quality: host.quality_mode,
                pinned_fingerprint: host.pinned_fingerprint.clone(),
            },
            &self.runtime,
            repaint,
        );
        sess.headless_admin = true;
        match tool {
            "terminal" => sess.terminal_open = true,
            "tunnels" => sess.tunnels_open = true,
            "sys" => sess.sys_open = true,
            "chat" => sess.chat_open = true,
            "voice" => sess.voice_open = true,
            "popup" => sess.popup_open = true,
            "power" => sess.power_open = true,
            _ => {}
        }
        self.sessions.push(sess);
        self.active_tab_id = Some(id);
    }

    pub fn open_bios_sol_terminal(&mut self, ctx: &Context, host: &HostRow) {
        let oob_host = host
            .oob_host
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&host.host)
            .to_string();
        let id = self.admin.next_local_id;
        self.admin.next_local_id += 1;
        let c = ctx.clone();
        let target = OobTarget {
            host: oob_host.clone(),
            port: host.oob_port,
            tls_port: host.oob_tls,
            use_tls: host.oob_tls > 0,
            username: host.oob_username.clone().unwrap_or_else(|| "admin".into()),
            password: host.oob_password.clone().unwrap_or_default(),
        };
        let (tx, rx) = spawn_oob_sol_terminal(target, &self.runtime, move || c.request_repaint());
        self.admin.direct_terminals.push(DirectTerminalTab {
            id,
            host_id: Some(host.id),
            title: format!("BIOS Console - {}", host.name),
            is_serial: true,
            host: oob_host,
            port: host.oob_port,
            username: String::new(),
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
            pinned_fp: None,
            pending_tofu_fp: None,
            mismatch_fps: None,
            need_auth: false,
            status: "Connecting Serial-over-LAN...".into(),
            connected: false,
            emu: TerminalEmulator::new(DEFAULT_TERM_COLS, DEFAULT_TERM_ROWS),
            cmd_tx: Some(tx),
            ev_rx: Some(rx),
            serial_port: String::new(),
            baud_rate: 115200,
        });
    }

    pub fn open_ssh_terminal_for_host(&mut self, host: &HostRow) {
        let id = self.admin.next_local_id;
        self.admin.next_local_id += 1;
        self.admin.direct_terminals.push(DirectTerminalTab {
            id,
            host_id: Some(host.id),
            title: format!("SSH - {}", host.name),
            is_serial: false,
            host: host.host.clone(),
            port: if host.port == 4899 { 22 } else { host.port },
            username: "root".into(),
            password: String::new(),
            key_path: host.ssh_key_path.clone().unwrap_or_default(),
            passphrase: String::new(),
            pinned_fp: host.pinned_fingerprint.clone(),
            pending_tofu_fp: None,
            mismatch_fps: None,
            need_auth: true,
            status: "Enter SSH credentials".into(),
            connected: false,
            emu: TerminalEmulator::new(DEFAULT_TERM_COLS, DEFAULT_TERM_ROWS),
            cmd_tx: None,
            ev_rx: None,
            serial_port: String::new(),
            baud_rate: 115200,
        });
    }

    pub fn open_local_serial_tab(&mut self, title: &str, port: &str, baud_rate: u32) {
        let id = self.admin.next_local_id;
        self.admin.next_local_id += 1;
        let port_name = if port.trim().is_empty() {
            self.admin.term_serial_port.clone()
        } else {
            port.trim().to_string()
        };
        self.admin.direct_terminals.push(DirectTerminalTab {
            id,
            host_id: None,
            title: format!("Serial - {title}"),
            is_serial: true,
            host: String::new(),
            port: 0,
            username: String::new(),
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
            pinned_fp: None,
            pending_tofu_fp: None,
            mismatch_fps: None,
            need_auth: port_name.is_empty(),
            status: "Ready".into(),
            connected: false,
            emu: TerminalEmulator::new(DEFAULT_TERM_COLS, DEFAULT_TERM_ROWS),
            cmd_tx: None,
            ev_rx: None,
            serial_port: port_name,
            baud_rate: baud_rate.max(300),
        });
    }

    pub fn open_sftp_for_host(&mut self, host: &HostRow) {
        let id = self.admin.next_local_id;
        self.admin.next_local_id += 1;
        let local_roots = list_local_drives();
        let initial_local = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from("."));
        let local_entries = read_local_dir(&initial_local).unwrap_or_default();
        self.admin.direct_sftps.push(DirectSftpWindow {
            id,
            host_id: Some(host.id),
            title: format!("SFTP - {}", host.name),
            host: host.host.clone(),
            port: if host.port == 4899 { 22 } else { host.port },
            username: "root".into(),
            password: String::new(),
            key_path: host.ssh_key_path.clone().unwrap_or_default(),
            passphrase: String::new(),
            pinned_fp: host.pinned_fingerprint.clone(),
            pending_tofu_fp: None,
            mismatch_fps: None,
            need_auth: true,
            connected: false,
            status: "Enter SSH/SFTP credentials".into(),
            local_roots,
            local_path: initial_local,
            local_entries,
            local_selected: Default::default(),
            remote_path: "/".into(),
            remote_entries: Vec::new(),
            remote_selected: Default::default(),
            local_mkdir: String::new(),
            remote_mkdir: String::new(),
            quick_edit: None,
            log_tail: None,
            last_tail_poll: Instant::now(),
            remote_hash_tree: None,
            cmd_tx: None,
            ev_rx: None,
        });
    }

    pub fn open_broadcast_for_folder(&mut self, folder_id: i64, folder_name: &str) {
        let all_hosts = self.db.list_hosts(Some(folder_id), "").unwrap_or_default();
        let rows = all_hosts
            .iter()
            .filter(|h| h.protocol_type == HostProtocol::DeskBolt)
            .map(|h| BroadcastHostRow {
                host_id: h.id,
                name: h.name.clone(),
                address: format!("{}:{}", h.host, h.port),
                status: "Ready".into(),
                exit_code: None,
                output: String::new(),
            })
            .collect();
        self.admin.broadcast_modal = Some(BroadcastModalState {
            folder_name: folder_name.to_string(),
            hosts: all_hosts,
            command: String::new(),
            results: rows,
            running: false,
            next_cmd_id: 1,
        });
    }

    pub fn poll_admin_background(&mut self, ctx: &Context) {
        // 1. Drain ping responses and schedule periodic host latency sweep
        while let Ok((hid, rtt, tcp_fb)) = self.admin.ping_rx.try_recv() {
            self.admin
                .latencies
                .entry(hid)
                .or_default()
                .record(rtt, tcp_fb);
        }
        if self.admin.ping_enabled
            && !self.hosts.is_empty()
            && self.admin.last_ping_sweep.elapsed() >= Duration::from_secs(4)
        {
            self.admin.last_ping_sweep = Instant::now();
            for h in &self.hosts {
                if h.protocol_type != HostProtocol::Serial {
                    let _ = self.admin.ping_tx.send((h.id, h.host.clone(), h.port));
                }
            }
        }

        // 2. Poll network tools async channels
        if let Some(rx) = &self.admin.scan_rx
            && let Ok(res) = rx.try_recv()
        {
            self.admin.scan_running = false;
            match res {
                Ok(hits) => {
                    self.admin.scan_results = hits;
                    self.admin.scan_error = None;
                }
                Err(e) => self.admin.scan_error = Some(e),
            }
            self.admin.scan_rx = None;
        }
        if let Some(rx) = &self.admin.dns_rx
            && let Ok(res) = rx.try_recv()
        {
            match res {
                Ok(rows) => {
                    self.admin.dns_results = rows;
                    self.admin.dns_error = None;
                }
                Err(e) => self.admin.dns_error = Some(e),
            }
            self.admin.dns_rx = None;
        }
        if let Some(rx) = &self.admin.trace_rx
            && let Ok(res) = rx.try_recv()
        {
            match res {
                Ok(hops) => {
                    self.admin.trace_hops = hops;
                    self.admin.trace_error = None;
                }
                Err(e) => self.admin.trace_error = Some(e),
            }
            self.admin.trace_rx = None;
        }

        if let Some(rx) = &self.admin.oob_rx
            && let Ok((_hid, res)) = rx.try_recv()
        {
            match res {
                Ok(msg) => {
                    self.admin.oob_status = msg.clone();
                    self.status_line = msg;
                }
                Err(e) => {
                    self.admin.oob_status = format!("Error: {e}");
                    self.status_line = format!("Out-of-Band error: {e}");
                }
            }
            self.admin.oob_rx = None;
        }

        // 3. Poll direct SSH/Serial terminals & SFTP tabs
        let mut save_pins = Vec::new();
        for tab in &mut self.admin.direct_terminals {
            if let Some(rx) = &tab.ev_rx {
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        DirectTerminalEvent::Connecting => tab.status = "Connecting...".into(),
                        DirectTerminalEvent::TofuConfirm(fp) => {
                            tab.pending_tofu_fp = Some(fp);
                            tab.status = "Verify SSH host key".into();
                        }
                        DirectTerminalEvent::TofuMismatch { expected, observed } => {
                            tab.mismatch_fps = Some((expected, observed));
                            tab.status = "Host key mismatch".into();
                        }
                        DirectTerminalEvent::NeedCredentials(e) => {
                            tab.need_auth = true;
                            tab.status = e.unwrap_or_else(|| "Credentials required".into());
                        }
                        DirectTerminalEvent::Ready(info) => {
                            tab.connected = true;
                            tab.need_auth = false;
                            tab.status = format!("Connected ({info})");
                        }
                        DirectTerminalEvent::Data(bytes) => tab.emu.feed(&bytes),
                        DirectTerminalEvent::Exited(msg) => {
                            tab.connected = false;
                            tab.status = msg;
                        }
                    }
                }
            }
        }

        for sftp in &mut self.admin.direct_sftps {
            if let Some(rx) = &sftp.ev_rx {
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        SftpWorkerEvent::Connected(fp) => {
                            sftp.connected = true;
                            sftp.need_auth = false;
                            sftp.status = "Connected (SFTP)".into();
                            if let Some(hid) = sftp.host_id
                                && sftp.pinned_fp.is_none()
                                && !fp.is_empty()
                            {
                                sftp.pinned_fp = Some(fp.clone());
                                save_pins.push((hid, fp));
                            }
                            if let Some(tx) = &sftp.cmd_tx {
                                let _ = tx.try_send(SftpWorkerCmd::ListDir(sftp.remote_path.clone()));
                            }
                        }
                        SftpWorkerEvent::TofuConfirm(fp) => {
                            sftp.pending_tofu_fp = Some(fp);
                            sftp.status = "Verify SSH host key".into();
                        }
                        SftpWorkerEvent::TofuMismatch { expected, observed } => {
                            sftp.mismatch_fps = Some((expected, observed));
                            sftp.status = "Host key mismatch".into();
                        }
                        SftpWorkerEvent::DirList { path, entries, error } => {
                            sftp.remote_path = path;
                            sftp.remote_entries = entries;
                            sftp.remote_selected.clear();
                            if let Some(e) = error {
                                sftp.status = e;
                            }
                        }
                        SftpWorkerEvent::ActionResult { ok, msg } => {
                            sftp.status = msg;
                            if ok && let Some(tx) = &sftp.cmd_tx {
                                let _ = tx.try_send(SftpWorkerCmd::ListDir(sftp.remote_path.clone()));
                                sftp.local_entries = read_local_dir(&sftp.local_path).unwrap_or_default();
                            }
                        }
                        SftpWorkerEvent::QuickEditLoaded { path, content, error } => {
                            sftp.quick_edit = Some(QuickEditBuffer {
                                remote_path: path,
                                content,
                                dirty: false,
                                saving: false,
                                status: error,
                            });
                        }
                        SftpWorkerEvent::QuickEditSaved { path, ok, error } => {
                            if let Some(qe) = &mut sftp.quick_edit
                                && qe.remote_path == path
                            {
                                qe.saving = false;
                                if ok {
                                    qe.dirty = false;
                                    qe.status = Some("Saved via SFTP.".into());
                                } else {
                                    qe.status = error;
                                }
                            }
                        }
                        SftpWorkerEvent::TailChunk { path, next_offset, data, truncated, error } => {
                            let tail = sftp.log_tail.get_or_insert_with(|| LogTailBuffer {
                                remote_path: path.clone(),
                                text: String::new(),
                                next_offset: 0,
                                following: true,
                                status: None,
                            });
                            if tail.remote_path == path {
                                if truncated {
                                    tail.text.clear();
                                    tail.text.push_str("--- Log truncated ---\n");
                                }
                                if !data.is_empty() {
                                    tail.text.push_str(&String::from_utf8_lossy(&data));
                                }
                                tail.next_offset = next_offset;
                                tail.status = error;
                            }
                        }
                        SftpWorkerEvent::HashTree { entries, error } => {
                            sftp.remote_hash_tree = error.is_none().then_some(entries);
                            if let Some(e) = error {
                                sftp.status = e;
                            }
                        }
                        SftpWorkerEvent::Disconnected(msg) => {
                            sftp.connected = false;
                            sftp.status = msg;
                        }
                    }
                }
            }

            if sftp.connected
                && let Some(tail) = &sftp.log_tail
                && tail.following
                && sftp.last_tail_poll.elapsed() >= Duration::from_millis(850)
                && let Some(tx) = &sftp.cmd_tx
            {
                sftp.last_tail_poll = Instant::now();
                let _ = tx.try_send(SftpWorkerCmd::Tail {
                    path: tail.remote_path.clone(),
                    offset: tail.next_offset,
                });
            }
        }
        for (hid, fp) in save_pins {
            let _ = self.db.set_pinned_fingerprint(hid, Some(&fp));
            self.reload_phonebook();
        }

        // 4. Poll DeskBolt session auto-tail timer & broadcast responses
        if self.admin.last_tail_poll.elapsed() >= Duration::from_millis(850) {
            self.admin.last_tail_poll = Instant::now();
            for s in &self.sessions {
                if let Some(tail) = &s.log_tail
                    && tail.following
                {
                    s.send_cmd(SessionCommand::File(FileTransferMsg::TailChunkReq {
                        path: tail.remote_path.clone(),
                        offset: tail.next_offset,
                        max_bytes: 64 * 1024,
                    }));
                }
            }
        }
        if let Some(bm) = &mut self.admin.broadcast_modal {
            for s in &mut self.sessions {
                for (cid, code, out, err) in s.broadcast_responses.drain(..) {
                    if let Some(row) = bm.results.iter_mut().find(|r| r.address == format!("{}:{}", s.host, s.port)) {
                        row.exit_code = Some(code);
                        row.output = if let Some(e) = err { format!("{out}\nError: {e}") } else { out };
                        row.status = format!("Exit {code} (cmd #{cid})");
                    }
                }
            }
        }

        // 5. Poll Sync Diff Modal scans
        if let Some(sync) = &mut self.admin.sync_modal {
            if let Some(rx) = &sync.local_rx
                && let Ok(res) = rx.try_recv()
            {
                match res {
                    Ok(entries) => sync.local_tree = Some(entries),
                    Err(e) => sync.status = format!("Local scan error: {e}"),
                }
                sync.local_rx = None;
            }
            if let Some(sid) = sync.deskbolt_session_id
                && let Some(s) = self.sessions.iter().find(|s| s.id == sid)
                && let Some(rtree) = &s.file_transfer.remote_hash_tree
            {
                sync.remote_tree = Some(rtree.clone());
            } else if let Some(sfid) = sync.sftp_id
                && let Some(sf) = self.admin.direct_sftps.iter().find(|s| s.id == sfid)
                && let Some(rtree) = &sf.remote_hash_tree
            {
                sync.remote_tree = Some(rtree.clone());
            }
            if sync.diffs.is_empty()
                && let (Some(l), Some(r)) = (&sync.local_tree, &sync.remote_tree)
            {
                sync.diffs = compute_tree_diff(l, r);
                sync.status = format!("{} entries compared", sync.diffs.len());
            }
        }
        let _ = ctx;
    }

    pub fn open_compare_sync_for_session(&mut self, session_id: u64) {
        let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) else {
            return;
        };
        let local_dir = s.file_transfer.local_path.clone();
        let remote_dir = s.file_transfer.remote_path.clone();
        s.file_transfer.remote_hash_tree = None;
        s.send_cmd(SessionCommand::File(FileTransferMsg::HashTreeReq {
            path: remote_dir.clone(),
        }));
        let (tx, rx) = std::sync::mpsc::channel();
        let ldir_c = local_dir.clone();
        self.runtime.spawn(async move {
            let _ = tx.send(scan_local_hash_tree(&ldir_c).await);
        });
        self.admin.sync_modal = Some(SyncDiffModal {
            title: format!("Compare & Sync - {}", s.title),
            deskbolt_session_id: Some(session_id),
            sftp_id: None,
            local_dir,
            remote_dir,
            mode: SyncMode::TwoWayNewest,
            local_tree: None,
            remote_tree: None,
            diffs: Vec::new(),
            status: "Computing SHA-256 directory manifests...".into(),
            local_rx: Some(rx),
        });
    }

    pub fn render_admin_windows(&mut self, ctx: &Context) {
        self.render_network_tools_window(ctx);
        self.render_direct_terminal_windows(ctx);
        self.render_direct_sftp_windows(ctx);
        self.render_deskbolt_session_admin_windows(ctx);
        self.render_comms_and_power_windows(ctx);
        self.render_sync_diff_modal(ctx);
        self.render_broadcast_modal(ctx);
    }

    fn render_network_tools_window(&mut self, ctx: &Context) {
        if !self.admin.net_tools_open {
            return;
        }
        let mut open = true;
        Window::new("Network Tools & Diagnostics")
            .open(&mut open)
            .default_size([760.0, 500.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.admin.net_tab, NetToolTab::Scanner, "Port Scanner");
                    ui.selectable_value(
                        &mut self.admin.net_tab,
                        NetToolTab::DnsArpTrace,
                        "DNS / ARP / Traceroute",
                    );
                    ui.selectable_value(&mut self.admin.net_tab, NetToolTab::Wol, "Wake-on-LAN");
                });
                ui.separator();

                match self.admin.net_tab {
                    NetToolTab::Scanner => {
                        ui.horizontal(|ui| {
                            ui.label("CIDR:");
                            ui.add(egui::TextEdit::singleline(&mut self.admin.scan_cidr).desired_width(140.0));
                            ui.label("Ports:");
                            ui.add(egui::TextEdit::singleline(&mut self.admin.scan_ports).desired_width(180.0));
                            ui.add_enabled_ui(!self.admin.scan_running, |ui| {
                                if ui.button(if self.admin.scan_running { "Scanning..." } else { "Scan Subnet" }).clicked() {
                                    match expand_cidr_v4(&self.admin.scan_cidr) {
                                        Ok(hosts) => {
                                            let ports: Vec<u16> = self
                                                .admin
                                                .scan_ports
                                                .split(',')
                                                .filter_map(|p| p.trim().parse::<u16>().ok())
                                                .filter(|&p| p > 0)
                                                .collect();
                                            let (tx, rx) = std::sync::mpsc::channel();
                                            self.admin.scan_rx = Some(rx);
                                            self.admin.scan_running = true;
                                            self.admin.scan_error = None;
                                            let c = ctx.clone();
                                            self.runtime.spawn(async move {
                                                let hits = scan_subnet(hosts, ports, 64).await;
                                                let _ = tx.send(Ok(hits));
                                                c.request_repaint();
                                            });
                                        }
                                        Err(e) => self.admin.scan_error = Some(e),
                                    }
                                }
                            });
                        });
                        if let Some(err) = &self.admin.scan_error {
                            ui.colored_label(Color32::RED, err);
                        }
                        ui.separator();
                        ScrollArea::vertical().show(ui, |ui| {
                            egui::Grid::new("scanner_grid").striped(true).num_columns(5).show(ui, |ui| {
                                ui.strong("IP");
                                ui.strong("Port");
                                ui.strong("Service");
                                ui.strong("Banner");
                                ui.strong("Action");
                                ui.end_row();
                                for hit in self.admin.scan_results.clone() {
                                    ui.label(hit.ip.to_string());
                                    ui.label(hit.port.to_string());
                                    ui.label(&hit.service);
                                    ui.label(&hit.banner);
                                    if ui.small_button("Add to Address Book").clicked() {
                                        let proto = if hit.port == 22 {
                                            HostProtocol::Ssh
                                        } else {
                                            HostProtocol::DeskBolt
                                        };
                                        let _ = self.db.upsert_host(&HostInput {
                                            id: None,
                                            folder_id: self.selected_folder,
                                            name: format!("{} ({})", hit.ip, hit.service),
                                            host: hit.ip.to_string(),
                                            port: hit.port,
                                            default_mode: deskbolt_proto::SessionMode::FullControl,
                                            quality_mode: deskbolt_proto::VideoQualityMode::Auto,
                                            mac_address: None,
                                            protocol_type: proto,
                                            ssh_key_path: None,
                                            saved_tunnels_json: None,
                                            ..Default::default()
                                        });
                                        self.reload_phonebook();
                                        self.status_line = format!("Imported {}:{}", hit.ip, hit.port);
                                    }
                                    ui.end_row();
                                }
                            });
                        });
                    }
                    NetToolTab::DnsArpTrace => {
                        ui.horizontal(|ui| {
                            ui.strong("DNS Lookup:");
                            ui.add(egui::TextEdit::singleline(&mut self.admin.dns_input).hint_text("Hostname or IP").desired_width(200.0));
                            egui::ComboBox::from_id_salt("dns_kind")
                                .selected_text(self.admin.dns_kind.label())
                                .show_ui(ui, |ui| {
                                    for k in DnsQueryKind::ALL {
                                        ui.selectable_value(&mut self.admin.dns_kind, k, k.label());
                                    }
                                });
                            if ui.button("Resolve").clicked() {
                                let q = self.admin.dns_input.clone();
                                let kind = self.admin.dns_kind;
                                let (tx, rx) = std::sync::mpsc::channel();
                                self.admin.dns_rx = Some(rx);
                                let c = ctx.clone();
                                self.runtime.spawn(async move {
                                    let _ = tx.send(dns_lookup(&q, kind).await);
                                    c.request_repaint();
                                });
                            }
                        });
                        if let Some(err) = &self.admin.dns_error {
                            ui.colored_label(Color32::RED, err);
                        }
                        if !self.admin.dns_results.is_empty() {
                            ScrollArea::vertical().max_height(80.0).id_salt("dns_out").show(ui, |ui| {
                                for r in &self.admin.dns_results {
                                    ui.monospace(r);
                                }
                            });
                        }
                        ui.separator();

                        ui.columns(2, |cols| {
                            cols[0].horizontal(|ui| {
                                ui.strong("OS ARP / Neighbor Table");
                                if ui.small_button("Refresh").clicked() {
                                    match read_arp_table() {
                                        Ok(list) => {
                                            self.admin.arp_entries = list;
                                            self.admin.arp_error = None;
                                        }
                                        Err(e) => self.admin.arp_error = Some(e),
                                    }
                                }
                            });
                            if let Some(err) = &self.admin.arp_error {
                                cols[0].colored_label(Color32::RED, err);
                            }
                            ScrollArea::vertical().id_salt("arp_scroll").show(&mut cols[0], |ui| {
                                egui::Grid::new("arp_grid").striped(true).num_columns(3).show(ui, |ui| {
                                    ui.strong("IP");
                                    ui.strong("MAC");
                                    ui.strong("State");
                                    ui.end_row();
                                    for a in &self.admin.arp_entries {
                                        ui.label(&a.ip);
                                        ui.monospace(&a.mac);
                                        ui.label(&a.state);
                                        ui.end_row();
                                    }
                                });
                            });

                            cols[1].horizontal(|ui| {
                                ui.strong("Traceroute:");
                                ui.add(egui::TextEdit::singleline(&mut self.admin.trace_target).hint_text("Host or IP").desired_width(140.0));
                                if ui.small_button("Trace").clicked() {
                                    let tgt = self.admin.trace_target.clone();
                                    let (tx, rx) = std::sync::mpsc::channel();
                                    self.admin.trace_rx = Some(rx);
                                    let c = ctx.clone();
                                    self.runtime.spawn(async move {
                                        let _ = tx.send(run_traceroute(&tgt, 20).await);
                                        c.request_repaint();
                                    });
                                }
                            });
                            if let Some(err) = &self.admin.trace_error {
                                cols[1].colored_label(Color32::RED, err);
                            }
                            ScrollArea::vertical().id_salt("trace_scroll").show(&mut cols[1], |ui| {
                                egui::Grid::new("trace_grid").striped(true).num_columns(3).show(ui, |ui| {
                                    ui.strong("Hop");
                                    ui.strong("Address");
                                    ui.strong("RTT");
                                    ui.end_row();
                                    for h in &self.admin.trace_hops {
                                        ui.label(h.hop.to_string());
                                        ui.monospace(&h.address);
                                        ui.label(
                                            h.rtt_ms
                                                .map(|ms| format!("{ms:.1} ms"))
                                                .unwrap_or_else(|| "*".into()),
                                        );
                                        ui.end_row();
                                    }
                                });
                            });
                        });
                    }
                    NetToolTab::Wol => {
                        egui::Grid::new("wol_grid").num_columns(2).show(ui, |ui| {
                            ui.label("MAC Address:");
                            ui.text_edit_singleline(&mut self.admin.wol_mac);
                            ui.end_row();
                            ui.label("Broadcast IP:");
                            ui.text_edit_singleline(&mut self.admin.wol_broadcast);
                            ui.end_row();
                            ui.label("UDP Port:");
                            ui.text_edit_singleline(&mut self.admin.wol_port);
                            ui.end_row();
                        });
                        if ui.button("Send Magic Packet").clicked() {
                            let port = self.admin.wol_port.trim().parse::<u16>().unwrap_or(9);
                            match send_wol_packet(&self.admin.wol_mac, &self.admin.wol_broadcast, port) {
                                Ok(()) => self.admin.wol_status = "Wake-on-LAN packet sent.".into(),
                                Err(e) => self.admin.wol_status = e,
                            }
                        }
                        if !self.admin.wol_status.is_empty() {
                            ui.label(&self.admin.wol_status);
                        }
                    }
                }
            });
        if !open {
            self.admin.net_tools_open = false;
        }
    }

    fn render_direct_terminal_windows(&mut self, ctx: &Context) {
        let mut to_remove = Vec::new();
        for idx in 0..self.admin.direct_terminals.len() {
            let tid = self.admin.direct_terminals[idx].id;
            let title = self.admin.direct_terminals[idx].title.clone();
            let mut open = true;
            Window::new(format!("{title}##term_{tid}"))
                .open(&mut open)
                .default_size([840.0, 520.0])
                .show(ctx, |ui| {
                    let tab = &mut self.admin.direct_terminals[idx];
                    if tab.need_auth {
                        if tab.is_serial {
                            ui.horizontal(|ui| {
                                ui.label("Port:");
                                ui.text_edit_singleline(&mut tab.serial_port);
                                ui.label("Baud:");
                                let mut b_str = tab.baud_rate.to_string();
                                ui.add(egui::TextEdit::singleline(&mut b_str).desired_width(70.0));
                                if let Ok(b) = b_str.trim().parse::<u32>() {
                                    tab.baud_rate = b;
                                }
                                if ui.button("Open Serial").clicked() {
                                    let c = ctx.clone();
                                    let (tx, rx) = spawn_local_serial_terminal(
                                        tab.serial_port.clone(),
                                        tab.baud_rate,
                                        8,
                                        SerialParity::None,
                                        1,
                                        Arc::new(move || c.request_repaint()),
                                    );
                                    tab.cmd_tx = Some(tx);
                                    tab.ev_rx = Some(rx);
                                    tab.need_auth = false;
                                }
                            });
                        } else {
                            egui::Grid::new(("ssh_auth", tid)).num_columns(2).show(ui, |ui| {
                                ui.label("Username:");
                                ui.text_edit_singleline(&mut tab.username);
                                ui.end_row();
                                ui.label("Password:");
                                ui.add(egui::TextEdit::singleline(&mut tab.password).password(true));
                                ui.end_row();
                                ui.label("Private Key Path:");
                                ui.text_edit_singleline(&mut tab.key_path);
                                ui.end_row();
                                ui.label("Key Passphrase:");
                                ui.add(egui::TextEdit::singleline(&mut tab.passphrase).password(true));
                                ui.end_row();
                            });
                            if ui.button("Connect SSH2").clicked() {
                                start_ssh_tab_connection(tab, &self.runtime, ctx, false);
                            }
                        }
                        ui.label(&tab.status);
                        return;
                    }

                    if let Some(fp) = tab.pending_tofu_fp.clone() {
                        ui.strong("Verify SSH Host Key Fingerprint");
                        ui.monospace(&fp);
                        if ui.button("Trust & Save Host Key").clicked() {
                            if let Some(hid) = tab.host_id {
                                let _ = self.db.set_pinned_fingerprint(hid, Some(&fp));
                            }
                            tab.pinned_fp = Some(fp);
                            tab.pending_tofu_fp = None;
                            start_ssh_tab_connection(tab, &self.runtime, ctx, true);
                        }
                        return;
                    }
                    if let Some((exp, obs)) = tab.mismatch_fps.clone() {
                        ui.colored_label(Color32::RED, "Warning: SSH Host Key Changed");
                        ui.monospace(format!("Expected: {exp}\nObserved: {obs}"));
                        if ui.button("Replace Key & Reconnect").clicked() {
                            if let Some(hid) = tab.host_id {
                                let _ = self.db.set_pinned_fingerprint(hid, Some(&obs));
                            }
                            tab.pinned_fp = Some(obs);
                            tab.mismatch_fps = None;
                            start_ssh_tab_connection(tab, &self.runtime, ctx, true);
                        }
                        return;
                    }

                    let cmd_tx_opt = tab.cmd_tx.clone();
                    let status = tab.status.clone();
                    let mut emu_ptr = std::mem::replace(&mut tab.emu, TerminalEmulator::new(1, 1));
                    let mut send_bytes = Vec::new();
                    let mut resize_to = None;
                    let saved = render_terminal_surface(
                        ui,
                        tid,
                        &status,
                        &mut emu_ptr,
                        &mut self.admin.snippets_open,
                        &self.admin.snippets,
                        &mut self.admin.snippet_name,
                        &mut self.admin.snippet_cmd,
                        &mut self.admin.snippet_cat,
                        &self.db,
                        &mut send_bytes,
                        &mut resize_to,
                    );
                    self.admin.direct_terminals[idx].emu = emu_ptr;
                    if saved {
                        self.reload_snippets();
                    }
                    if !send_bytes.is_empty()
                        && let Some(tx) = &cmd_tx_opt
                    {
                        let _ = tx.try_send(DirectTerminalCmd::Write(send_bytes));
                    }
                    if let Some((c, r)) = resize_to
                        && let Some(tx) = &cmd_tx_opt
                    {
                        let _ = tx.try_send(DirectTerminalCmd::Resize { cols: c, rows: r });
                    }
                });
            if !open {
                if let Some(tx) = &self.admin.direct_terminals[idx].cmd_tx {
                    let _ = tx.try_send(DirectTerminalCmd::Close);
                }
                to_remove.push(tid);
            }
        }
        if !to_remove.is_empty() {
            self.admin.direct_terminals.retain(|t| !to_remove.contains(&t.id));
        }
    }

    fn render_direct_sftp_windows(&mut self, ctx: &Context) {
        let mut to_remove = Vec::new();
        for idx in 0..self.admin.direct_sftps.len() {
            let sid = self.admin.direct_sftps[idx].id;
            let title = self.admin.direct_sftps[idx].title.clone();
            let mut open = true;
            Window::new(format!("{title}##sftp_{sid}"))
                .open(&mut open)
                .default_size([960.0, 580.0])
                .show(ctx, |ui| {
                    let sf = &mut self.admin.direct_sftps[idx];
                    if sf.need_auth {
                        egui::Grid::new(("sftp_auth", sid)).num_columns(2).show(ui, |ui| {
                            ui.label("Username:");
                            ui.text_edit_singleline(&mut sf.username);
                            ui.end_row();
                            ui.label("Password:");
                            ui.add(egui::TextEdit::singleline(&mut sf.password).password(true));
                            ui.end_row();
                            ui.label("Private Key Path:");
                            ui.text_edit_singleline(&mut sf.key_path);
                            ui.end_row();
                        });
                        if ui.button("Connect SFTP").clicked() {
                            start_sftp_worker(sf, &self.runtime, ctx, false);
                        }
                        ui.label(&sf.status);
                        return;
                    }
                    if let Some(fp) = sf.pending_tofu_fp.clone() {
                        ui.strong("Verify SSH Host Key Fingerprint");
                        ui.monospace(&fp);
                        if ui.button("Trust & Connect").clicked() {
                            if let Some(hid) = sf.host_id {
                                let _ = self.db.set_pinned_fingerprint(hid, Some(&fp));
                            }
                            sf.pinned_fp = Some(fp);
                            sf.pending_tofu_fp = None;
                            start_sftp_worker(sf, &self.runtime, ctx, true);
                        }
                        return;
                    }
                    if let Some((exp, obs)) = sf.mismatch_fps.clone() {
                        ui.colored_label(Color32::RED, "Warning: SSH Host Key Changed");
                        ui.monospace(format!("Expected: {exp}\nObserved: {obs}"));
                        if ui.button("Replace Key & Reconnect").clicked() {
                            if let Some(hid) = sf.host_id {
                                let _ = self.db.set_pinned_fingerprint(hid, Some(&obs));
                            }
                            sf.pinned_fp = Some(obs);
                            sf.mismatch_fps = None;
                            start_sftp_worker(sf, &self.runtime, ctx, true);
                        }
                        return;
                    }

                    ui.horizontal(|ui| {
                        ui.label(&sf.status);
                        if ui.button("Compare & Sync...").clicked() {
                            sf.remote_hash_tree = None;
                            if let Some(tx) = &sf.cmd_tx {
                                let _ = tx.try_send(SftpWorkerCmd::HashTree(sf.remote_path.clone()));
                            }
                            let (tx, rx) = std::sync::mpsc::channel();
                            let ldir = sf.local_path.clone();
                            self.runtime.spawn(async move {
                                let _ = tx.send(scan_local_hash_tree(&ldir).await);
                            });
                            self.admin.sync_modal = Some(SyncDiffModal {
                                title: format!("Compare & Sync - {}", sf.title),
                                deskbolt_session_id: None,
                                sftp_id: Some(sf.id),
                                local_dir: sf.local_path.clone(),
                                remote_dir: sf.remote_path.clone(),
                                mode: SyncMode::TwoWayNewest,
                                local_tree: None,
                                remote_tree: None,
                                diffs: Vec::new(),
                                status: "Computing SHA-256 manifests...".into(),
                                local_rx: Some(rx),
                            });
                        }
                    });
                    ui.separator();

                    let sf = &mut self.admin.direct_sftps[idx];
                    ui.columns(2, |cols| {
                        cols[0].group(|ui| {
                            ui.strong("Local Computer");
                            ui.horizontal(|ui| {
                                let mut lp = sf.local_path.to_string_lossy().into_owned();
                                if ui.text_edit_singleline(&mut lp).lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                    sf.local_path = PathBuf::from(lp);
                                    sf.local_entries = read_local_dir(&sf.local_path).unwrap_or_default();
                                }
                                if ui.button("Up").clicked()
                                    && let Some(p) = sf.local_path.parent()
                                {
                                    sf.local_path = p.to_path_buf();
                                    sf.local_entries = read_local_dir(&sf.local_path).unwrap_or_default();
                                }
                                if ui.button("Upload ->").clicked() {
                                    let sel: Vec<String> = sf.local_selected.iter().cloned().collect();
                                    for name in sel {
                                        if let Some(e) = sf.local_entries.iter().find(|x| x.name == name)
                                            && !e.is_dir
                                            && let Some(tx) = &sf.cmd_tx
                                        {
                                            let _ = tx.try_send(SftpWorkerCmd::Upload {
                                                local: sf.local_path.join(&name),
                                                remote: join_remote_path(&sf.remote_path, &name),
                                            });
                                        }
                                    }
                                }
                            });
                            ScrollArea::vertical().id_salt(("sftp_loc", sid)).show(ui, |ui| {
                                for e in sf.local_entries.clone() {
                                    let is_sel = sf.local_selected.contains(&e.name);
                                    let lbl = if e.is_dir { format!("[DIR] {}", e.name) } else { format!("{} ({})", e.name, format_bytes(e.size)) };
                                    let r = ui.selectable_label(is_sel, lbl);
                                    if r.clicked() {
                                        if !ui.input(|i| i.modifiers.ctrl) {
                                            sf.local_selected.clear();
                                        }
                                        if is_sel { sf.local_selected.remove(&e.name); } else { sf.local_selected.insert(e.name.clone()); }
                                    }
                                    if r.double_clicked() && e.is_dir {
                                        sf.local_path = sf.local_path.join(&e.name);
                                        sf.local_entries = read_local_dir(&sf.local_path).unwrap_or_default();
                                    }
                                }
                            });
                        });

                        cols[1].group(|ui| {
                            ui.strong("Remote SFTP Host");
                            ui.horizontal(|ui| {
                                if ui.text_edit_singleline(&mut sf.remote_path).lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))
                                    && let Some(tx) = &sf.cmd_tx
                                {
                                    let _ = tx.try_send(SftpWorkerCmd::ListDir(sf.remote_path.clone()));
                                }
                                if ui.button("Up").clicked()
                                    && let Some(tx) = &sf.cmd_tx
                                {
                                    let p = parent_remote_path(&sf.remote_path);
                                    let _ = tx.try_send(SftpWorkerCmd::ListDir(p));
                                }
                                if ui.button("<- Download").clicked() {
                                    let sel: Vec<String> = sf.remote_selected.iter().cloned().collect();
                                    for name in sel {
                                        if let Some(e) = sf.remote_entries.iter().find(|x| x.name == name)
                                            && !e.is_dir
                                            && let Some(tx) = &sf.cmd_tx
                                        {
                                            let _ = tx.try_send(SftpWorkerCmd::Download {
                                                remote: join_remote_path(&sf.remote_path, &name),
                                                local: sf.local_path.join(&name),
                                            });
                                        }
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut sf.remote_mkdir).hint_text("New folder").desired_width(110.0));
                                if ui.button("Create").clicked() && !sf.remote_mkdir.trim().is_empty()
                                    && let Some(tx) = &sf.cmd_tx
                                {
                                    let rp = join_remote_path(&sf.remote_path, sf.remote_mkdir.trim());
                                    sf.remote_mkdir.clear();
                                    let _ = tx.try_send(SftpWorkerCmd::Mkdir(rp));
                                }
                            });
                            ScrollArea::vertical().id_salt(("sftp_rem", sid)).show(ui, |ui| {
                                for e in sf.remote_entries.clone() {
                                    let is_sel = sf.remote_selected.contains(&e.name);
                                    let lbl = if e.is_dir { format!("[DIR] {}", e.name) } else { format!("{} ({})", e.name, format_bytes(e.size)) };
                                    let r = ui.selectable_label(is_sel, lbl);
                                    if r.clicked() {
                                        if !ui.input(|i| i.modifiers.ctrl) {
                                            sf.remote_selected.clear();
                                        }
                                        if is_sel { sf.remote_selected.remove(&e.name); } else { sf.remote_selected.insert(e.name.clone()); }
                                    }
                                    r.context_menu(|ui| {
                                        if !e.is_dir && ui.button("Quick Edit").clicked()
                                            && let Some(tx) = &sf.cmd_tx
                                        {
                                            let _ = tx.try_send(SftpWorkerCmd::FetchText(join_remote_path(&sf.remote_path, &e.name)));
                                            ui.close();
                                        }
                                        if !e.is_dir && ui.button("Tail Live Log").clicked()
                                            && let Some(tx) = &sf.cmd_tx
                                        {
                                            let rp = join_remote_path(&sf.remote_path, &e.name);
                                            sf.log_tail = Some(LogTailBuffer {
                                                remote_path: rp.clone(),
                                                text: String::new(),
                                                next_offset: 0,
                                                following: true,
                                                status: Some("Tailing...".into()),
                                            });
                                            let _ = tx.try_send(SftpWorkerCmd::Tail { path: rp, offset: 0 });
                                            ui.close();
                                        }
                                        if ui.button("Delete").clicked()
                                            && let Some(tx) = &sf.cmd_tx
                                        {
                                            let _ = tx.try_send(SftpWorkerCmd::Delete {
                                                path: join_remote_path(&sf.remote_path, &e.name),
                                                is_dir: e.is_dir,
                                            });
                                            ui.close();
                                        }
                                    });
                                    if r.double_clicked() {
                                        let next = join_remote_path(&sf.remote_path, &e.name);
                                        if e.is_dir {
                                            if let Some(tx) = &sf.cmd_tx {
                                                let _ = tx.try_send(SftpWorkerCmd::ListDir(next));
                                            }
                                        } else if crate::sftp_sync::is_quick_edit_ext(&e.name)
                                            && let Some(tx) = &sf.cmd_tx
                                        {
                                            let _ = tx.try_send(SftpWorkerCmd::FetchText(next));
                                        }
                                    }
                                }
                            });
                        });
                    });
                });

            // Quick Edit & Live Tail modals for SFTP
            if let Some(mut qe) = self.admin.direct_sftps[idx].quick_edit.take() {
                let mut qe_open = true;
                let tx_opt = self.admin.direct_sftps[idx].cmd_tx.clone();
                Window::new(format!("Quick Edit - {}##sftp_qe_{sid}", qe.remote_path))
                    .open(&mut qe_open)
                    .default_size([700.0, 480.0])
                    .show(ctx, |ui| {
                        let save_hotkey = ui.input(|i| i.modifiers.ctrl && i.key_pressed(Key::S));
                        ui.horizontal(|ui| {
                            if (ui.button("Save (Ctrl+S)").clicked() || save_hotkey) && !qe.saving
                                && let Some(tx) = &tx_opt
                            {
                                qe.saving = true;
                                qe.status = Some("Saving...".into());
                                let _ = tx.try_send(SftpWorkerCmd::SaveText {
                                    remote: qe.remote_path.clone(),
                                    content: qe.content.clone(),
                                });
                            }
                            if let Some(st) = &qe.status {
                                ui.label(st);
                            }
                        });
                        ScrollArea::vertical().show(ui, |ui| {
                            if ui.add_sized(ui.available_size(), egui::TextEdit::multiline(&mut qe.content).code_editor()).changed() {
                                qe.dirty = true;
                            }
                        });
                    });
                if qe_open {
                    self.admin.direct_sftps[idx].quick_edit = Some(qe);
                }
            }

            if let Some(mut tail) = self.admin.direct_sftps[idx].log_tail.take() {
                let mut tl_open = true;
                Window::new(format!("Live Log Tail - {}##sftp_tl_{sid}", tail.remote_path))
                    .open(&mut tl_open)
                    .default_size([740.0, 440.0])
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut tail.following, "Follow");
                            if ui.button("Clear").clicked() {
                                tail.text.clear();
                            }
                            if let Some(st) = &tail.status {
                                ui.label(st);
                            }
                        });
                        ScrollArea::vertical().stick_to_bottom(tail.following).show(ui, |ui| {
                            ui.add(egui::TextEdit::multiline(&mut tail.text).code_editor().desired_width(f32::INFINITY));
                        });
                    });
                if tl_open {
                    self.admin.direct_sftps[idx].log_tail = Some(tail);
                }
            }

            if !open {
                if let Some(tx) = &self.admin.direct_sftps[idx].cmd_tx {
                    let _ = tx.try_send(SftpWorkerCmd::Disconnect);
                }
                to_remove.push(sid);
            }
        }
        if !to_remove.is_empty() {
            self.admin.direct_sftps.retain(|s| !to_remove.contains(&s.id));
        }
    }

    fn render_deskbolt_session_admin_windows(&mut self, ctx: &Context) {
        let mut close_headless = Vec::new();
        for idx in 0..self.sessions.len() {
            let sid = self.sessions[idx].id;
            let title = self.sessions[idx].title.clone();
            let connected = matches!(self.sessions[idx].phase, ConnectionPhase::Connected);

            // 1. Remote PTY & Serial Window
            if self.sessions[idx].terminal_open {
                let mut open = true;
                let render_term = |ui: &mut egui::Ui, app: &mut Self| {
                    let s = &mut app.sessions[idx];
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Start / Restart Shell").clicked() && connected {
                            s.send_cmd(SessionCommand::Terminal(TerminalMsg::OpenPty {
                                cols: s.terminal_emu.cols,
                                rows: s.terminal_emu.rows,
                                shell_hint: None,
                            }));
                        }
                        if ui.button("Serial Port...").clicked() {
                            app.admin.term_show_serial = !app.admin.term_show_serial;
                            if connected {
                                s.send_cmd(SessionCommand::Terminal(TerminalMsg::ListSerialPortsReq));
                            }
                        }
                        ui.label(&s.terminal_status);
                    });

                    if app.admin.term_show_serial {
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt(("rem_ser", sid))
                                .selected_text(if app.admin.term_serial_port.is_empty() {
                                    "Select port..."
                                } else {
                                    &app.admin.term_serial_port
                                })
                                .show_ui(ui, |ui| {
                                    for p in &s.remote_serial_ports {
                                        ui.selectable_value(&mut app.admin.term_serial_port, p.clone(), p);
                                    }
                                });
                            ui.add(egui::TextEdit::singleline(&mut app.admin.term_serial_port).desired_width(90.0));
                            ui.label("Baud:");
                            ui.add(egui::TextEdit::singleline(&mut app.admin.term_serial_baud).desired_width(70.0));
                            if ui.button("Connect Serial").clicked() && connected {
                                let baud = app.admin.term_serial_baud.trim().parse::<u32>().unwrap_or(115200);
                                s.send_cmd(SessionCommand::Terminal(TerminalMsg::SerialOpen {
                                    port: app.admin.term_serial_port.clone(),
                                    baud_rate: baud,
                                    data_bits: 8,
                                    parity: SerialParity::None,
                                    stop_bits: 1,
                                }));
                            }
                        });
                    }
                    ui.separator();

                    let status = s.terminal_status.clone();
                    let mut emu = std::mem::replace(&mut s.terminal_emu, TerminalEmulator::new(1, 1));
                    let mut bytes_out = Vec::new();
                    let mut resize_to = None;
                    let saved = render_terminal_surface(
                        ui,
                        sid,
                        &status,
                        &mut emu,
                        &mut app.admin.snippets_open,
                        &app.admin.snippets,
                        &mut app.admin.snippet_name,
                        &mut app.admin.snippet_cmd,
                        &mut app.admin.snippet_cat,
                        &app.db,
                        &mut bytes_out,
                        &mut resize_to,
                    );
                    app.sessions[idx].terminal_emu = emu;
                    if saved {
                        app.reload_snippets();
                    }
                    if !bytes_out.is_empty() && connected {
                        let msg = if app.admin.term_show_serial {
                            TerminalMsg::SerialData(bytes_out)
                        } else {
                            TerminalMsg::PtyData(bytes_out)
                        };
                        app.sessions[idx].send_cmd(SessionCommand::Terminal(msg));
                    }
                    if let Some((c, r)) = resize_to
                        && connected
                    {
                        app.sessions[idx].send_cmd(SessionCommand::Terminal(TerminalMsg::PtyResize {
                            cols: c,
                            rows: r,
                        }));
                    }
                };

                #[cfg(not(target_os = "android"))]
                {
                    let vp_id = ViewportId::from_hash_of(("deskbolt_term_vp", sid));
                    let builder = ViewportBuilder::default()
                        .with_title(format!("Terminal - {title}"))
                        .with_inner_size([920.0, 560.0]);
                    ctx.show_viewport_immediate(vp_id, builder, |ui, _| {
                        if ui.input(|i| i.viewport().close_requested()) {
                            open = false;
                            return;
                        }
                        egui::CentralPanel::default().show(ui, |ui| render_term(ui, self));
                    });
                }
                #[cfg(target_os = "android")]
                Window::new(format!("Terminal - {title}##term_win_{sid}"))
                    .open(&mut open)
                    .show(ctx, |ui| render_term(ui, self));

                if !open {
                    self.sessions[idx].terminal_open = false;
                    self.sessions[idx].send_cmd(SessionCommand::Terminal(TerminalMsg::ClosePty));
                    if close_headless_if_unused(&self.sessions[idx]) {
                        close_headless.push(sid);
                    }
                }
            }

            // 2. Encrypted Port Forwarding Tunnels Window
            if self.sessions[idx].tunnels_open {
                let mut open = true;
                Window::new(format!("Port Forwarding Tunnels - {title}##tun_{sid}"))
                    .open(&mut open)
                    .default_size([620.0, 360.0])
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.admin.tun_reverse, "Reverse (Remote -> Local)");
                            ui.label(if self.admin.tun_reverse { "Remote Bind Port:" } else { "Local Port:" });
                            ui.add(egui::TextEdit::singleline(&mut self.admin.tun_local_port).desired_width(60.0));
                            ui.label("-> Target:");
                            ui.add(egui::TextEdit::singleline(&mut self.admin.tun_target_host).desired_width(130.0));
                            ui.label(":");
                            ui.add(egui::TextEdit::singleline(&mut self.admin.tun_target_port).desired_width(60.0));
                            if ui.button("Start Tunnel").clicked() && connected {
                                let lp = self.admin.tun_local_port.trim().parse::<u16>().unwrap_or(0);
                                let tp = self.admin.tun_target_port.trim().parse::<u16>().unwrap_or(0);
                                let th = self.admin.tun_target_host.trim().to_string();
                                if lp > 0 && tp > 0 && !th.is_empty() {
                                    let s = &mut self.sessions[idx];
                                    let rid = s.next_tunnel_rule_id;
                                    s.next_tunnel_rule_id += 1;
                                    let reverse = self.admin.tun_reverse;
                                    s.tunnels.push(TunnelRule {
                                        id: rid,
                                        local_port: lp,
                                        target_host: th.clone(),
                                        target_port: tp,
                                        reverse,
                                        active: false,
                                        status: "Starting...".into(),
                                    });
                                    if reverse {
                                        s.send_cmd(SessionCommand::StartReverseForward {
                                            listener_id: rid,
                                            bind_port: lp,
                                            target_host: th,
                                            target_port: tp,
                                        });
                                    } else {
                                        s.send_cmd(SessionCommand::StartLocalForward {
                                            rule_id: rid,
                                            local_port: lp,
                                            target_host: th,
                                            target_port: tp,
                                        });
                                    }
                                }
                            }
                        });
                        ui.separator();
                        let s = &mut self.sessions[idx];
                        let mut stop_rule = None;
                        egui::Grid::new(("tun_grid", sid)).striped(true).num_columns(5).show(ui, |ui| {
                            ui.strong("Direction");
                            ui.strong("Bind");
                            ui.strong("Target");
                            ui.strong("Status");
                            ui.strong("Action");
                            ui.end_row();
                            for r in &s.tunnels {
                                ui.label(if r.reverse { "Reverse" } else { "Local" });
                                ui.label(format!("127.0.0.1:{}", r.local_port));
                                ui.label(format!("{}:{}", r.target_host, r.target_port));
                                ui.label(&r.status);
                                if ui.small_button("Stop").clicked() {
                                    stop_rule = Some((r.id, r.reverse));
                                }
                                ui.end_row();
                            }
                        });
                        if let Some((rid, rev)) = stop_rule {
                            if rev {
                                s.send_cmd(SessionCommand::StopReverseForward { listener_id: rid });
                            } else {
                                s.send_cmd(SessionCommand::StopLocalForward { rule_id: rid });
                            }
                            s.tunnels.retain(|r| r.id != rid);
                        }
                    });
                if !open {
                    self.sessions[idx].tunnels_open = false;
                    if close_headless_if_unused(&self.sessions[idx]) {
                        close_headless.push(sid);
                    }
                }
            }

            // 3. Headless System, Process & Service Manager Window
            if self.sessions[idx].sys_open {
                let mut open = true;
                Window::new(format!("System & Service Manager - {title}##sys_{sid}"))
                    .open(&mut open)
                    .default_size([760.0, 500.0])
                    .show(ctx, |ui| {
                        let s = &mut self.sessions[idx];
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut self.admin.sys_tab, SysManagerTab::OverviewDisks, "Overview & Disks");
                            ui.selectable_value(&mut self.admin.sys_tab, SysManagerTab::Processes, "Processes");
                            ui.selectable_value(&mut self.admin.sys_tab, SysManagerTab::Services, "Services");
                            ui.separator();
                            if ui.button("Refresh").clicked() && connected {
                                s.send_cmd(SessionCommand::SysAdmin(SysAdminMsg::SystemSnapshotReq));
                            }
                            ui.add(egui::TextEdit::singleline(&mut self.admin.sys_filter).hint_text("Filter...").desired_width(140.0));
                            ui.label(&s.sys_status);
                        });
                        ui.separator();

                        let Some(snap) = s.sys_snapshot.clone() else {
                            ui.label("Waiting for host system snapshot...");
                            return;
                        };
                        let filt = self.admin.sys_filter.trim().to_ascii_lowercase();
                        match self.admin.sys_tab {
                            SysManagerTab::OverviewDisks => {
                                ui.label(format!(
                                    "Host: {}  |  OS: {}  |  Uptime: {}s",
                                    snap.hostname, snap.os_version, snap.uptime_secs
                                ));
                                ui.horizontal(|ui| {
                                    ui.label(format!("CPU ({} cores): {:.1}%", snap.cpu_cores, snap.cpu_usage_pct));
                                    ui.add(egui::ProgressBar::new((snap.cpu_usage_pct / 100.0).clamp(0.0, 1.0)).desired_width(160.0));
                                    let mem_frac = if snap.total_mem_bytes > 0 {
                                        (snap.used_mem_bytes as f64 / snap.total_mem_bytes as f64) as f32
                                    } else {
                                        0.0
                                    };
                                    ui.label(format!(
                                        "Memory: {} / {}",
                                        format_bytes(snap.used_mem_bytes),
                                        format_bytes(snap.total_mem_bytes)
                                    ));
                                    ui.add(egui::ProgressBar::new(mem_frac.clamp(0.0, 1.0)).desired_width(160.0));
                                });
                                ui.separator();
                                ui.strong("Storage Volumes");
                                ScrollArea::vertical().id_salt(("sys_d", sid)).show(ui, |ui| {
                                    egui::Grid::new(("disks_g", sid)).striped(true).num_columns(5).show(ui, |ui| {
                                        ui.strong("Mount");
                                        ui.strong("Label");
                                        ui.strong("FS");
                                        ui.strong("Free");
                                        ui.strong("Total");
                                        ui.end_row();
                                        for d in &snap.disks {
                                            ui.label(&d.mount_point);
                                            ui.label(&d.name);
                                            ui.label(&d.fs_type);
                                            ui.label(format_bytes(d.available_bytes));
                                            ui.label(format_bytes(d.total_bytes));
                                            ui.end_row();
                                        }
                                    });
                                });
                            }
                            SysManagerTab::Processes => {
                                ScrollArea::vertical().id_salt(("sys_p", sid)).show(ui, |ui| {
                                    egui::Grid::new(("proc_g", sid)).striped(true).num_columns(5).show(ui, |ui| {
                                        ui.strong("PID");
                                        ui.strong("Name");
                                        ui.strong("CPU %");
                                        ui.strong("Memory");
                                        ui.strong("Action");
                                        ui.end_row();
                                        for p in snap.processes.iter().filter(|p| {
                                            filt.is_empty()
                                                || p.name.to_ascii_lowercase().contains(&filt)
                                                || p.pid.to_string().contains(&filt)
                                        }) {
                                            ui.label(p.pid.to_string());
                                            ui.label(&p.name);
                                            ui.label(format!("{:.1}%", p.cpu_pct));
                                            ui.label(format_bytes(p.memory_bytes));
                                            if ui.small_button("Terminate").clicked() {
                                                s.send_cmd(SessionCommand::SysAdmin(SysAdminMsg::KillProcess(p.pid)));
                                            }
                                            ui.end_row();
                                        }
                                    });
                                });
                            }
                            SysManagerTab::Services => {
                                ScrollArea::vertical().id_salt(("sys_s", sid)).show(ui, |ui| {
                                    egui::Grid::new(("svc_g", sid)).striped(true).num_columns(4).show(ui, |ui| {
                                        ui.strong("Service");
                                        ui.strong("Display Name");
                                        ui.strong("State");
                                        ui.strong("Controls");
                                        ui.end_row();
                                        for svc in snap.services.iter().filter(|sv| {
                                            filt.is_empty()
                                                || sv.name.to_ascii_lowercase().contains(&filt)
                                                || sv.display_name.to_ascii_lowercase().contains(&filt)
                                        }) {
                                            ui.label(&svc.name);
                                            ui.label(&svc.display_name);
                                            ui.label(match svc.state {
                                                ServiceStateKind::Running => "Running",
                                                ServiceStateKind::Stopped => "Stopped",
                                                ServiceStateKind::Paused => "Paused",
                                                ServiceStateKind::Pending => "Pending",
                                                ServiceStateKind::Unknown => "Unknown",
                                            });
                                            ui.horizontal(|ui| {
                                                if ui.small_button("Start").clicked() {
                                                    s.send_cmd(SessionCommand::SysAdmin(SysAdminMsg::ServiceAction {
                                                        name: svc.name.clone(),
                                                        action: ServiceOp::Start,
                                                    }));
                                                }
                                                if ui.small_button("Stop").clicked() {
                                                    s.send_cmd(SessionCommand::SysAdmin(SysAdminMsg::ServiceAction {
                                                        name: svc.name.clone(),
                                                        action: ServiceOp::Stop,
                                                    }));
                                                }
                                                if ui.small_button("Restart").clicked() {
                                                    s.send_cmd(SessionCommand::SysAdmin(SysAdminMsg::ServiceAction {
                                                        name: svc.name.clone(),
                                                        action: ServiceOp::Restart,
                                                    }));
                                                }
                                            });
                                            ui.end_row();
                                        }
                                    });
                                });
                            }
                        }
                    });
                if !open {
                    self.sessions[idx].sys_open = false;
                    if close_headless_if_unused(&self.sessions[idx]) {
                        close_headless.push(sid);
                    }
                }
            }

            // 4. DeskBolt Quick-Edit Config Window
            if let Some(mut qe) = self.sessions[idx].quick_edit.take() {
                let mut open = true;
                Window::new(format!("Quick Edit - {}##db_qe_{sid}", qe.remote_path))
                    .open(&mut open)
                    .default_size([700.0, 480.0])
                    .show(ctx, |ui| {
                        let hotkey = ui.input(|i| i.modifiers.ctrl && i.key_pressed(Key::S));
                        ui.horizontal(|ui| {
                            if (ui.button("Save (Ctrl+S)").clicked() || hotkey) && !qe.saving {
                                qe.saving = true;
                                qe.status = Some("Uploading...".into());
                                self.sessions[idx].send_cmd(SessionCommand::QuickEditSave {
                                    remote_path: qe.remote_path.clone(),
                                    content: qe.content.clone(),
                                });
                            }
                            if let Some(st) = &qe.status {
                                ui.label(st);
                            }
                        });
                        ScrollArea::vertical().show(ui, |ui| {
                            if ui.add_sized(ui.available_size(), egui::TextEdit::multiline(&mut qe.content).code_editor()).changed() {
                                qe.dirty = true;
                            }
                        });
                    });
                if open {
                    self.sessions[idx].quick_edit = Some(qe);
                }
            }

            // 5. DeskBolt Live Log Tail Window
            if let Some(mut tail) = self.sessions[idx].log_tail.take() {
                let mut open = true;
                Window::new(format!("Live Log Tail - {}##db_tl_{sid}", tail.remote_path))
                    .open(&mut open)
                    .default_size([740.0, 440.0])
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut tail.following, "Follow");
                            if ui.button("Clear").clicked() {
                                tail.text.clear();
                            }
                            if let Some(st) = &tail.status {
                                ui.label(st);
                            }
                        });
                        ScrollArea::vertical().stick_to_bottom(tail.following).show(ui, |ui| {
                            ui.add(egui::TextEdit::multiline(&mut tail.text).code_editor().desired_width(f32::INFINITY));
                        });
                    });
                if open {
                    self.sessions[idx].log_tail = Some(tail);
                }
            }
        }
        if !close_headless.is_empty() {
            for s in &self.sessions {
                if close_headless.contains(&s.id) {
                    s.send_cmd(SessionCommand::Disconnect);
                }
            }
            self.sessions.retain(|s| !close_headless.contains(&s.id));
        }
    }

    pub(crate) fn close_headless_session_ids(&mut self, ids: &[u64]) {
        if ids.is_empty() {
            return;
        }
        for s in &self.sessions {
            if ids.contains(&s.id) {
                s.send_cmd(SessionCommand::Disconnect);
            }
        }
        self.sessions.retain(|s| !ids.contains(&s.id));
    }

    fn render_sync_diff_modal(&mut self, ctx: &Context) {
        let Some(mut modal) = self.admin.sync_modal.take() else {
            return;
        };
        let mut keep_open = true;
        Window::new(modal.title.clone())
            .default_size([760.0, 480.0])
            .show(ctx, |ui| {
                ui.label(format!(
                    "Local: {}  <=>  Remote: {}",
                    modal.local_dir.display(),
                    modal.remote_dir
                ));
                ui.horizontal(|ui| {
                    ui.label("Sync Mode:");
                    egui::ComboBox::from_id_salt("sync_mode_box")
                        .selected_text(modal.mode.label())
                        .show_ui(ui, |ui| {
                            for m in SyncMode::ALL {
                                ui.selectable_value(&mut modal.mode, m, m.label());
                            }
                        });
                    ui.label(&modal.status);
                });
                ui.separator();

                ScrollArea::vertical().max_height(310.0).show(ui, |ui| {
                    egui::Grid::new("sync_diff_grid").striped(true).num_columns(4).show(ui, |ui| {
                        ui.strong("Relative Path");
                        ui.strong("Status");
                        ui.strong("Local");
                        ui.strong("Remote");
                        ui.end_row();
                        for d in &modal.diffs {
                            if d.status == DiffStatus::Identical {
                                continue;
                            }
                            ui.label(&d.rel_path);
                            ui.label(d.status.label());
                            ui.label(d.local_size.map(format_bytes).unwrap_or_default());
                            ui.label(d.remote_size.map(format_bytes).unwrap_or_default());
                            ui.end_row();
                        }
                    });
                });

                ui.separator();
                ui.horizontal(|ui| {
                    let ready = !modal.diffs.is_empty();
                    if ui.add_enabled(ready, egui::Button::new("Run Sync")).clicked() {
                        let actions = plan_sync_actions(&modal.diffs, modal.mode);
                        if let Some(sid) = modal.deskbolt_session_id
                            && let Some(s) = self.sessions.iter().find(|s| s.id == sid)
                        {
                            for act in actions {
                                match act {
                                    SyncAction::EnsureLocalDir { rel_path } => {
                                        let _ = std::fs::create_dir_all(modal.local_dir.join(rel_path));
                                    }
                                    SyncAction::EnsureRemoteDir { rel_path } => {
                                        s.send_cmd(SessionCommand::File(FileTransferMsg::MkdirReq {
                                            path: join_remote_path(&modal.remote_dir, &rel_path),
                                        }));
                                    }
                                    SyncAction::Upload { rel_path, .. } => {
                                        s.send_cmd(SessionCommand::QueueUpload {
                                            local_path: modal.local_dir.join(&rel_path),
                                            remote_path: join_remote_path(&modal.remote_dir, &rel_path),
                                        });
                                    }
                                    SyncAction::Download { rel_path, size } => {
                                        s.send_cmd(SessionCommand::QueueDownload {
                                            remote_path: join_remote_path(&modal.remote_dir, &rel_path),
                                            local_path: modal.local_dir.join(&rel_path),
                                            total_bytes: size,
                                        });
                                    }
                                }
                            }
                        } else if let Some(sfid) = modal.sftp_id
                            && let Some(sf) = self.admin.direct_sftps.iter().find(|s| s.id == sfid)
                            && let Some(tx) = &sf.cmd_tx
                        {
                            for act in actions {
                                match act {
                                    SyncAction::EnsureLocalDir { rel_path } => {
                                        let _ = std::fs::create_dir_all(modal.local_dir.join(rel_path));
                                    }
                                    SyncAction::EnsureRemoteDir { rel_path } => {
                                        let _ = tx.try_send(SftpWorkerCmd::Mkdir(join_remote_path(&modal.remote_dir, &rel_path)));
                                    }
                                    SyncAction::Upload { rel_path, .. } => {
                                        let _ = tx.try_send(SftpWorkerCmd::Upload {
                                            local: modal.local_dir.join(&rel_path),
                                            remote: join_remote_path(&modal.remote_dir, &rel_path),
                                        });
                                    }
                                    SyncAction::Download { rel_path, .. } => {
                                        let _ = tx.try_send(SftpWorkerCmd::Download {
                                            remote: join_remote_path(&modal.remote_dir, &rel_path),
                                            local: modal.local_dir.join(&rel_path),
                                        });
                                    }
                                }
                            }
                        }
                        keep_open = false;
                    }
                    if ui.button("Close").clicked() {
                        keep_open = false;
                    }
                });
            });
        if keep_open {
            self.admin.sync_modal = Some(modal);
        }
    }

    fn render_broadcast_modal(&mut self, ctx: &Context) {
        let Some(mut bm) = self.admin.broadcast_modal.take() else {
            return;
        };
        let mut keep_open = true;
        Window::new(format!("Folder Broadcast Execution - {}", bm.folder_name))
            .default_size([760.0, 480.0])
            .show(ctx, |ui| {
                ui.label("Runs non-interactive commands across all connected Full Control hosts in this folder.");
                ui.horizontal(|ui| {
                    ui.label("Command:");
                    ui.add(egui::TextEdit::singleline(&mut bm.command).desired_width(480.0));
                    if ui.button("Execute All").clicked() && !bm.command.trim().is_empty() {
                        let cmd_id = bm.next_cmd_id;
                        bm.next_cmd_id += 1;
                        for row in &mut bm.results {
                            if let Some(s) = self.sessions.iter().find(|s| {
                                format!("{}:{}", s.host, s.port) == row.address
                                    && matches!(s.phase, ConnectionPhase::Connected)
                                    && s.mode == deskbolt_proto::SessionMode::FullControl
                            }) {
                                row.status = "Running...".into();
                                row.exit_code = None;
                                row.output.clear();
                                s.send_cmd(SessionCommand::SysAdmin(SysAdminMsg::BroadcastExecReq {
                                    command_id: cmd_id,
                                    command: bm.command.clone(),
                                }));
                            } else {
                                row.status = "Not connected in Full Control".into();
                            }
                        }
                    }
                });
                ui.separator();
                ScrollArea::vertical().max_height(330.0).show(ui, |ui| {
                    for row in &bm.results {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.strong(&row.name);
                                ui.label(&row.address);
                                ui.label(&row.status);
                            });
                            if !row.output.is_empty() {
                                ui.monospace(&row.output);
                            }
                        });
                    }
                });
                if ui.button("Close").clicked() {
                    keep_open = false;
                }
            });
        if keep_open {
            self.admin.broadcast_modal = Some(bm);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_terminal_surface(
    ui: &mut egui::Ui,
    term_id: u64,
    status: &str,
    emu: &mut TerminalEmulator,
    snippets_open: &mut bool,
    snippets: &[CommandSnippet],
    snippet_name: &mut String,
    snippet_cmd: &mut String,
    snippet_cat: &mut String,
    db: &crate::db::PhonebookDb,
    out_bytes: &mut Vec<u8>,
    out_resize: &mut Option<(u16, u16)>,
) -> bool {
    let mut snippets_changed = false;
    ui.horizontal(|ui| {
        if ui.button("Copy Screen").clicked() {
            ui.ctx().copy_text(emu.screen_text());
        }
        if ui.button("Paste").clicked()
            && let Some(txt) = crate::session::read_local_clipboard_utf8()
        {
            out_bytes.extend_from_slice(txt.as_bytes());
        }
        let logging = emu.log_path.is_some();
        if ui
            .selectable_label(logging, if logging { "Stop Log" } else { "Log Session" })
            .clicked()
        {
            let log_p = std::env::temp_dir().join(format!("deskbolt_term_{term_id}.log"));
            let _ = emu.toggle_session_log(&log_p);
        }
        let mut sb = emu.scroll_offset as i32;
        ui.label("Scrollback:");
        if ui.add(egui::DragValue::new(&mut sb).range(0..=2000)).changed() {
            emu.set_scrollback(sb.max(0) as usize);
        }
        ui.checkbox(snippets_open, "Snippets");
        ui.label(status);
    });

    if *snippets_open {
        egui::Panel::right(("snippets_drawer", term_id))
            .default_size(220.0)
            .show(ui, |ui| {
                ui.strong("Command Snippets");
                ui.separator();
                ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for sn in snippets {
                        ui.horizontal(|ui| {
                            if ui.button(&sn.name).on_hover_text(&sn.command).clicked() {
                                out_bytes.extend_from_slice(sn.command.as_bytes());
                                out_bytes.push(b'\r');
                            }
                            if ui.small_button("x").clicked() && db.delete_snippet(sn.id).is_ok() {
                                snippets_changed = true;
                            }
                        });
                    }
                });
                ui.separator();
                ui.add(egui::TextEdit::singleline(snippet_name).hint_text("Name"));
                ui.add(egui::TextEdit::singleline(snippet_cmd).hint_text("Command"));
                ui.add(egui::TextEdit::singleline(snippet_cat).hint_text("Category"));
                if ui.button("Save Snippet").clicked()
                    && db.upsert_snippet(None, snippet_name, snippet_cmd, snippet_cat).is_ok()
                {
                    snippet_name.clear();
                    snippet_cmd.clear();
                    snippets_changed = true;
                }
            });
    }

    let avail = ui.available_size();
    let char_w = 7.4_f32;
    let line_h = 15.0_f32;
    let new_cols = ((avail.x / char_w).floor() as u16).clamp(24, 240);
    let new_rows = ((avail.y / line_h).floor() as u16).clamp(8, 120);
    if emu.resize(new_cols, new_rows) {
        *out_resize = Some((new_cols, new_rows));
    }

    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(avail.x.max(200.0), avail.y.max(120.0)),
        egui::Sense::click(),
    );
    if resp.clicked() {
        resp.request_focus();
    }
    ui.painter().rect_filled(rect, 2.0, Color32::from_rgb(16, 19, 24));

    let screen = emu.parser.screen();
    let font_id = egui::FontId::monospace(12.5);
    let (cur_r, cur_c) = screen.cursor_position();
    for r in 0..emu.rows {
        let y = rect.min.y + (r as f32) * line_h + 2.0;
        if y + line_h > rect.max.y {
            break;
        }
        let mut row_txt = String::with_capacity(emu.cols as usize);
        let mut prev_fg = Color32::from_rgb(228, 232, 238);
        let mut run_start_col = 0u16;
        for c in 0..emu.cols {
            let (ch_str, fg, bg) = if let Some(cell) = screen.cell(r, c) {
                let s = cell.contents();
                let ch = if s.is_empty() { " ".to_string() } else { s };
                (
                    ch,
                    vt_color_to_egui(cell.fgcolor(), false),
                    vt_color_to_egui(cell.bgcolor(), true),
                )
            } else {
                (
                    " ".to_string(),
                    Color32::from_rgb(228, 232, 238),
                    Color32::from_rgb(16, 19, 24),
                )
            };
            if bg != Color32::from_rgb(16, 19, 24) {
                let x = rect.min.x + 4.0 + (c as f32) * char_w;
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(egui::Pos2::new(x, y), Vec2::new(char_w, line_h)),
                    0.0,
                    bg,
                );
            }
            if c > 0 && fg != prev_fg {
                let x = rect.min.x + 4.0 + (run_start_col as f32) * char_w;
                ui.painter().text(
                    egui::Pos2::new(x, y),
                    egui::Align2::LEFT_TOP,
                    &row_txt,
                    font_id.clone(),
                    prev_fg,
                );
                row_txt.clear();
                run_start_col = c;
            }
            prev_fg = fg;
            row_txt.push_str(&ch_str);
        }
        if !row_txt.is_empty() {
            let x = rect.min.x + 4.0 + (run_start_col as f32) * char_w;
            ui.painter().text(
                egui::Pos2::new(x, y),
                egui::Align2::LEFT_TOP,
                &row_txt,
                font_id.clone(),
                prev_fg,
            );
        }
    }

    if cur_r < emu.rows && cur_c < emu.cols {
        let cx = rect.min.x + 4.0 + (cur_c as f32) * char_w;
        let cy = rect.min.y + (cur_r as f32) * line_h + 2.0;
        ui.painter().rect_filled(
            egui::Rect::from_min_size(egui::Pos2::new(cx, cy), Vec2::new(char_w, line_h)),
            0.0,
            Color32::from_rgba_unmultiplied(180, 210, 255, 110),
        );
    }

    if resp.has_focus() || ui.ui_contains_pointer() {
        let events = ui.input(|i| i.events.clone());
        for ev in events {
            match ev {
                egui::Event::Text(txt) => out_bytes.extend_from_slice(txt.as_bytes()),
                egui::Event::Paste(txt) => out_bytes.extend_from_slice(txt.as_bytes()),
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if let Some(seq) = egui_key_to_vt_bytes(key, &modifiers) {
                        out_bytes.extend_from_slice(&seq);
                    }
                }
                _ => {}
            }
        }
    }

    let _ = RichText::new("");
    snippets_changed
}

fn start_ssh_tab_connection(
    tab: &mut DirectTerminalTab,
    runtime: &tokio::runtime::Handle,
    ctx: &Context,
    trust_unpinned: bool,
) {
    let c = ctx.clone();
    let params = SshConnectParams {
        host: tab.host.clone(),
        port: tab.port,
        username: tab.username.clone(),
        password: tab.password.clone(),
        key_path: (!tab.key_path.trim().is_empty()).then(|| PathBuf::from(tab.key_path.trim())),
        key_passphrase: (!tab.passphrase.is_empty()).then(|| tab.passphrase.clone()),
        expected_fingerprint: tab.pinned_fp.clone(),
        trust_unpinned,
    };
    let (tx, rx) = spawn_direct_ssh_terminal(
        runtime,
        params,
        tab.emu.cols,
        tab.emu.rows,
        Arc::new(move || c.request_repaint()),
    );
    tab.cmd_tx = Some(tx);
    tab.ev_rx = Some(rx);
    tab.need_auth = false;
}

fn start_sftp_worker(
    sf: &mut DirectSftpWindow,
    runtime: &tokio::runtime::Handle,
    ctx: &Context,
    trust_unpinned: bool,
) {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<SftpWorkerCmd>(32);
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    let params = SshConnectParams {
        host: sf.host.clone(),
        port: sf.port,
        username: sf.username.clone(),
        password: sf.password.clone(),
        key_path: (!sf.key_path.trim().is_empty()).then(|| PathBuf::from(sf.key_path.trim())),
        key_passphrase: (!sf.passphrase.is_empty()).then(|| sf.passphrase.clone()),
        expected_fingerprint: sf.pinned_fp.clone(),
        trust_unpinned,
    };
    let c = ctx.clone();
    let send_ev = move |ev: SftpWorkerEvent| {
        let _ = ev_tx.send(ev);
        c.request_repaint();
    };

    runtime.spawn(async move {
        let (client, fp) = match SftpClient::connect(&params).await {
            Ok(pair) => pair,
            Err(SshConnectError::TofuConfirmationRequired { fingerprint }) => {
                send_ev(SftpWorkerEvent::TofuConfirm(fingerprint));
                return;
            }
            Err(SshConnectError::TofuMismatch { expected, observed }) => {
                send_ev(SftpWorkerEvent::TofuMismatch { expected, observed });
                return;
            }
            Err(SshConnectError::Failed(msg)) => {
                send_ev(SftpWorkerEvent::Disconnected(msg));
                return;
            }
        };
        send_ev(SftpWorkerEvent::Connected(fp));

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                SftpWorkerCmd::ListDir(path) => match client.list_dir(&path).await {
                    Ok(entries) => send_ev(SftpWorkerEvent::DirList {
                        path,
                        entries,
                        error: None,
                    }),
                    Err(e) => send_ev(SftpWorkerEvent::DirList {
                        path,
                        entries: Vec::new(),
                        error: Some(e),
                    }),
                },
                SftpWorkerCmd::Mkdir(p) => match client.mkdir(&p).await {
                    Ok(()) => send_ev(SftpWorkerEvent::ActionResult {
                        ok: true,
                        msg: format!("Created {p}"),
                    }),
                    Err(e) => send_ev(SftpWorkerEvent::ActionResult { ok: false, msg: e }),
                },
                SftpWorkerCmd::Delete { path, is_dir } => match client.delete(&path, is_dir).await {
                    Ok(()) => send_ev(SftpWorkerEvent::ActionResult {
                        ok: true,
                        msg: format!("Deleted {path}"),
                    }),
                    Err(e) => send_ev(SftpWorkerEvent::ActionResult { ok: false, msg: e }),
                },
                SftpWorkerCmd::Upload { local, remote } => match client.upload_file(&local, &remote).await {
                    Ok(n) => send_ev(SftpWorkerEvent::ActionResult {
                        ok: true,
                        msg: format!("Uploaded {} ({})", remote, format_bytes(n)),
                    }),
                    Err(e) => send_ev(SftpWorkerEvent::ActionResult { ok: false, msg: e }),
                },
                SftpWorkerCmd::Download { remote, local } => match client.download_file(&remote, &local).await {
                    Ok(n) => send_ev(SftpWorkerEvent::ActionResult {
                        ok: true,
                        msg: format!("Downloaded {} ({})", local.display(), format_bytes(n)),
                    }),
                    Err(e) => send_ev(SftpWorkerEvent::ActionResult { ok: false, msg: e }),
                },
                SftpWorkerCmd::FetchText(remote) => match client.read_text_limited(&remote, 512 * 1024).await {
                    Ok(bytes) => send_ev(SftpWorkerEvent::QuickEditLoaded {
                        path: remote,
                        content: String::from_utf8_lossy(&bytes).into_owned(),
                        error: None,
                    }),
                    Err(e) => send_ev(SftpWorkerEvent::QuickEditLoaded {
                        path: remote,
                        content: String::new(),
                        error: Some(e),
                    }),
                },
                SftpWorkerCmd::SaveText { remote, content } => {
                    match client.write_all_bytes(&remote, content.as_bytes()).await {
                        Ok(()) => send_ev(SftpWorkerEvent::QuickEditSaved {
                            path: remote,
                            ok: true,
                            error: None,
                        }),
                        Err(e) => send_ev(SftpWorkerEvent::QuickEditSaved {
                            path: remote,
                            ok: false,
                            error: Some(e),
                        }),
                    }
                }
                SftpWorkerCmd::Tail { path, offset } => match client.tail_chunk(&path, offset, 64 * 1024).await {
                    Ok((next_offset, data, truncated)) => send_ev(SftpWorkerEvent::TailChunk {
                        path,
                        next_offset,
                        data,
                        truncated,
                        error: None,
                    }),
                    Err(e) => send_ev(SftpWorkerEvent::TailChunk {
                        path,
                        next_offset: offset,
                        data: Vec::new(),
                        truncated: false,
                        error: Some(e),
                    }),
                },
                SftpWorkerCmd::HashTree(path) => match client.hash_tree(&path).await {
                    Ok(entries) => send_ev(SftpWorkerEvent::HashTree {
                        entries,
                        error: None,
                    }),
                    Err(e) => send_ev(SftpWorkerEvent::HashTree {
                        entries: Vec::new(),
                        error: Some(e),
                    }),
                },
                SftpWorkerCmd::Disconnect => break,
            }
        }
        send_ev(SftpWorkerEvent::Disconnected("SFTP disconnected.".into()));
    });

    sf.cmd_tx = Some(cmd_tx);
    sf.ev_rx = Some(ev_rx);
    sf.need_auth = false;
}

pub(crate) fn close_headless_if_unused(s: &ViewerSession) -> bool {
    s.headless_admin
        && !s.terminal_open
        && !s.tunnels_open
        && !s.sys_open
        && !s.chat_open
        && !s.voice_open
        && !s.popup_open
        && !s.power_open
}
