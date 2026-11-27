use deskbolt_proto::video_pipeline::VideoCapabilities;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use deskbolt_proto::{
    ARGON2_M_KIB, ARGON2_P_COST, ARGON2_T_COST, AsyncFrameReader, ChannelId, ClipboardSync, CommsMsg, ControlMsg,
    FLAG_KEYFRAME, FLAG_NONE, FLAG_URGENT, FileTransferMsg, Frame, InputMsg, IpRateLimiter,
    IpcMsg, NONCE_LEN, PROTO_VERSION, PowerMsg, SALT_LEN, ServerTlsStream, SessionMode,
    SpecialAction, StoredCredential, SysAdminMsg, TerminalMsg, TlsServerConfig, TunnelMsg,
    VideoMsg, VoiceControls, VoiceEngine, accept_tls, build_server_config, compute_server_proof,
    pack_message, random_bytes, read_frame_async, sha256_fingerprint, unpack_message,
    verify_client_proof, write_frame_async,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tracing::{info, warn};

use crate::collaboration::{Roster, ViewerLease};
use crate::config::{ServerConfig, ServerPolicy, evaluate_ip_access, log_audit_event};
use crate::files::FileSessionHandler;
use crate::ipc::{AgentHub, ViewerActivity};
use crate::platform::{execute_power_action, prompt_connection_confirmation, show_desktop_popup, trigger_send_sas};
use crate::sysadmin::{SysAdminHandler, execute_broadcast_command};
use crate::terminal::TerminalSessionHandler;
use crate::tunnel::TunnelSessionHandler;

const DEFAULT_PREAUTH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PREAUTH_CONNS: usize = 32;
const MAX_VIDEO_QUEUE: usize = 2;

/// Runs the TLS 1.3 TCP listener (`port`, default `4899`) with auth, rate limiting, and session mux.
pub async fn run_tcp_server(
    cfg: ServerConfig,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    hub: AgentHub,
    sas_enabled: bool,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let cred = Arc::new(cfg.load_credential()?);
    let policy = ServerPolicy::from_config(&cfg);
    let bind_target = format!("{}:{}", cfg.bind_addr, cfg.port);
    let listener = TcpListener::bind(&bind_target)
        .await
        .map_err(|e| format!("failed to bind {bind_target}: {e}"))?;

    info!("DeskBolt server listening on {bind_target} (TLS 1.3)");
    serve_listener_with_policy(
        listener,
        cert_der,
        key_der,
        cred,
        hub,
        sas_enabled,
        policy,
        DEFAULT_PREAUTH_TIMEOUT,
        shutdown_rx,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
async fn serve_listener(
    listener: TcpListener,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    cred: Arc<StoredCredential>,
    hub: AgentHub,
    sas_enabled: bool,
    single_controller_lock: bool,
    preauth_timeout: Duration,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    serve_listener_with_policy(
        listener,
        cert_der,
        key_der,
        cred,
        hub,
        sas_enabled,
        ServerPolicy::for_test(single_controller_lock),
        preauth_timeout,
        shutdown_rx,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn serve_listener_with_policy(
    listener: TcpListener,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    cred: Arc<StoredCredential>,
    hub: AgentHub,
    sas_enabled: bool,
    policy: ServerPolicy,
    preauth_timeout: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    #[cfg(all(feature = "dev-mcp", not(target_os = "android")))]
    if !policy.os_prompts_enabled()
        && listener.local_addr().map_err(|e| e.to_string())?.ip()
            != std::net::Ipv4Addr::LOCALHOST
    {
        return Err("The fixture listener requires 127.0.0.1.".into());
    }
    let cert_fp = sha256_fingerprint(&cert_der);
    let tls_cfg = build_server_config(cert_der, key_der).map_err(|e| e.to_string())?;
    let limiter = Arc::new(Mutex::new(match policy.lockout_threshold {
        Some(t) => IpRateLimiter::with_limits(t, policy.ban_duration_secs),
        None => IpRateLimiter::new(),
    }));
    let preauth_sem = Arc::new(Semaphore::new(MAX_PREAUTH_CONNS));
    let (roster, _) = watch::channel(Roster::new(policy.single_controller_lock));
    let policy = Arc::new(policy);
    let (chat_tx, _) = broadcast::channel::<(u64, CommsMsg)>(64);
    let next_conn_id = Arc::new(AtomicU64::new(1));

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let _ = stream.set_nodelay(true);
                        let Ok(permit) = preauth_sem.clone().try_acquire_owned() else {
                            warn!(%peer, "Dropping connection: pre-auth limit reached");
                            drop(stream);
                            continue;
                        };
                        let conn_id = next_conn_id.fetch_add(1, Ordering::Relaxed);
                        let tls_cfg = tls_cfg.clone();
                        let cred = cred.clone();
                        let limiter = limiter.clone();
                        let hub = hub.clone();
                        let roster = roster.clone();
                        let policy_c = policy.clone();
                        let chat_tx_c = chat_tx.clone();
                        let sess_shutdown = shutdown_rx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(
                                conn_id,
                                stream,
                                peer,
                                tls_cfg,
                                cert_fp,
                                cred,
                                limiter,
                                hub,
                                sas_enabled,
                                roster,
                                policy_c,
                                chat_tx_c,
                                preauth_timeout,
                                permit,
                                sess_shutdown,
                            )
                            .await
                            {
                                warn!(%peer, "Session ended: {e}");
                            }
                        });
                    }
                    Err(e) => warn!("TCP accept error: {e}"),
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    conn_id: u64,
    stream: S,
    peer: SocketAddr,
    tls_cfg: Arc<TlsServerConfig>,
    cert_fp: [u8; 32],
    cred: Arc<StoredCredential>,
    limiter: Arc<Mutex<IpRateLimiter>>,
    hub: AgentHub,
    sas_enabled: bool,
    roster: watch::Sender<Roster>,
    policy: Arc<ServerPolicy>,
    chat_tx: broadcast::Sender<(u64, CommsMsg)>,
    preauth_timeout: Duration,
    preauth_permit: tokio::sync::OwnedSemaphorePermit,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let peer_ip = peer.ip();
    if let Err(reason) = evaluate_ip_access(peer_ip, &policy.ip_allowlist, &policy.ip_blocklist) {
        log_audit_event(
            policy.audit_log_path.as_deref(),
            &peer.to_string(),
            "connect_denied",
            &reason,
        );
        return Err(reason);
    }

    let (tls, mut mode, lease, video_decoders) = match tokio::time::timeout(
        preauth_timeout,
        authenticate_stream(
            stream,
            peer_ip,
            tls_cfg,
            cert_fp,
            &cred,
            &limiter,
            roster,
            conn_id,
            &policy,
        ),
    )
    .await
    {
        Ok(Ok(ok)) => ok,
        Ok(Err(e)) => {
            log_audit_event(
                policy.audit_log_path.as_deref(),
                &peer.to_string(),
                "auth_failed",
                &e,
            );
            return Err(e);
        }
        Err(_) => return Err("authentication timed out".into()),
    };
    drop(preauth_permit);

    info!(%peer, ?mode, "Viewer authenticated");
    log_audit_event(
        policy.audit_log_path.as_deref(),
        &peer.to_string(),
        "auth_ok",
        &format!("{mode:?}"),
    );

    let needs_desktop = mode != SessionMode::FileTransfer;
    if needs_desktop {
        let _ = hub.viewer_wake_tx.send(ViewerActivity::Connected);
        let _ = hub.to_agent_tx.try_send(IpcMsg::QualityPreset {
            mode: policy.default_quality,
            target_fps: policy.max_fps,
            h264_bitrate_kbps: 4000,
        });
        let _ = hub
            .to_agent_tx
            .try_send(IpcMsg::SetCaptureCursor(policy.capture_cursor));
        let _ = hub.to_agent_tx.try_send(IpcMsg::SetClipboardLimit(policy.clipboard_max_bytes as u32));
        if mode == SessionMode::FullControl && policy.curtain_on_control && policy.os_prompts_enabled() {
            let _ = hub.to_agent_tx.send(IpcMsg::PolicyCurtain { viewer: conn_id, enabled: true }).await;
        }
    }

    let res = run_authenticated_session(
        conn_id,
        &peer.to_string(),
        tls,
        &mut mode,
        &hub,
        sas_enabled,
        &lease,
        &policy,
        chat_tx,
        &mut shutdown_rx,
        video_decoders,
    )
    .await;

    if needs_desktop {
        if mode == SessionMode::FullControl {
            let _ = hub.to_agent_tx.try_send(IpcMsg::Input(InputMsg::ReleaseAllKeys));
            if policy.curtain_on_control && policy.os_prompts_enabled() {
                let _ = hub.to_agent_tx.send(IpcMsg::PolicyCurtain { viewer: conn_id, enabled: false }).await;
            }
        }
        let _ = hub.viewer_wake_tx.send(ViewerActivity::Disconnected);
    }
    log_audit_event(
        policy.audit_log_path.as_deref(),
        &peer.to_string(),
        "disconnect",
        res.as_ref().err().map(String::as_str).unwrap_or("clean"),
    );
    drop(lease);

    res
}

async fn approve_local_access(peer: Option<IpAddr>, mode: SessionMode, policy: &ServerPolicy) -> bool {
    if !policy.confirm_connect || !policy.os_prompts_enabled() {
        return true;
    }
    let default_allow = policy.confirm_default_allow;
    let Some(peer) = peer else {
        return default_allow;
    };
    let timeout = policy.confirm_timeout_secs;
    tokio::task::spawn_blocking(move || {
        prompt_connection_confirmation(peer, mode, timeout, default_allow)
    })
    .await
    .unwrap_or(default_allow)
}

#[allow(clippy::too_many_arguments)]
async fn authenticate_stream<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    peer_ip: IpAddr,
    tls_cfg: Arc<TlsServerConfig>,
    cert_fp: [u8; 32],
    cred: &StoredCredential,
    limiter: &Mutex<IpRateLimiter>,
    roster: watch::Sender<Roster>,
    conn_id: u64,
    policy: &ServerPolicy,
) -> Result<(ServerTlsStream<S>, SessionMode, ViewerLease, VideoCapabilities), String> {
    let mut tls = accept_tls(stream, tls_cfg)
        .await
        .map_err(|e| format!("TLS handshake failed: {e}"))?;

    let hello_frame = read_frame_async(&mut tls)
        .await
        .map_err(|e| format!("read ClientHello: {e}"))?
        .ok_or_else(|| "EOF waiting for ClientHello".to_string())?;
    if hello_frame.channel != ChannelId::AuthControl {
        return Err("expected AuthControl channel for ClientHello".into());
    }
    let ControlMsg::ClientHello {
        proto_version,
        client_nonce,
        requested_mode,
        client_name,
        video_decoders,
    } = unpack_message::<ControlMsg>(&hello_frame).map_err(|e| e.to_string())?
    else {
        return Err("expected ClientHello message".into());
    };

    if proto_version != PROTO_VERSION {
        let reject = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::AuthResult {
                accepted: false,
                server_proof: [0u8; 32],
                granted_mode: requested_mode,
                retry_after_secs: 0,
                reason: format!("Unsupported protocol version {proto_version}."),
            },
            None,
        )
        .map_err(|e| e.to_string())?;
        let _ = write_frame_async(&mut tls, reject.channel, reject.flags, &reject.payload).await;
        return Err("protocol version mismatch".into());
    }

    let cooldown = {
        let mut lim = limiter.lock().expect("limiter mutex");
        lim.check(peer_ip, Instant::now()).err()
    };
    if let Some(retry_after_secs) = cooldown {
        let reject = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::AuthResult {
                accepted: false,
                server_proof: [0u8; 32],
                granted_mode: requested_mode,
                retry_after_secs,
                reason: format!("Too many failed attempts. Try again in {retry_after_secs} seconds."),
            },
            None,
        )
        .map_err(|e| e.to_string())?;
        let _ = write_frame_async(&mut tls, reject.channel, reject.flags, &reject.payload).await;
        return Err(format!("rate limited ({retry_after_secs}s)"));
    }

    let server_nonce = random_bytes::<NONCE_LEN>().map_err(|e| e.to_string())?;
    let challenge = pack_message(
        ChannelId::AuthControl,
        FLAG_URGENT,
        &ControlMsg::ServerChallenge {
            server_nonce,
            argon2_salt: cred.salt,
            argon2_m_kib: cred.m_kib.max(ARGON2_M_KIB.min(1024)),
            argon2_t: cred.t_cost.max(ARGON2_T_COST.min(1)),
            argon2_p: cred.p_cost.max(ARGON2_P_COST.min(1)),
        },
        None,
    )
    .map_err(|e| e.to_string())?;
    let _ = SALT_LEN;
    write_frame_async(&mut tls, challenge.channel, challenge.flags, &challenge.payload)
        .await
        .map_err(|e| e.to_string())?;

    let auth_frame = read_frame_async(&mut tls)
        .await
        .map_err(|e| format!("read ClientAuth: {e}"))?
        .ok_or_else(|| "EOF waiting for ClientAuth".to_string())?;
    if auth_frame.channel != ChannelId::AuthControl {
        return Err("expected AuthControl for ClientAuth".into());
    }
    let ControlMsg::ClientAuth { client_proof } =
        unpack_message::<ControlMsg>(&auth_frame).map_err(|e| e.to_string())?
    else {
        return Err("expected ClientAuth message".into());
    };

    if !verify_client_proof(&cred.auth_key, &client_nonce, &server_nonce, &cert_fp, &client_proof) {
        let retry_after_secs = {
            let mut lim = limiter.lock().expect("limiter mutex");
            lim.record_failure(peer_ip, Instant::now())
        };
        let reject = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::AuthResult {
                accepted: false,
                server_proof: [0u8; 32],
                granted_mode: requested_mode,
                retry_after_secs,
                reason: "Authentication failed. Check password and try again.".into(),
            },
            None,
        )
        .map_err(|e| e.to_string())?;
        let _ = write_frame_async(&mut tls, reject.channel, reject.flags, &reject.payload).await;
        return Err("invalid client auth proof".into());
    }

    {
        let mut lim = limiter.lock().expect("limiter mutex");
        lim.record_success(peer_ip);
    }

    let effective_requested = if policy.force_view_only && requested_mode == SessionMode::FullControl {
        SessionMode::ViewOnly
    } else {
        requested_mode
    };
    if !policy.permissions.allows_mode(effective_requested) {
        let reject = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::AuthResult {
                accepted: false,
                server_proof: [0u8; 32],
                granted_mode: requested_mode,
                retry_after_secs: 0,
                reason: "Requested session mode is disabled by host administrator.".into(),
            },
            None,
        )
        .map_err(|e| e.to_string())?;
        let _ = write_frame_async(&mut tls, reject.channel, reject.flags, &reject.payload).await;
        return Err("requested mode disallowed by host policy".into());
    }

    if !approve_local_access(Some(peer_ip), effective_requested, policy).await {
        let reject = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::AuthResult {
                accepted: false,
                server_proof: [0u8; 32],
                granted_mode: requested_mode,
                retry_after_secs: 0,
                reason: "Connection declined by local desktop user.".into(),
            },
            None,
        )
        .map_err(|e| e.to_string())?;
        let _ = write_frame_async(&mut tls, reject.channel, reject.flags, &reject.payload).await;
        return Err("connection rejected by local user confirmation".into());
    }

    let (lease, granted_mode) = ViewerLease::join(roster, conn_id, &client_name, effective_requested)?;
    if granted_mode != effective_requested && !policy.permissions.allows_mode(granted_mode) {
        return Err("View Only mode is disabled by the host.".into());
    }

    let server_proof = compute_server_proof(&cred.auth_key, &server_nonce, &client_nonce, &cert_fp);
    let ok = pack_message(
        ChannelId::AuthControl,
        FLAG_URGENT,
        &ControlMsg::AuthResult {
            accepted: true,
            server_proof,
            granted_mode,
            retry_after_secs: 0,
            reason: String::new(),
        },
        None,
    )
    .map_err(|e| e.to_string())?;
    write_frame_async(&mut tls, ok.channel, ok.flags, &ok.payload)
        .await
        .map_err(|e| e.to_string())?;

    Ok((tls, granted_mode, lease, video_decoders))
}

struct OutboundQueues {
    high: VecDeque<Frame>,
    clip: VecDeque<Frame>,
    aux: VecDeque<Frame>,
    video: VecDeque<Frame>,
    bulk: VecDeque<Frame>,
}

impl OutboundQueues {
    fn new() -> Self {
        Self {
            high: VecDeque::new(),
            clip: VecDeque::new(),
            aux: VecDeque::new(),
            video: VecDeque::new(),
            bulk: VecDeque::new(),
        }
    }

    fn push(&mut self, frame: Frame, to_agent_tx: &mpsc::Sender<IpcMsg>) {
        match frame.channel {
            ChannelId::AuthControl
            | ChannelId::Input
            | ChannelId::Terminal
            | ChannelId::Comms
            | ChannelId::PowerControl => {
                self.high.push_back(frame);
            }
            ChannelId::Clipboard => {
                if self.clip.len() >= 4 {
                    self.clip.pop_front();
                }
                self.clip.push_back(frame);
            }
            ChannelId::Tunnel | ChannelId::SysAdmin => {
                self.aux.push_back(frame);
            }
            ChannelId::VideoTiles if frame.flags & FLAG_URGENT != 0 => self.high.push_back(frame),
            ChannelId::VideoTiles => {
                if self.video.len() >= MAX_VIDEO_QUEUE {
                    // Drop oldest stale video frame and request fresh keyframe recovery
                    self.video.pop_front();
                    let _ = to_agent_tx.try_send(IpcMsg::KeyframeRequest { monitor_id: 0 });
                }
                self.video.push_back(frame);
            }
            ChannelId::FileTransfer => self.bulk.push_back(frame),
        }
    }

    fn pop_next(&mut self) -> Option<Frame> {
        self.high
            .pop_front()
            .or_else(|| self.clip.pop_front())
            .or_else(|| self.aux.pop_front())
            .or_else(|| self.video.pop_front())
            .or_else(|| self.bulk.pop_front())
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_authenticated_session<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    conn_id: u64,
    peer_str: &str,
    tls: ServerTlsStream<S>,
    mode: &mut SessionMode,
    hub: &AgentHub,
    sas_enabled: bool,
    lease: &ViewerLease,
    policy: &Arc<ServerPolicy>,
    chat_tx: broadcast::Sender<(u64, CommsMsg)>,
    shutdown_rx: &mut watch::Receiver<bool>,
    video_decoders: VideoCapabilities,
) -> Result<(), String> {
    let (mut reader, mut writer) = tokio::io::split(tls);
    let mut frames = AsyncFrameReader::default();
    let mut agent_rx = hub.from_agent_tx.subscribe();
    let mut chat_rx = chat_tx.subscribe();
    let mut file_handler = FileSessionHandler::with_part_ttl(policy.part_ttl_hours);
    let (term_tx, mut term_rx) = mpsc::channel::<TerminalMsg>(64);
    let mut term_handler = TerminalSessionHandler::new(term_tx);
    let (tun_tx, mut tun_rx) = mpsc::channel::<TunnelMsg>(128);
    let mut tun_handler = TunnelSessionHandler::new(tun_tx);
    let (sys_tx, mut sys_rx) = mpsc::channel::<SysAdminMsg>(32);
    let mut sys_handler = SysAdminHandler::new();
    let (voice_tx, mut voice_rx) = mpsc::channel::<Vec<u8>>(64);
    let mut voice_engine: Option<VoiceEngine> = None;
    let mut system_audio = SystemAudioState::Idle;
    let mut video_feedback = VideoFeedback { decoders: video_decoders, ..VideoFeedback::default() };

    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(128);
    let wake_agent = hub.to_agent_tx.clone();
    let writer_task = tokio::spawn(async move {
        let mut queues = OutboundQueues::new();
        loop {
            let first = match out_rx.recv().await {
                Some(f) => f,
                None => break,
            };
            queues.push(first, &wake_agent);
            while let Ok(more) = out_rx.try_recv() {
                queues.push(more, &wake_agent);
            }
            while let Some(frame) = queues.pop_next() {
                if write_frame_async(&mut writer, frame.channel, frame.flags, &frame.payload)
                    .await
                    .is_err()
                {
                    return;
                }
                while let Ok(more) = out_rx.try_recv() {
                    queues.push(more, &wake_agent);
                }
            }
        }
    });

    // Send current known monitor list immediately if not FileTransfer-only
    if *mode != SessionMode::FileTransfer {
        let _ = hub.to_agent_tx.try_send(IpcMsg::VideoDecoderSupport { viewer: conn_id, capabilities: Some(video_decoders) });
        #[cfg(target_os = "android")]
        if let Ok(frame) = pack_message(ChannelId::AuthControl, FLAG_NONE, &ControlMsg::AndroidControls, None) {
            let _ = out_tx.send(frame).await;
        }
        let (monitors, active) = hub.monitors_rx.borrow().clone();
        if !monitors.is_empty()
            && let Ok(frame) = pack_message(
                ChannelId::AuthControl,
                FLAG_NONE,
                &ControlMsg::MonitorList {
                    monitors,
                    active_monitor_id: active,
                },
                None,
            )
        {
            let _ = out_tx.send(frame).await;
        }
        let _ = hub.to_agent_tx.try_send(IpcMsg::KeyframeRequest { monitor_id: active });
    }

    let mut download_pump = tokio::time::interval(Duration::from_millis(25));
    let mut keepalive_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(10),
        Duration::from_secs(10),
    );
    let mut probe_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(2), Duration::from_secs(2),
    );
    probe_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut roster = lease.subscribe();
    let mut watching_viewers = false;
    let mut last_rx = Instant::now();
    let mut disconnect = crate::platform::subscribe_disconnect();

    let result = loop {
        tokio::select! {
            _ = disconnect.changed() => break Ok(()),
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break Ok(());
                }
            }
            changed = roster.changed(), if *mode != SessionMode::FileTransfer => {
                if changed.is_err() {
                    break Ok(());
                }
                let (controlling, snapshot) = {
                    let current = roster.borrow_and_update();
                    (current.controlling(conn_id), watching_viewers.then(|| current.snapshot(conn_id)))
                };
                if controlling && *mode == SessionMode::ViewOnly {
                    activate_control(mode, hub, conn_id, policy, &out_tx).await;
                }
                if let Some(snapshot) = snapshot {
                    send_control(&out_tx, snapshot).await;
                }
            }
            _ = keepalive_tick.tick() => {
                if last_rx.elapsed() > Duration::from_secs(35) {
                    break Err("session keepalive timeout".into());
                }
                if *mode == SessionMode::FileTransfer
                    && let Ok(frame) = pack_message(ChannelId::AuthControl, FLAG_NONE, &ControlMsg::Ping { timestamp_ms: 0 }, None)
                {
                    let _ = out_tx.try_send(frame);
                }
            }
            _ = probe_tick.tick(), if *mode != SessionMode::FileTransfer => {
                let _ = hub.to_agent_tx.try_send(IpcMsg::VideoDecoderSupport {
                    viewer: conn_id, capabilities: Some(video_feedback.decoders),
                });
                if video_feedback.monitor_workspace {
                    let _ = hub.to_agent_tx.try_send(IpcMsg::MonitorWorkspace { viewer: conn_id, enabled: true });
                }
                let nonce = video_feedback.begin_probe();
                if let Ok(frame) = pack_message(ChannelId::AuthControl, FLAG_URGENT, &ControlMsg::Ping { timestamp_ms: nonce }, None) {
                    let _ = out_tx.try_send(frame);
                }
            }
            _ = download_pump.tick() => {
                let bursts = file_handler.poll_downloads().await;
                for ft in bursts {
                    if let Ok(f) = file_handler.pack_reply(&ft)
                        && out_tx.send(f).await.is_err()
                    {
                        break;
                    }
                }
            }
            Some(tmsg) = term_rx.recv() => {
                if let Ok(f) = pack_message(ChannelId::Terminal, FLAG_NONE, &tmsg, None) {
                    let _ = out_tx.send(f).await;
                }
            }
            Some(tun_msg) = tun_rx.recv() => {
                if let Ok(f) = pack_message(ChannelId::Tunnel, FLAG_NONE, &tun_msg, None) {
                    let _ = out_tx.send(f).await;
                }
            }
            Some(smsg) = sys_rx.recv() => {
                if let Ok(f) = pack_message(ChannelId::SysAdmin, FLAG_NONE, &smsg, Some(1024)) {
                    let _ = out_tx.send(f).await;
                }
            }
            Some(packet) = voice_rx.recv() => {
                if let Ok(f) = pack_message(ChannelId::Comms, FLAG_NONE, &CommsMsg::VoiceFrame(packet), None) {
                    let _ = out_tx.try_send(f);
                }
            }
            Ok((src_id, cmsg)) = chat_rx.recv() => {
                if src_id == conn_id { continue; }
                let ghost = matches!(cmsg, CommsMsg::GhostCursor { .. });
                let allowed = if ghost { *mode != SessionMode::FileTransfer } else { policy.permissions.text_chat };
                if allowed && let Ok(f) = pack_message(ChannelId::Comms, FLAG_NONE, &cmsg, None) {
                    if ghost {
                        let _ = out_tx.try_send(f);
                    } else {
                        let _ = out_tx.send(f).await;
                    }
                }
            }
            ev = agent_rx.recv(), if *mode != SessionMode::FileTransfer => {
                match ev {
                    Ok(msg) => {
                        match msg.as_ref() {
                            IpcMsg::DesktopActionResult { viewer, action, accepted, message } if *viewer == conn_id => {
                                let response = ControlMsg::DesktopActionResult { action: *action, accepted: *accepted, message: message.clone() };
                                if let Ok(frame) = pack_message(ChannelId::AuthControl, FLAG_URGENT, &response, None) {
                                    let _ = out_tx.send(frame).await;
                                }
                                continue;
                            }
                            IpcMsg::AnnotationUnavailable { viewer, reason } if *viewer == conn_id => {
                                send_control_error(&out_tx, reason);
                                continue;
                            }
                            IpcMsg::SystemAudioReady { viewer, error } if *viewer == conn_id => {
                                if system_audio == SystemAudioState::Pending {
                                    if let Some(error) = error {
                                        system_audio = SystemAudioState::Idle;
                                        send_control_error(&out_tx, &format!("System audio unavailable: {error}"));
                                        send_system_audio(&out_tx, CommsMsg::SystemAudioStop).await;
                                    } else {
                                        system_audio = SystemAudioState::Active;
                                        send_system_audio(&out_tx, CommsMsg::SystemAudioOffer).await;
                                    }
                                }
                                continue;
                            }
                            IpcMsg::SystemAudioFrame(packet) => {
                                if system_audio == SystemAudioState::Active
                                    && let Ok(frame) = pack_message(ChannelId::Comms, FLAG_NONE, &CommsMsg::SystemAudioFrame(packet.clone()), None)
                                {
                                    let _ = out_tx.try_send(frame);
                                }
                                continue;
                            }
                            IpcMsg::SystemAudioFailed(error) => {
                                if system_audio != SystemAudioState::Idle {
                                    system_audio = SystemAudioState::Idle;
                                    let _ = hub.to_agent_tx.try_send(IpcMsg::SystemAudioStop(conn_id));
                                    send_control_error(&out_tx, &format!("System audio stopped: {error}"));
                                    send_system_audio(&out_tx, CommsMsg::SystemAudioStop).await;
                                }
                                continue;
                            }
                            IpcMsg::AgentReady { .. } if system_audio != SystemAudioState::Idle => {
                                system_audio = SystemAudioState::Idle;
                                send_system_audio(&out_tx, CommsMsg::SystemAudioStop).await;
                            }
                            _ => {}
                        }
                        if let IpcMsg::Clipboard(clipboard) = msg.as_ref()
                            && (*mode != SessionMode::FullControl || !clipboard.within_limit(policy.clipboard_max_bytes))
                        {
                            continue;
                        }
                        if !policy.capture_cursor && matches!(msg.as_ref(), IpcMsg::Video(VideoMsg::CursorUpdate { .. })) {
                            continue;
                        }
                        match msg.as_ref() {
                            IpcMsg::Video(VideoMsg::EncodedFrame { codec, .. })
                                if !video_feedback.decoders.includes(*codec) => continue,
                            IpcMsg::Video(VideoMsg::H264Frame { frame_seq, .. }
                                | VideoMsg::EncodedFrame { frame_seq, .. }
                                | VideoMsg::TileBatch { frame_seq, .. }) => {
                                if *frame_seq < video_feedback.last_frame {
                                    video_feedback.last_ack = 0;
                                }
                                video_feedback.last_frame = *frame_seq;
                            }
                            _ => {}
                        }
                        if let Some(frame) = ipc_to_wire_frame(&msg) {
                            if frame.channel == ChannelId::VideoTiles && frame.flags & FLAG_URGENT != 0 {
                                let _ = out_tx.send(frame).await;
                            } else {
                                let _ = out_tx.try_send(frame);
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let active = hub.monitors_rx.borrow().1;
                        let _ = hub.to_agent_tx.try_send(IpcMsg::KeyframeRequest { monitor_id: active });
                    }
                    Err(broadcast::error::RecvError::Closed) => break Ok(()),
                }
            }
            frame_res = frames.read(&mut reader) => {
                match frame_res {
                    Ok(None) => break Ok(()),
                    Ok(Some(frame)) => {
                        last_rx = Instant::now();
                        if let Err(e) = dispatch_client_frame(
                            conn_id,
                            peer_str,
                            frame,
                            mode,
                            hub,
                            sas_enabled,
                            lease,
                            &mut watching_viewers,
                            policy,
                            &chat_tx,
                            &out_tx,
                            &mut file_handler,
                            &mut term_handler,
                            &mut tun_handler,
                            &mut sys_handler,
                            &sys_tx,
                            &mut voice_engine,
                            &voice_tx,
                            &mut system_audio,
                            &mut video_feedback,
                        ).await {
                            if e == "client requested disconnect" {
                                break Ok(());
                            }
                            break Err(e);
                        }
                    }
                    Err(e) => break Err(format!("read error: {e}")),
                }
            }
        }
    };

    if system_audio != SystemAudioState::Idle {
        let _ = hub.to_agent_tx.send(IpcMsg::SystemAudioStop(conn_id)).await;
    }
    if *mode != SessionMode::FileTransfer {
        let _ = hub.to_agent_tx.send(IpcMsg::VideoDecoderSupport { viewer: conn_id, capabilities: None }).await;
    }
    if video_feedback.desktop_settings_used {
        let _ = hub.to_agent_tx.send(IpcMsg::ReleaseDesktopSettings(conn_id)).await;
    }
    if video_feedback.monitor_workspace {
        let _ = hub.to_agent_tx.send(IpcMsg::MonitorWorkspace { viewer: conn_id, enabled: false }).await;
    }
    if video_feedback.level > 0 {
        let _ = hub.to_agent_tx.try_send(IpcMsg::VideoCongestion { viewer: conn_id, level: 0 });
    }
    drop(voice_engine);
    term_handler.cleanup_all();
    tun_handler.cleanup_all().await;
    file_handler.cleanup_all();
    drop(out_tx);
    let _ = writer_task.await;
    result
}

#[derive(Default)]
struct VideoFeedback {
    decoders: VideoCapabilities,
    monitor_workspace: bool,
    desktop_settings_used: bool,
    last_annotation: Option<Instant>,
    last_ghost: Option<Instant>,
    probe: Option<(u64, Instant)>,
    next_probe: u64,
    rtt: Option<(u64, Instant)>,
    last_frame: u64,
    last_ack: u64,
    last_report: Option<Instant>,
    level: u8,
    clean_samples: u8,
}

impl VideoFeedback {
    fn begin_probe(&mut self) -> u64 {
        self.next_probe = self.next_probe.wrapping_add(1).max(1);
        self.probe = Some((self.next_probe, Instant::now()));
        self.next_probe
    }

    fn pong(&mut self, nonce: u64) {
        if let Some((sent, start)) = self.probe
            && sent == nonce
        {
            self.rtt = Some((start.elapsed().as_millis() as u64, Instant::now()));
            self.probe = None;
        }
    }

    fn acknowledge(&mut self, sequence: u64, decode_ms: u16, now: Instant) -> Option<u8> {
        if sequence <= self.last_ack || sequence > self.last_frame {
            return None;
        }
        self.last_ack = sequence;
        if self.last_report.is_some_and(|t| now.duration_since(t) < Duration::from_millis(250)) {
            return None;
        }
        self.last_report = Some(now);
        let rtt = self.rtt.filter(|(_, t)| now.duration_since(*t) <= Duration::from_secs(5))
            .map_or(0, |(ms, _)| ms);
        if rtt >= 250 || decode_ms >= 100 {
            self.level = 2;
            self.clean_samples = 0;
        } else if rtt >= 140 || decode_ms >= 45 {
            self.level = self.level.max(1);
            self.clean_samples = 0;
        } else if rtt < 100 && decode_ms < 30 {
            self.clean_samples += 1;
            if self.clean_samples == 4 {
                self.level = self.level.saturating_sub(1);
                self.clean_samples = 0;
            }
        } else {
            self.clean_samples = 0;
        }
        Some(self.level)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SystemAudioState {
    Idle,
    Pending,
    Active,
}

async fn send_system_audio(out: &mpsc::Sender<Frame>, message: CommsMsg) {
    if let Ok(frame) = pack_message(ChannelId::Comms, FLAG_NONE, &message, None) {
        let _ = out.send(frame).await;
    }
}

fn ipc_to_wire_frame(msg: &IpcMsg) -> Option<Frame> {
    match msg {
        IpcMsg::AgentReady {
            monitors,
            active_monitor_id,
            ..
        }
        | IpcMsg::MonitorList {
            monitors,
            active_monitor_id,
        } => pack_message(
            ChannelId::AuthControl,
            FLAG_NONE,
            &ControlMsg::MonitorList {
                monitors: monitors.clone(),
                active_monitor_id: *active_monitor_id,
            },
            None,
        )
        .ok(),
        IpcMsg::ResolutionChange {
            monitor_id,
            width,
            height,
            dpi_scale,
        } => pack_message(
            ChannelId::AuthControl,
            FLAG_NONE,
            &ControlMsg::ResolutionChange {
                monitor_id: *monitor_id,
                width: *width,
                height: *height,
                dpi_scale: *dpi_scale,
            },
            None,
        )
        .ok(),
        IpcMsg::DesktopStatus {
            state,
            desktop_name,
        } => pack_message(
            ChannelId::AuthControl,
            FLAG_NONE,
            &ControlMsg::SessionStatus {
                desktop_state: *state,
                message: desktop_name.clone(),
            },
            None,
        )
        .ok(),
        IpcMsg::Video(v) => {
            let flags = match v {
                VideoMsg::H264Frame { is_keyframe: true, .. }
                | VideoMsg::EncodedFrame { is_keyframe: true, .. }
                | VideoMsg::TileBatch { is_full_refresh: true, .. } => FLAG_KEYFRAME,
                VideoMsg::CursorUpdate { .. } => FLAG_URGENT,
                _ => FLAG_NONE,
            };
            pack_message(ChannelId::VideoTiles, flags, v, None).ok()
        }
        IpcMsg::Clipboard(c) => pack_message(ChannelId::Clipboard, FLAG_NONE, c, None).ok(),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_client_frame(
    conn_id: u64,
    peer_str: &str,
    frame: Frame,
    mode: &mut SessionMode,
    hub: &AgentHub,
    sas_enabled: bool,
    lease: &ViewerLease,
    watching_viewers: &mut bool,
    policy: &ServerPolicy,
    chat_tx: &broadcast::Sender<(u64, CommsMsg)>,
    out_tx: &mpsc::Sender<Frame>,
    file_handler: &mut FileSessionHandler,
    term_handler: &mut TerminalSessionHandler,
    tun_handler: &mut TunnelSessionHandler,
    sys_handler: &mut SysAdminHandler,
    sys_tx: &mpsc::Sender<SysAdminMsg>,
    voice_engine: &mut Option<VoiceEngine>,
    voice_tx: &mpsc::Sender<Vec<u8>>,
    system_audio: &mut SystemAudioState,
    video_feedback: &mut VideoFeedback,
) -> Result<(), String> {
    match frame.channel {
        ChannelId::AuthControl => {
            let msg: ControlMsg = unpack_message(&frame).map_err(|e| e.to_string())?;
            match msg {
                ControlMsg::Ping { timestamp_ms } => {
                    if let Ok(pong) = pack_message(
                        ChannelId::AuthControl,
                        FLAG_URGENT,
                        &ControlMsg::Pong { timestamp_ms },
                        None,
                    ) {
                        let _ = out_tx.try_send(pong);
                    }
                }
                ControlMsg::Pong { timestamp_ms } => video_feedback.pong(timestamp_ms),
                ControlMsg::VideoCapabilities(capabilities) if *mode != SessionMode::FileTransfer => {
                    video_feedback.decoders = video_feedback.decoders.intersect(capabilities);
                    let _ = hub.to_agent_tx.try_send(IpcMsg::VideoDecoderSupport {
                        viewer: conn_id, capabilities: Some(video_feedback.decoders),
                    });
                }
                ControlMsg::FrameAck { last_seq, decode_ms } if *mode != SessionMode::FileTransfer => {
                    if let Some(level) = video_feedback.acknowledge(last_seq, decode_ms, Instant::now()) {
                        let _ = hub.to_agent_tx.try_send(IpcMsg::VideoCongestion { viewer: conn_id, level });
                    }
                }
                ControlMsg::MonitorWorkspace { enabled } if *mode != SessionMode::FileTransfer => {
                    video_feedback.monitor_workspace = enabled;
                    let _ = hub.to_agent_tx.send(IpcMsg::MonitorWorkspace { viewer: conn_id, enabled }).await;
                }
                ControlMsg::MonitorSelect { monitor_id } if *mode != SessionMode::FileTransfer => {
                    let _ = hub.to_agent_tx.try_send(IpcMsg::MonitorSelect { monitor_id });
                }
                ControlMsg::QualityPreset {
                    mode: qm,
                    target_fps,
                    h264_bitrate_kbps,
                } if *mode != SessionMode::FileTransfer => {
                    let _ = hub.to_agent_tx.try_send(IpcMsg::QualityPreset {
                        mode: qm,
                        target_fps: target_fps.min(policy.max_fps),
                        h264_bitrate_kbps,
                    });
                }
                ControlMsg::KeyframeRequest { monitor_id } if *mode != SessionMode::FileTransfer => {
                    let _ = hub.to_agent_tx.try_send(IpcMsg::KeyframeRequest { monitor_id });
                }
                ControlMsg::WatchViewers if *mode != SessionMode::FileTransfer => {
                    *watching_viewers = true;
                    let snapshot = lease.subscribe().borrow().snapshot(conn_id);
                    send_control(out_tx, snapshot).await;
                }
                ControlMsg::RequestControl | ControlMsg::SessionModeSwitch { mode: SessionMode::FullControl }
                    if *mode == SessionMode::ViewOnly =>
                {
                    if policy.force_view_only || !policy.permissions.allows_mode(SessionMode::FullControl) {
                        send_control_error(out_tx, "Host does not allow Full Control.");
                        return Ok(());
                    }
                    let peer_ip = peer_str.parse::<SocketAddr>().ok().map(|peer| peer.ip());
                    if !approve_local_access(peer_ip, SessionMode::FullControl, policy).await {
                        send_control_error(out_tx, "Control request declined by the host.");
                        return Ok(());
                    }
                    match lease.acquire_or_request(matches!(msg, ControlMsg::RequestControl)) {
                        Ok(true) => activate_control(mode, hub, conn_id, policy, out_tx).await,
                        Ok(false) => send_control(out_tx, ControlMsg::ControlRequestResult {
                            message: "Control requested. Waiting for the current controller.".into(),
                        }).await,
                        Err(reason) => send_control_error(out_tx, reason),
                    }
                }
                ControlMsg::SessionModeSwitch { mode: SessionMode::ViewOnly }
                    if *mode == SessionMode::FullControl =>
                {
                    if !policy.permissions.allows_mode(SessionMode::ViewOnly) {
                        send_control_error(out_tx, "Host does not allow View Only.");
                        return Ok(());
                    }
                    relinquish_control(hub, conn_id, policy, video_feedback).await;
                    term_handler.cleanup_all();
                    tun_handler.cleanup_all().await;
                    file_handler.cleanup_all();
                    lease.release();
                    *mode = SessionMode::ViewOnly;
                    send_control(out_tx, ControlMsg::SessionModeSwitch { mode: *mode }).await;
                }
                ControlMsg::HandOffControl { viewer, approved }
                    if *mode == SessionMode::FullControl && lease.can_handoff(viewer) =>
                {
                    if !approved {
                        lease.handoff(viewer, false);
                        return Ok(());
                    }
                    relinquish_control(hub, conn_id, policy, video_feedback).await;
                    term_handler.cleanup_all();
                    tun_handler.cleanup_all().await;
                    file_handler.cleanup_all();
                    if lease.handoff(viewer, true) {
                        *mode = SessionMode::ViewOnly;
                        send_control(out_tx, ControlMsg::SessionModeSwitch { mode: *mode }).await;
                    } else if policy.curtain_on_control && policy.os_prompts_enabled() {
                        let _ = hub.to_agent_tx.send(IpcMsg::PolicyCurtain { viewer: conn_id, enabled: true }).await;
                    }
                }
                ControlMsg::SpecialAction(action) => {
                    if *mode != SessionMode::FullControl {
                        if let Ok(err) = pack_message(
                            ChannelId::AuthControl,
                            FLAG_URGENT,
                            &ControlMsg::Error {
                                message: "Action not permitted in View Only mode.".into(),
                            },
                            None,
                        ) {
                            let _ = out_tx.try_send(err);
                        }
                        return Ok(());
                    }
                    match action {
                        SpecialAction::SendCtrlAltDel => {
                            if sas_enabled {
                                if let Err(e) = trigger_send_sas()
                                    && let Ok(err) = pack_message(
                                        ChannelId::AuthControl,
                                        FLAG_URGENT,
                                        &ControlMsg::Error { message: e },
                                        None,
                                    )
                                {
                                    let _ = out_tx.try_send(err);
                                }
                            } else if let Ok(err) = pack_message(
                                ChannelId::AuthControl,
                                FLAG_URGENT,
                                &ControlMsg::Error {
                                    message: "Ctrl+Alt+Del requires running DeskBolt as a Windows Service.".into(),
                                },
                                None,
                            ) {
                                let _ = out_tx.try_send(err);
                            }
                        }
                        SpecialAction::ReleaseAllKeys => {
                            let _ = hub.to_agent_tx.try_send(IpcMsg::Input(InputMsg::ReleaseAllKeys));
                        }
                        SpecialAction::SetDisplayMode { .. } | SpecialAction::RestoreDisplayMode
                            | SpecialAction::PrivacyCurtain { .. } | SpecialAction::LockLocalInput { .. } => {
                            let (permission, denial) = match action {
                                SpecialAction::PrivacyCurtain { .. } =>
                                    (policy.permissions.privacy_curtain, "The privacy curtain is disabled by the host."),
                                SpecialAction::LockLocalInput { .. } =>
                                    (policy.permissions.local_input_lock, "Locking local input is disabled by the host."),
                                _ => (policy.permissions.display_mode, "Changing display modes is disabled by the host."),
                            };
                            let failure = if !permission || !policy.os_prompts_enabled() {
                                Some(denial)
                            } else if hub.to_agent_tx.try_send(IpcMsg::DesktopAction { viewer: conn_id, action }).is_err() {
                                Some("The host desktop is temporarily unavailable.")
                            } else {
                                video_feedback.desktop_settings_used = true;
                                None
                            };
                            if let Some(message) = failure {
                                let response = ControlMsg::DesktopActionResult { action, accepted: false, message: message.into() };
                                if let Ok(frame) = pack_message(ChannelId::AuthControl, FLAG_URGENT, &response, None) {
                                    let _ = out_tx.try_send(frame);
                                }
                            }
                        }
                    }
                }
                ControlMsg::Disconnect { .. } => {
                    return Err("client requested disconnect".into());
                }
                _ => {}
            }
        }
        ChannelId::Input => {
            if *mode != SessionMode::FullControl {
                // Trust boundary enforcement: silently reject input in ViewOnly/FileTransfer mode
                return Ok(());
            }
            let inp: InputMsg = unpack_message(&frame).map_err(|e| e.to_string())?;
            if matches!(inp, InputMsg::DeviceAction(deskbolt_proto::DeviceAction::LockScreen))
                && !policy.permissions.power_control
            {
                send_control_error(out_tx, "Remote lock is disabled by the host.");
                return Ok(());
            }
            let _ = hub.to_agent_tx.try_send(IpcMsg::Input(inp));
        }
        ChannelId::Clipboard => {
            if *mode != SessionMode::FullControl {
                return Ok(());
            }
            let clip: ClipboardSync = unpack_message(&frame).map_err(|e| e.to_string())?;
            if clip.within_limit(policy.clipboard_max_bytes) {
                let _ = hub.to_agent_tx.try_send(IpcMsg::Clipboard(clip));
            } else {
                send_control_error(out_tx, "Clipboard exceeds the host size limit.");
            }
        }
        ChannelId::FileTransfer => {
            if *mode == SessionMode::ViewOnly || !policy.permissions.file_transfer {
                let message = "File transfer is disabled by current session policy.";
                let reply = match unpack_message(&frame) {
                    Ok(FileTransferMsg::TransferStart { transfer_id, .. }) => FileTransferMsg::TransferStartAck {
                        transfer_id, accepted: false, exists: false, existing_size: 0,
                        existing_modified_unix: 0, resume_offset: 0, prefix_sha256: None,
                        error: Some(message.into()),
                    },
                    Ok(FileTransferMsg::HashTreeReq { root, .. }) => FileTransferMsg::HashTreeResp {
                        root, entries: Vec::new(), next_cursor: None, error: Some(message.into()),
                    },
                    _ => {
                        send_control_error(out_tx, message);
                        return Ok(());
                    }
                };
                if let Ok(f) = pack_message(ChannelId::FileTransfer, FLAG_NONE, &reply, None) {
                    let _ = out_tx.send(f).await;
                }
                return Ok(());
            }
            let ft: FileTransferMsg = unpack_message(&frame).map_err(|e| e.to_string())?;
            let replies = file_handler.handle_message(ft).await;
            for reply in replies {
                if let Ok(f) = file_handler.pack_reply(&reply) {
                    let _ = out_tx.send(f).await;
                }
            }
        }
        ChannelId::Terminal => {
            if *mode != SessionMode::FullControl || !policy.permissions.terminal {
                send_control_error(out_tx, "Terminal and serial access are disabled by host policy or session mode.");
                return Ok(());
            }
            let msg: TerminalMsg = unpack_message(&frame).map_err(|e| e.to_string())?;
            term_handler.handle_message(msg).await;
        }
        ChannelId::Tunnel => {
            if *mode != SessionMode::FullControl || !policy.permissions.tunnels {
                send_control_error(out_tx, "Port forwarding tunnels are disabled by host policy or session mode.");
                return Ok(());
            }
            let msg: TunnelMsg = unpack_message(&frame).map_err(|e| e.to_string())?;
            tun_handler.handle_message(msg).await;
        }
        ChannelId::SysAdmin => {
            if *mode != SessionMode::FullControl {
                send_control_error(out_tx, "System administration requires Full Control mode.");
                return Ok(());
            }
            let msg: SysAdminMsg = unpack_message(&frame).map_err(|e| e.to_string())?;
            match msg {
                SysAdminMsg::SystemSnapshotReq => {
                    let _ = sys_tx.send(sys_handler.snapshot()).await;
                }
                SysAdminMsg::KillProcess(pid) => {
                    let _ = sys_tx.send(sys_handler.kill_process(pid)).await;
                }
                SysAdminMsg::ServiceAction { name, action } => {
                    let _ = sys_tx.send(sys_handler.control_service(&name, action)).await;
                }
                SysAdminMsg::BroadcastExecReq {
                    command_id,
                    command,
                } => {
                    let tx = sys_tx.clone();
                    tokio::spawn(async move {
                        let res = tokio::task::spawn_blocking(move || {
                            execute_broadcast_command(command_id, &command)
                        })
                        .await
                        .unwrap_or_else(|e| SysAdminMsg::BroadcastExecResp {
                            command_id,
                            exit_code: -1,
                            output: String::new(),
                            error: Some(format!("broadcast task failed: {e}")),
                        });
                        let _ = tx.send(res).await;
                    });
                }
                _ => {}
            }
        }
        ChannelId::Comms => {
            let msg: CommsMsg = unpack_message(&frame).map_err(|e| e.to_string())?;
            match msg {
                CommsMsg::GhostCursor { cursor, .. } => {
                    let now = Instant::now();
                    if *mode != SessionMode::FileTransfer
                        && video_feedback.last_ghost.is_none_or(|last| now.duration_since(last) >= Duration::from_millis(75))
                        && hub.monitors_rx.borrow().0.iter().any(|display| display.id == cursor.display)
                    {
                        video_feedback.last_ghost = Some(now);
                        let _ = chat_tx.send((conn_id, CommsMsg::GhostCursor { viewer: conn_id, cursor }));
                    }
                }
                CommsMsg::AnnotationEvent(event) => {
                    if cfg!(target_os = "android") || *mode == SessionMode::FileTransfer
                        || !policy.permissions.send_message || !policy.os_prompts_enabled()
                    {
                        send_control_error(out_tx, "Desktop annotation is unavailable for this session.");
                        return Ok(());
                    }
                    let now = Instant::now();
                    if video_feedback.last_annotation.is_none_or(|last| now.duration_since(last) >= Duration::from_millis(10)) {
                        video_feedback.last_annotation = Some(now);
                        let _ = hub.to_agent_tx.try_send(IpcMsg::Annotation { viewer: conn_id, event });
                    }
                }
                CommsMsg::ChatMessage { .. } => {
                    if !policy.permissions.text_chat {
                        send_control_error(out_tx, "Text chat is disabled by host administrator.");
                        return Ok(());
                    }
                    let _ = chat_tx.send((conn_id, msg));
                }
                CommsMsg::PopupMessage {
                    title,
                    body,
                    urgent,
                } => {
                    if !policy.permissions.send_message {
                        let ack = CommsMsg::PopupAck {
                            shown: false,
                            message: "Popup messaging is disabled by host administrator.".into(),
                        };
                        if let Ok(f) = pack_message(ChannelId::Comms, FLAG_NONE, &ack, None) {
                            let _ = out_tx.send(f).await;
                        }
                        return Ok(());
                    }
                    log_audit_event(
                        policy.audit_log_path.as_deref(),
                        peer_str,
                        "popup_message",
                        &title,
                    );
                    let ack = if sas_enabled || !policy.os_prompts_enabled() {
                        let queued = hub.to_agent_tx.try_send(IpcMsg::ShowPopup {
                            title,
                            body,
                            urgent,
                        }).is_ok();
                        CommsMsg::PopupAck {
                            shown: queued,
                            message: if queued {
                                "Message queued for the remote desktop session.".into()
                            } else {
                                "The remote desktop could not receive the message.".into()
                            },
                        }
                    } else {
                        match tokio::task::spawn_blocking(move || show_desktop_popup(&title, &body, urgent))
                            .await
                        {
                            Ok(Ok(())) => CommsMsg::PopupAck {
                                shown: true,
                                message: "Message delivered to remote desktop session.".into(),
                            },
                            Ok(Err(e)) => CommsMsg::PopupAck {
                                shown: false,
                                message: e,
                            },
                            Err(e) => CommsMsg::PopupAck {
                                shown: false,
                                message: format!("Message delivery failed: {e}"),
                            },
                        }
                    };
                    if let Ok(f) = pack_message(ChannelId::Comms, FLAG_NONE, &ack, None) {
                        let _ = out_tx.send(f).await;
                    }
                }
                CommsMsg::SystemAudioOffer => {
                    if *mode == SessionMode::FileTransfer || !policy.permissions.system_audio {
                        send_control_error(out_tx, "The host has disabled system audio for this session.");
                        send_system_audio(out_tx, CommsMsg::SystemAudioStop).await;
                    } else if *system_audio == SystemAudioState::Idle {
                        if hub.to_agent_tx.try_send(IpcMsg::SystemAudioStart(conn_id)).is_ok() {
                            *system_audio = SystemAudioState::Pending;
                        } else {
                            send_control_error(out_tx, "Could not reach the host audio service.");
                            send_system_audio(out_tx, CommsMsg::SystemAudioStop).await;
                        }
                    }
                }
                CommsMsg::SystemAudioStop => {
                    if *system_audio != SystemAudioState::Idle {
                        *system_audio = SystemAudioState::Idle;
                        let _ = hub.to_agent_tx.send(IpcMsg::SystemAudioStop(conn_id)).await;
                    }
                }
                CommsMsg::VoiceOffer { sample_rate, channels } => {
                    if sample_rate != deskbolt_proto::VOICE_SAMPLE_RATE || channels != 1 {
                        send_control_error(out_tx, "The host requires 48 kHz mono voice.");
                        return Ok(());
                    }
                    if !policy.permissions.voice_chat {
                        send_control_error(out_tx, "Voice chat is disabled by host administrator.");
                        return Ok(());
                    }
                    #[cfg(any(test, all(feature = "dev-mcp", not(target_os = "android"))))]
                    if !policy.os_prompts_enabled() {
                        if let Ok(f) = pack_message(ChannelId::Comms, FLAG_NONE, &CommsMsg::VoiceAccept, None) {
                            let _ = out_tx.send(f).await;
                        }
                        return Ok(());
                    }
                    let controls = VoiceControls::default();
                    controls.bitrate_kbps.store(policy.voice_bitrate_kbps, Ordering::Relaxed);
                    match VoiceEngine::start(None, None, voice_tx.clone(), Arc::new(controls)) {
                        Ok(eng) => {
                            *voice_engine = Some(eng);
                            if let Ok(f) = pack_message(ChannelId::Comms, FLAG_NONE, &CommsMsg::VoiceAccept, None) {
                                let _ = out_tx.send(f).await;
                            }
                        }
                        Err(e) => {
                            send_control_error(out_tx, &format!("Host audio unavailable: {e}"));
                            if let Ok(f) = pack_message(ChannelId::Comms, FLAG_NONE, &CommsMsg::VoiceEnd, None) {
                                let _ = out_tx.send(f).await;
                            }
                        }
                    }
                }
                CommsMsg::VoiceFrame(bytes) => {
                    if policy.permissions.voice_chat
                        && let Some(eng) = voice_engine.as_ref()
                    {
                        eng.push_remote_frame(&bytes);
                    }
                }
                CommsMsg::VoiceEnd => {
                    *voice_engine = None;
                }
                _ => {}
            }
        }
        ChannelId::PowerControl => {
            let permitted = *mode == SessionMode::FullControl && policy.permissions.power_control;
            let denied = "Power control is disabled by host policy or requires Full Control.";
            let (action, force, delay_secs) = match unpack_message(&frame).map_err(|e| e.to_string())? {
                PowerMsg::PowerCommand { action, force, delay_secs } => (action, force, delay_secs),
                PowerMsg::PowerAck { .. } => return Ok(()),
                #[cfg(all(feature = "dev-mcp", not(target_os = "android")))]
                PowerMsg::SimulatePowerAction(action) => {
                    let result = if permitted && !policy.os_prompts_enabled() {
                        Ok(format!("Simulated power action: {action:?}"))
                    } else {
                        Err(denied.into())
                    };
                    send_power_ack(out_tx, result).await;
                    return Ok(());
                }
            };
            if !permitted || !policy.os_prompts_enabled() {
                send_power_ack(out_tx, Err(denied.into())).await;
                return Ok(());
            }
            log_audit_event(
                policy.audit_log_path.as_deref(),
                peer_str,
                "power_command",
                &format!("{action:?} force={force} delay={delay_secs}"),
            );
            let result = tokio::task::spawn_blocking(move || execute_power_action(action, force, delay_secs))
                .await
                .unwrap_or_else(|e| Err(format!("Could not run the power action: {e}")));
            send_power_ack(out_tx, result).await;
        }
        ChannelId::VideoTiles => {}
    }
    Ok(())
}

async fn send_power_ack(out_tx: &mpsc::Sender<Frame>, result: Result<String, String>) {
    let accepted = result.is_ok();
    let message = result.unwrap_or_else(|message| message);
    let ack = PowerMsg::PowerAck { accepted, message };
    if let Ok(f) = pack_message(ChannelId::PowerControl, FLAG_URGENT, &ack, None) {
        let _ = out_tx.send(f).await;
    }
}

async fn send_control(out: &mpsc::Sender<Frame>, message: ControlMsg) {
    if let Ok(frame) = pack_message(ChannelId::AuthControl, FLAG_URGENT, &message, None) {
        let _ = out.send(frame).await;
    }
}

async fn activate_control(
    mode: &mut SessionMode, hub: &AgentHub, id: u64, policy: &ServerPolicy, out: &mpsc::Sender<Frame>,
) {
    if policy.curtain_on_control && policy.os_prompts_enabled() {
        let _ = hub.to_agent_tx.send(IpcMsg::PolicyCurtain { viewer: id, enabled: true }).await;
    }
    *mode = SessionMode::FullControl;
    send_control(out, ControlMsg::SessionModeSwitch { mode: *mode }).await;
}

async fn relinquish_control(hub: &AgentHub, id: u64, policy: &ServerPolicy, feedback: &mut VideoFeedback) {
    let _ = hub.to_agent_tx.send(IpcMsg::Input(InputMsg::ReleaseAllKeys)).await;
    if policy.curtain_on_control && policy.os_prompts_enabled() {
        let _ = hub.to_agent_tx.send(IpcMsg::PolicyCurtain { viewer: id, enabled: false }).await;
    }
    if feedback.desktop_settings_used {
        let _ = hub.to_agent_tx.send(IpcMsg::ReleaseDesktopSettings(id)).await;
        feedback.desktop_settings_used = false;
    }
}

fn send_control_error(out_tx: &mpsc::Sender<Frame>, message: &str) {
    if let Ok(err) = pack_message(
        ChannelId::AuthControl,
        FLAG_URGENT,
        &ControlMsg::Error {
            message: message.to_string(),
        },
        None,
    ) {
        let _ = out_tx.try_send(err);
    }
}

#[cfg(test)]
mod tests {
    use deskbolt_proto::{
        MouseBtn, PointerEvent, TofuPinPolicy, TofuServerCertVerifier, compute_client_proof,
        connect_tls, derive_auth_key, generate_self_signed_cert, verify_server_proof,
    };
    use tokio::net::TcpStream;

    use super::*;

    #[test]
    fn congested_frame_acknowledgements_require_valid_sequences_and_recover_gradually() {
        let now = Instant::now();
        let mut feedback = VideoFeedback { last_frame: 30, rtt: Some((270, now)), ..VideoFeedback::default() };
        assert_eq!(feedback.acknowledge(1000, 1, now), None);
        assert_eq!(feedback.acknowledge(1, 1, now), Some(2));
        assert_eq!(feedback.acknowledge(1, 1, now), None);
        for step in 1..=8 {
            let at = now + Duration::from_millis(step * 250);
            feedback.rtt = Some((20, at));
            let level = feedback.acknowledge(step + 1, 10, at).unwrap();
            assert_eq!(level, 2 - (step / 4) as u8);
        }
        assert_eq!(feedback.acknowledge(10, 75, now + Duration::from_secs(3)), Some(1));
    }

    #[tokio::test]
    async fn unavailable_local_prompt_uses_the_configured_fallback() {
        let mut policy = ServerPolicy::for_test(true);
        policy.suppress_os_prompts = false;
        policy.confirm_connect = true;
        assert!(!approve_local_access(None, SessionMode::FullControl, &policy).await);
        policy.confirm_default_allow = true;
        assert!(approve_local_access(None, SessionMode::FullControl, &policy).await);
    }

    fn test_hub() -> (AgentHub, mpsc::Receiver<IpcMsg>) {
        let (to_agent_tx, to_agent_rx) = mpsc::channel(32);
        let (from_agent_tx, _) = broadcast::channel(16);
        let (_mon_tx, monitors_rx) = watch::channel((Vec::new(), 0));
        let (wake_tx, _wake_rx) = mpsc::unbounded_channel();
        (
            AgentHub {
                to_agent_tx,
                from_agent_tx,
                monitors_rx,
                viewer_wake_tx: wake_tx,
            },
            to_agent_rx,
        )
    }

    #[cfg(all(feature = "dev-mcp", not(target_os = "android")))]
    #[tokio::test]
    async fn regular_listener_rejects_power_simulation() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let id = generate_self_signed_cert(&[]).unwrap();
            let salt = [0x44; SALT_LEN];
            let password = b"LoopbackPower#1";
            let cred = Arc::new(StoredCredential {
                salt,
                m_kib: 1024,
                t_cost: 1,
                p_cost: 1,
                auth_key: derive_auth_key(password, &salt, 1024, 1, 1).unwrap(),
            });
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (hub, _agent_rx) = test_hub();
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let mut policy = ServerPolicy::for_test(false);
            policy.suppress_os_prompts = false;
            let server_task = tokio::spawn(serve_listener_with_policy(
                listener, id.cert_der, id.key_der, cred, hub, false, policy,
                Duration::from_secs(2), shutdown_rx,
            ));
            let (mut tls, mode) =
                test_auth_connect(addr, id.fingerprint, password, SessionMode::FullControl).await;
            assert_eq!(mode, SessionMode::FullControl);
            let frame = pack_message(
                ChannelId::PowerControl, FLAG_NONE,
                &PowerMsg::SimulatePowerAction(deskbolt_proto::PowerAction::Restart), None,
            ).unwrap();
            write_frame_async(&mut tls, frame.channel, frame.flags, &frame.payload).await.unwrap();
            loop {
                let reply = read_frame_async(&mut tls).await.unwrap().unwrap();
                if reply.channel == ChannelId::PowerControl {
                    assert!(matches!(
                        unpack_message::<PowerMsg>(&reply).unwrap(),
                        PowerMsg::PowerAck { accepted: false, .. }
                    ));
                    break;
                }
            }
            shutdown_tx.send(true).unwrap();
            server_task.await.unwrap().unwrap();
        }).await.unwrap();
    }

    #[tokio::test]
    async fn system_audio_requires_a_permitted_desktop_session() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let identity = generate_self_signed_cert(&[]).unwrap();
            let salt = [0x48; SALT_LEN];
            let password = b"LoopbackAudio#1";
            for (allow, mode) in [
                (false, SessionMode::ViewOnly),
                (true, SessionMode::FileTransfer),
                (true, SessionMode::ViewOnly),
            ] {
                let cred = Arc::new(StoredCredential {
                    salt,
                    m_kib: 1024,
                    t_cost: 1,
                    p_cost: 1,
                    auth_key: derive_auth_key(password, &salt, 1024, 1, 1).unwrap(),
                });
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let (hub, mut agent_commands) = test_hub();
                let agent_events = hub.from_agent_tx.clone();
                let (shutdown_tx, shutdown_rx) = watch::channel(false);
                let mut policy = ServerPolicy::for_test(false);
                policy.permissions.system_audio = allow;
                let server = tokio::spawn(serve_listener_with_policy(
                    listener, identity.cert_der.clone(), identity.key_der.clone(), cred,
                    hub, false, policy, Duration::from_secs(2), shutdown_rx,
                ));
                let (mut tls, granted) =
                    test_auth_connect(addr, identity.fingerprint, password, mode).await;
                assert_eq!(granted, mode);
                let offer = pack_message(ChannelId::Comms, FLAG_NONE, &CommsMsg::SystemAudioOffer, None).unwrap();
                write_frame_async(&mut tls, offer.channel, offer.flags, &offer.payload).await.unwrap();

                let permitted = allow && mode != SessionMode::FileTransfer;
                let packet = vec![0, 0, 0, 0, 0xf8, 0xff, 0xfe];
                if permitted {
                    let viewer = loop {
                        if let IpcMsg::SystemAudioStart(viewer) = agent_commands.recv().await.unwrap() {
                            break viewer;
                        }
                    };
                    agent_events.send(Arc::new(IpcMsg::SystemAudioReady { viewer, error: None })).unwrap();
                    agent_events.send(Arc::new(IpcMsg::SystemAudioFrame(packet.clone()))).unwrap();
                }
                loop {
                    let frame = read_frame_async(&mut tls).await.unwrap().unwrap();
                    if frame.channel != ChannelId::Comms {
                        continue;
                    }
                    match unpack_message::<CommsMsg>(&frame).unwrap() {
                        CommsMsg::SystemAudioStop if !permitted => {
                            while let Ok(command) = agent_commands.try_recv() {
                                assert!(!matches!(command, IpcMsg::SystemAudioStart(_)));
                            }
                            break;
                        }
                        CommsMsg::SystemAudioOffer if permitted => {}
                        CommsMsg::SystemAudioFrame(bytes) if permitted => {
                            assert_eq!(bytes, packet);
                            break;
                        }
                        other => panic!("unexpected desktop audio message {other:?}"),
                    }
                }
                drop(tls);
                if permitted {
                    while !matches!(agent_commands.recv().await.unwrap(), IpcMsg::SystemAudioStop(_)) {}
                }
                shutdown_tx.send(true).unwrap();
                server.await.unwrap().unwrap();
            }
        }).await.unwrap();
    }

    #[tokio::test]
    async fn loopback_auth_timeout_rate_limit_and_viewonly_isolation() {
        let id = generate_self_signed_cert(&[]).unwrap();
        let salt = [0x55u8; SALT_LEN];
        let auth_key = derive_auth_key(b"TopSecret!2026", &salt, 1024, 1, 1).unwrap();
        let cred = Arc::new(StoredCredential {
            salt,
            m_kib: 1024,
            t_cost: 1,
            p_cost: 1,
            auth_key,
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (hub, mut to_agent_rx) = test_hub();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let server_task = tokio::spawn(serve_listener(
            listener,
            id.cert_der.clone(),
            id.key_der.clone(),
            cred,
            hub,
            false,
            false,
            Duration::from_millis(300),
            shutdown_rx,
        ));

        // 1. Silent TCP connection must be dropped by preauth deadline
        let mut silent = TcpStream::connect(addr).await.unwrap();
        let mut b = [0u8; 1];
        let read_res = tokio::io::AsyncReadExt::read(&mut silent, &mut b).await.unwrap();
        assert_eq!(read_res, 0);

        // 2. ViewOnly session authenticates and CANNOT inject Input or SendCtrlAltDel
        let stream = TcpStream::connect(addr).await.unwrap();
        let verifier = Arc::new(TofuServerCertVerifier::new(TofuPinPolicy::ExpectPinned(
            id.fingerprint,
        )));
        let (mut tls, fp) = connect_tls(stream, "localhost", verifier).await.unwrap();
        assert_eq!(fp, id.fingerprint);

        let c_nonce = [0xAAu8; NONCE_LEN];
        let hello = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::ClientHello {
                proto_version: PROTO_VERSION,
                client_nonce: c_nonce,
                requested_mode: SessionMode::ViewOnly,
                client_name: "TestViewer".into(),
                video_decoders: VideoCapabilities::default(),
            },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls, hello.channel, hello.flags, &hello.payload)
            .await
            .unwrap();

        let ch_frame = read_frame_async(&mut tls).await.unwrap().unwrap();
        let ControlMsg::ServerChallenge {
            server_nonce,
            argon2_salt,
            argon2_m_kib,
            argon2_t,
            argon2_p,
        } = unpack_message::<ControlMsg>(&ch_frame).unwrap()
        else {
            panic!("expected ServerChallenge");
        };

        let key = derive_auth_key(b"TopSecret!2026", &argon2_salt, argon2_m_kib, argon2_t, argon2_p).unwrap();
        let proof = compute_client_proof(&key, &c_nonce, &server_nonce, &fp);
        let auth = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::ClientAuth { client_proof: proof },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls, auth.channel, auth.flags, &auth.payload)
            .await
            .unwrap();

        let res_frame = read_frame_async(&mut tls).await.unwrap().unwrap();
        let ControlMsg::AuthResult {
            accepted,
            server_proof,
            granted_mode,
            ..
        } = unpack_message::<ControlMsg>(&res_frame).unwrap()
        else {
            panic!("expected AuthResult");
        };
        assert!(accepted);
        assert_eq!(granted_mode, SessionMode::ViewOnly);
        assert!(verify_server_proof(&key, &server_nonce, &c_nonce, &fp, &server_proof));

        // Attempt unauthorized input injection in ViewOnly mode
        let bad_inp = pack_message(
            ChannelId::Input,
            FLAG_URGENT,
            &InputMsg::Pointer(PointerEvent::Button {
                button: MouseBtn::Left,
                down: true,
                norm_x: 0.5,
                norm_y: 0.5,
            }),
            None,
        )
        .unwrap();
        write_frame_async(&mut tls, bad_inp.channel, bad_inp.flags, &bad_inp.payload)
            .await
            .unwrap();

        let bad_sas = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::SpecialAction(SpecialAction::SendCtrlAltDel),
            None,
        )
        .unwrap();
        write_frame_async(&mut tls, bad_sas.channel, bad_sas.flags, &bad_sas.payload)
            .await
            .unwrap();

        let err_frame = read_frame_async(&mut tls).await.unwrap().unwrap();
        let err_msg: ControlMsg = unpack_message(&err_frame).unwrap();
        assert!(matches!(err_msg, ControlMsg::Error { .. }));

        // Verify agent received zero Input messages from ViewOnly session
        tokio::time::sleep(Duration::from_millis(50)).await;
        while let Ok(msg) = to_agent_rx.try_recv() {
            assert!(!matches!(msg, IpcMsg::Input(_)));
        }

        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn loopback_server_video_broadcast_and_file_roundtrip() {
        use deskbolt_proto::{
            DirtyTile, H264VideoEncoder, TileRect, TransferDir, VideoMsg, encode_dirty_tile,
        };
        use ring::digest::{Context as ShaCtx, SHA256};

        let id = generate_self_signed_cert(&[]).unwrap();
        let salt = [0x33u8; SALT_LEN];
        let auth_key = derive_auth_key(b"ServerPass#4", &salt, 1024, 1, 1).unwrap();
        let cred = Arc::new(StoredCredential {
            salt,
            m_kib: 1024,
            t_cost: 1,
            p_cost: 1,
            auth_key,
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (hub, mut to_agent_rx) = test_hub();
        let hub_tx = hub.from_agent_tx.clone();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let server_task = tokio::spawn(serve_listener(
            listener,
            id.cert_der.clone(),
            id.key_der.clone(),
            cred,
            hub,
            false,
            false,
            Duration::from_secs(2),
            shutdown_rx,
        ));

        let tcp = TcpStream::connect(addr).await.unwrap();
        let verifier = Arc::new(TofuServerCertVerifier::new(TofuPinPolicy::ExpectPinned(
            id.fingerprint,
        )));
        let (mut tls, _) = connect_tls(tcp, "localhost", verifier).await.unwrap();

        let c_nonce = [0x11u8; NONCE_LEN];
        let hello = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::ClientHello {
                proto_version: PROTO_VERSION,
                client_nonce: c_nonce,
                requested_mode: SessionMode::FullControl,
                client_name: "LoopbackTester".into(),
                video_decoders: VideoCapabilities::default(),
            },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls, hello.channel, hello.flags, &hello.payload)
            .await
            .unwrap();

        let ch = read_frame_async(&mut tls).await.unwrap().unwrap();
        let ControlMsg::ServerChallenge {
            server_nonce,
            argon2_salt,
            argon2_m_kib,
            argon2_t,
            argon2_p,
        } = unpack_message(&ch).unwrap()
        else {
            panic!("expected ServerChallenge");
        };
        let key = derive_auth_key(b"ServerPass#4", &argon2_salt, argon2_m_kib, argon2_t, argon2_p).unwrap();
        let auth = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::ClientAuth {
                client_proof: compute_client_proof(&key, &c_nonce, &server_nonce, &id.fingerprint),
            },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls, auth.channel, auth.flags, &auth.payload)
            .await
            .unwrap();

        let res = read_frame_async(&mut tls).await.unwrap().unwrap();
        let ControlMsg::AuthResult {
            accepted,
            granted_mode,
            server_proof,
            ..
        } = unpack_message(&res).unwrap()
        else {
            panic!("expected AuthResult");
        };
        assert!(accepted);
        assert_eq!(granted_mode, SessionMode::FullControl);
        assert!(verify_server_proof(
            &key,
            &server_nonce,
            &c_nonce,
            &id.fingerprint,
            &server_proof
        ));

        // Drain any wakeups triggered during setup
        while let Ok(msg) = to_agent_rx.try_recv() {
            if matches!(msg, IpcMsg::KeyframeRequest { .. }) {
                break;
            }
        }

        // 1. Zstd tile + OpenH264 broadcast delivery through serve_listener
        let bgra = vec![0x44u8; 32 * 32 * 4];
        let tile: DirtyTile = encode_dirty_tile(
            &bgra,
            32,
            32,
            TileRect {
                x: 0,
                y: 0,
                width: 32,
                height: 32,
            },
            3,
        )
        .unwrap();
        hub_tx
            .send(Arc::new(IpcMsg::Video(VideoMsg::TileBatch {
                monitor_id: 0,
                frame_seq: 1,
                timestamp_us: 0,
                width: 32,
                height: 32,
                is_full_refresh: true,
                move_rects: Vec::new(),
                tiles: vec![tile],
            })))
            .unwrap();
        let f1 = read_frame_async(&mut tls).await.unwrap().unwrap();
        assert!(matches!(
            unpack_message::<VideoMsg>(&f1).unwrap(),
            VideoMsg::TileBatch { frame_seq: 1, .. }
        ));

        let mut enc = H264VideoEncoder::new(32, 32, 1_000_000, 30.0).unwrap();
        enc.force_keyframe();
        let (annex_b, is_kf) = enc.encode_bgra(&bgra, 32, 32).unwrap();
        hub_tx
            .send(Arc::new(IpcMsg::Video(VideoMsg::H264Frame {
                monitor_id: 0,
                frame_seq: 2,
                timestamp_us: 1,
                width: 32,
                height: 32,
                is_keyframe: is_kf,
                annex_b,
            })))
            .unwrap();
        let f2 = read_frame_async(&mut tls).await.unwrap().unwrap();
        assert!(matches!(
            unpack_message::<VideoMsg>(&f2).unwrap(),
            VideoMsg::H264Frame { frame_seq: 2, .. }
        ));

        // 2. Chunked SHA-256 upload & download round-trip via FileTransfer channel
        let temp_dir = std::env::temp_dir().join(format!("deskbolt_net_ft_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();
        let remote_path = temp_dir.join("payload.TXT").to_string_lossy().into_owned();
        let payload: Vec<u8> = (0..140_000u32).map(|i| ((i * 7) % 251) as u8).collect();
        let mut expected_sha = [0u8; 32];
        let mut h = ShaCtx::new(&SHA256);
        h.update(&payload);
        expected_sha.copy_from_slice(h.finish().as_ref());

        let start = pack_message(
            ChannelId::FileTransfer,
            FLAG_NONE,
            &deskbolt_proto::FileTransferMsg::TransferStart {
                transfer_id: 1,
                direction: TransferDir::Upload,
                remote_path: remote_path.clone(),
                total_bytes: payload.len() as u64,
                modified_unix: 0,
                overwrite: true,
                resume_offset: 0,
                prefix_sha256: None,
            },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls, start.channel, start.flags, &start.payload)
            .await
            .unwrap();
        let ack_f = read_transfer_frame(&mut tls).await;
        assert!(matches!(
            unpack_message::<deskbolt_proto::FileTransferMsg>(&ack_f).unwrap(),
            deskbolt_proto::FileTransferMsg::TransferStartAck { accepted: true, .. }
        ));

        let mut offset = 0usize;
        for (idx, chunk) in payload.chunks(64 * 1024).enumerate() {
            if idx == 1 {
                use tokio::io::AsyncWriteExt as _;
                tls.shutdown().await.unwrap();
                while read_frame_async(&mut tls).await.is_ok_and(|frame| frame.is_some()) {}
                drop(tls);
                (tls, _) = test_auth_connect(addr, id.fingerprint, b"ServerPass#4", SessionMode::FileTransfer).await;
                write_frame_async(&mut tls, start.channel, start.flags, &start.payload).await.unwrap();
                let ack = read_transfer_frame(&mut tls).await;
                let expected = ring::digest::digest(&SHA256, &payload[..offset]);
                assert!(matches!(unpack_message::<FileTransferMsg>(&ack).unwrap(),
                    FileTransferMsg::TransferStartAck { accepted: true, resume_offset, prefix_sha256: Some(hash), .. }
                    if resume_offset == offset as u64 && hash == expected.as_ref()));
            }
            let seq = idx.max(1) as u32;
            let cf = deskbolt_proto::frame::pack_file_message(
                &deskbolt_proto::FileTransferMsg::FileChunk {
                    transfer_id: 1,
                    offset: offset as u64,
                    chunk_seq: seq,
                    data: chunk.to_vec(),
                },
                Some(std::path::Path::new(if idx == 1 { "payload.zip" } else { "payload.TXT" })),
            )
            .unwrap();
            assert_eq!(cf.flags & deskbolt_proto::FLAG_COMPRESSED != 0, idx != 1);
            write_frame_async(&mut tls, cf.channel, cf.flags, &cf.payload)
                .await
                .unwrap();
            let ca = read_transfer_frame(&mut tls).await;
            assert!(matches!(
                unpack_message::<deskbolt_proto::FileTransferMsg>(&ca).unwrap(),
                deskbolt_proto::FileTransferMsg::FileAck { acked_seq, .. } if acked_seq == seq
            ));
            offset += chunk.len();
        }
        let done = pack_message(
            ChannelId::FileTransfer,
            FLAG_NONE,
            &deskbolt_proto::FileTransferMsg::FileComplete {
                transfer_id: 1,
                total_bytes: payload.len() as u64,
                sha256: expected_sha,
            },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls, done.channel, done.flags, &done.payload)
            .await
            .unwrap();
        let vr = read_transfer_frame(&mut tls).await;
        assert!(matches!(
            unpack_message::<deskbolt_proto::FileTransferMsg>(&vr).unwrap(),
            deskbolt_proto::FileTransferMsg::FileVerifyResult { verified: true, .. }
        ));

        // Download same file back through serve_listener
        let resume = 64 * 1024;
        let prefix = ring::digest::digest(&SHA256, &payload[..resume]);
        let dl_start = pack_message(
            ChannelId::FileTransfer,
            FLAG_NONE,
            &deskbolt_proto::FileTransferMsg::TransferStart {
                transfer_id: 2,
                direction: TransferDir::Download,
                remote_path,
                total_bytes: payload.len() as u64,
                modified_unix: 0,
                overwrite: true,
                resume_offset: resume as u64,
                prefix_sha256: Some(prefix.as_ref().try_into().unwrap()),
            },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls, dl_start.channel, dl_start.flags, &dl_start.payload)
            .await
            .unwrap();
        let dl_ack = read_transfer_frame(&mut tls).await;
        assert!(matches!(
            unpack_message::<deskbolt_proto::FileTransferMsg>(&dl_ack).unwrap(),
            deskbolt_proto::FileTransferMsg::TransferStartAck { accepted: true, resume_offset, .. } if resume_offset == resume as u64
        ));

        let mut downloaded = payload[..resume].to_vec();
        loop {
            let f = read_transfer_frame(&mut tls).await;
            match unpack_message::<deskbolt_proto::FileTransferMsg>(&f).unwrap() {
                deskbolt_proto::FileTransferMsg::FileChunk { chunk_seq, data, .. } => {
                    assert_ne!(f.flags & deskbolt_proto::FLAG_COMPRESSED, 0);
                    downloaded.extend_from_slice(&data);
                    let ack = pack_message(
                        ChannelId::FileTransfer,
                        FLAG_NONE,
                        &deskbolt_proto::FileTransferMsg::FileAck {
                            transfer_id: 2,
                            acked_seq: chunk_seq,
                        },
                        None,
                    )
                    .unwrap();
                    write_frame_async(&mut tls, ack.channel, ack.flags, &ack.payload)
                        .await
                        .unwrap();
                }
                deskbolt_proto::FileTransferMsg::FileComplete { total_bytes, sha256, .. } => {
                    assert_eq!(total_bytes as usize, payload.len());
                    assert_eq!(sha256, expected_sha);
                    let vr = pack_message(
                        ChannelId::FileTransfer,
                        FLAG_NONE,
                        &deskbolt_proto::FileTransferMsg::FileVerifyResult {
                            transfer_id: 2,
                            verified: true,
                            error: None,
                        },
                        None,
                    )
                    .unwrap();
                    write_frame_async(&mut tls, vr.channel, vr.flags, &vr.payload)
                        .await
                        .unwrap();
                    break;
                }
                other => panic!("unexpected FileTransferMsg during download: {other:?}"),
            }
        }
        assert_eq!(downloaded, payload);

        // Page through a tree larger and deeper than the previous scan limits.
        for index in 0..15_001 {
            std::fs::create_dir(temp_dir.join(format!("dir-{index:05}"))).unwrap();
        }
        let deep = (0..40).fold(temp_dir.clone(), |dir, _| dir.join("d"));
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("included.txt"), b"deep").unwrap();
        std::fs::create_dir(temp_dir.join("node_modules")).unwrap();
        std::fs::write(temp_dir.join("node_modules/hidden.txt"), b"excluded").unwrap();
        std::fs::write(temp_dir.join("hidden.tmp"), b"excluded").unwrap();
        let mut cursor = None;
        let mut seen = std::collections::HashSet::new();
        let mut pages = 0;
        loop {
            let req = FileTransferMsg::HashTreeReq {
                root: temp_dir.to_string_lossy().into_owned(),
                cursor,
                page_size: 512,
                exclude_globs: deskbolt_proto::hash_tree::DEFAULT_EXCLUDE_GLOBS.lines().map(str::to_owned).collect(),
            };
            let frame = pack_message(ChannelId::FileTransfer, FLAG_NONE, &req, None).unwrap();
            write_frame_async(&mut tls, frame.channel, frame.flags, &frame.payload).await.unwrap();
            let frame = read_transfer_frame(&mut tls).await;
            let FileTransferMsg::HashTreeResp { entries, next_cursor, error: None, .. } =
                unpack_message(&frame).unwrap() else { panic!("invalid hash page"); };
            pages += 1;
            assert!(entries.len() <= 512);
            for entry in entries {
                assert!(!entry.rel_path.contains("hidden") && !entry.rel_path.starts_with("node_modules"));
                assert!(seen.insert(entry.rel_path));
            }
            cursor = next_cursor;
            if cursor.is_none() { break; }
        }
        assert!(pages > 1);
        assert_eq!(seen.len(), 15_043);
        assert!(seen.contains(&format!("{}included.txt", "d/".repeat(40))));

        let fallback = pack_message(ChannelId::FileTransfer, FLAG_NONE, &FileTransferMsg::TransferStart {
            transfer_id: 3,
            direction: TransferDir::Download,
            remote_path: temp_dir.join("payload.TXT").to_string_lossy().into_owned(),
            total_bytes: payload.len() as u64,
            modified_unix: 0,
            overwrite: false,
            resume_offset: resume as u64,
            prefix_sha256: Some([0; 32]),
        }, None).unwrap();
        write_frame_async(&mut tls, fallback.channel, fallback.flags, &fallback.payload).await.unwrap();
        let ack = read_transfer_frame(&mut tls).await;
        assert!(matches!(unpack_message::<FileTransferMsg>(&ack).unwrap(),
            FileTransferMsg::TransferStartAck { accepted: true, resume_offset: 0, prefix_sha256: None, .. }));
        let first = read_transfer_frame(&mut tls).await;
        assert!(matches!(unpack_message::<FileTransferMsg>(&first).unwrap(),
            FileTransferMsg::FileChunk { offset: 0, .. }));

        drop(tls);
        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    async fn read_transfer_frame(tls: &mut deskbolt_proto::ClientTlsStream<TcpStream>) -> Frame {
        loop {
            let frame = read_frame_async(tls).await.unwrap().unwrap();
            if frame.channel == ChannelId::FileTransfer {
                return frame;
            }
            assert_eq!(frame.channel, ChannelId::AuthControl);
            assert!(matches!(unpack_message(&frame).unwrap(), ControlMsg::Ping { .. }));
        }
    }

    async fn test_auth_connect(
        addr: SocketAddr,
        fp: [u8; 32],
        password: &[u8],
        mode: SessionMode,
    ) -> (deskbolt_proto::ClientTlsStream<TcpStream>, SessionMode) {
        let tcp = TcpStream::connect(addr).await.unwrap();
        let verifier = Arc::new(TofuServerCertVerifier::new(TofuPinPolicy::ExpectPinned(fp)));
        let (mut tls, _) = connect_tls(tcp, "localhost", verifier).await.unwrap();
        let c_nonce = [0x22u8; NONCE_LEN];
        let hello = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::ClientHello {
                proto_version: PROTO_VERSION,
                client_nonce: c_nonce,
                requested_mode: mode,
                client_name: "LockTester".into(),
                video_decoders: VideoCapabilities::default(),
            },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls, hello.channel, hello.flags, &hello.payload)
            .await
            .unwrap();
        let ch = read_frame_async(&mut tls).await.unwrap().unwrap();
        let ControlMsg::ServerChallenge {
            server_nonce,
            argon2_salt,
            argon2_m_kib,
            argon2_t,
            argon2_p,
        } = unpack_message(&ch).unwrap()
        else {
            panic!("expected ServerChallenge");
        };
        let key = derive_auth_key(password, &argon2_salt, argon2_m_kib, argon2_t, argon2_p).unwrap();
        let auth = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::ClientAuth {
                client_proof: compute_client_proof(&key, &c_nonce, &server_nonce, &fp),
            },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls, auth.channel, auth.flags, &auth.payload)
            .await
            .unwrap();
        let res = read_frame_async(&mut tls).await.unwrap().unwrap();
        let ControlMsg::AuthResult {
            accepted: true,
            granted_mode,
            ..
        } = unpack_message(&res).unwrap()
        else {
            panic!("expected accepted AuthResult");
        };
        (tls, granted_mode)
    }

    #[tokio::test]
    async fn single_controller_lock_hands_off_to_an_approved_viewer_and_releases_on_drop() {
        let id = generate_self_signed_cert(&[]).unwrap();
        let salt = [0x66u8; SALT_LEN];
        let auth_key = derive_auth_key(b"LockSecret!1", &salt, 1024, 1, 1).unwrap();
        let cred = Arc::new(StoredCredential {
            salt,
            m_kib: 1024,
            t_cost: 1,
            p_cost: 1,
            auth_key,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (mut hub, _to_agent_rx) = test_hub();
        let (monitor_updates, monitors_rx) = watch::channel((Vec::new(), 0));
        hub.monitors_rx = monitors_rx;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let server_task = tokio::spawn(serve_listener(
            listener,
            id.cert_der.clone(),
            id.key_der.clone(),
            cred,
            hub,
            false,
            true,
            Duration::from_secs(2),
            shutdown_rx,
        ));

        let (mut tls1, m1) = test_auth_connect(addr, id.fingerprint, b"LockSecret!1", SessionMode::FullControl).await;
        assert_eq!(m1, SessionMode::FullControl);

        let (mut tls2, m2) = test_auth_connect(addr, id.fingerprint, b"LockSecret!1", SessionMode::FullControl).await;
        assert_eq!(m2, SessionMode::ViewOnly);

        let sw = pack_message(
            ChannelId::AuthControl,
            FLAG_URGENT,
            &ControlMsg::SessionModeSwitch {
                mode: SessionMode::FullControl,
            },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls2, sw.channel, sw.flags, &sw.payload)
            .await
            .unwrap();
        let err_f = read_frame_async(&mut tls2).await.unwrap().unwrap();
        assert!(matches!(
            unpack_message::<ControlMsg>(&err_f).unwrap(),
            ControlMsg::Error { .. }
        ));

        async fn send(stream: &mut deskbolt_proto::ClientTlsStream<TcpStream>, message: ControlMsg) {
            let frame = pack_message(ChannelId::AuthControl, FLAG_URGENT, &message, None).unwrap();
            write_frame_async(stream, frame.channel, frame.flags, &frame.payload).await.unwrap();
        }
        async fn wait_for(
            stream: &mut deskbolt_proto::ClientTlsStream<TcpStream>,
            predicate: impl Fn(&ControlMsg) -> bool,
        ) -> ControlMsg {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let frame = read_frame_async(stream).await.unwrap().unwrap();
                    if frame.channel == ChannelId::AuthControl {
                        let msg = unpack_message::<ControlMsg>(&frame).unwrap();
                        if predicate(&msg) {
                            break msg;
                        }
                    }
                }
            }).await.unwrap()
        }
        send(&mut tls1, ControlMsg::WatchViewers).await;
        send(&mut tls2, ControlMsg::RequestControl).await;
        let prompt = wait_for(&mut tls1, |msg| matches!(msg, ControlMsg::ViewerPresenceList { request: Some(_), .. })).await;
        let ControlMsg::ViewerPresenceList { request: Some(requester), .. } = prompt else { unreachable!() };
        monitor_updates.send_replace((vec![deskbolt_proto::MonitorInfo {
            id: 0, name: "Display".into(), is_primary: true, left: 0, top: 0,
            width: 64, height: 64, dpi_scale: 1.0,
        }], 0));
        let cursor = deskbolt_proto::GhostCursor { display: 0, point: [42, 99] };
        let ghost = pack_message(ChannelId::Comms, FLAG_NONE, &CommsMsg::GhostCursor { viewer: 99, cursor }, None).unwrap();
        write_frame_async(&mut tls2, ghost.channel, ghost.flags, &ghost.payload).await.unwrap();
        let shared = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let frame = read_frame_async(&mut tls1).await.unwrap().unwrap();
                if frame.channel == ChannelId::Comms {
                    break unpack_message::<CommsMsg>(&frame).unwrap();
                }
            }
        }).await.unwrap();
        assert_eq!(shared, CommsMsg::GhostCursor { viewer: requester, cursor });
        send(&mut tls1, ControlMsg::HandOffControl { viewer: requester, approved: true }).await;
        wait_for(&mut tls1, |msg| matches!(msg, ControlMsg::SessionModeSwitch { mode: SessionMode::ViewOnly })).await;
        wait_for(&mut tls2, |msg| matches!(msg, ControlMsg::SessionModeSwitch { mode: SessionMode::FullControl })).await;
        drop(tls2);
        wait_for(&mut tls1, |msg| matches!(msg, ControlMsg::ViewerPresenceList { viewers, .. } if viewers.len() == 1)).await;
        let (_tls3, m3) = test_auth_connect(addr, id.fingerprint, b"LockSecret!1", SessionMode::FullControl).await;
        assert_eq!(m3, SessionMode::FullControl);

        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn admin_channels_enforced_by_session_mode_and_sysadmin_responds() {
        let id = generate_self_signed_cert(&[]).unwrap();
        let salt = [0x77u8; SALT_LEN];
        let auth_key = derive_auth_key(b"AdminPass!1", &salt, 1024, 1, 1).unwrap();
        let cred = Arc::new(StoredCredential {
            salt,
            m_kib: 1024,
            t_cost: 1,
            p_cost: 1,
            auth_key,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (hub, _to_agent_rx) = test_hub();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let server_task = tokio::spawn(serve_listener(
            listener,
            id.cert_der.clone(),
            id.key_der.clone(),
            cred,
            hub,
            false,
            false,
            Duration::from_secs(2),
            shutdown_rx,
        ));

        // 1. ViewOnly viewer cannot open Terminal/Tunnel/SysAdmin channels
        let (mut vo_tls, vo_mode) =
            test_auth_connect(addr, id.fingerprint, b"AdminPass!1", SessionMode::ViewOnly).await;
        assert_eq!(vo_mode, SessionMode::ViewOnly);
        let req = pack_message(
            ChannelId::SysAdmin,
            FLAG_NONE,
            &SysAdminMsg::SystemSnapshotReq,
            None,
        )
        .unwrap();
        write_frame_async(&mut vo_tls, req.channel, req.flags, &req.payload)
            .await
            .unwrap();
        let vo_reply = read_frame_async(&mut vo_tls).await.unwrap().unwrap();
        assert!(matches!(
            unpack_message::<ControlMsg>(&vo_reply).unwrap(),
            ControlMsg::Error { .. }
        ));
        drop(vo_tls);

        // 2. FullControl viewer receives SystemSnapshotResp on Channel 0x08
        let (mut fc_tls, fc_mode) =
            test_auth_connect(addr, id.fingerprint, b"AdminPass!1", SessionMode::FullControl).await;
        assert_eq!(fc_mode, SessionMode::FullControl);
        let req2 = pack_message(
            ChannelId::SysAdmin,
            FLAG_NONE,
            &SysAdminMsg::SystemSnapshotReq,
            None,
        )
        .unwrap();
        write_frame_async(&mut fc_tls, req2.channel, req2.flags, &req2.payload)
            .await
            .unwrap();
        let snap_frame = read_frame_async(&mut fc_tls).await.unwrap().unwrap();
        assert_eq!(snap_frame.channel, ChannelId::SysAdmin);
        let snap_msg: SysAdminMsg = unpack_message(&snap_frame).unwrap();
        assert!(matches!(
            snap_msg,
            SysAdminMsg::SystemSnapshotResp { error: None, .. }
        ));

        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn server_policy_enforces_cidr_perms_comms_and_power() {
        let id = generate_self_signed_cert(&[]).unwrap();
        let salt = [0x66u8; SALT_LEN];
        let auth_key = derive_auth_key(b"PolicyPass!8", &salt, 1024, 1, 1).unwrap();
        let cred = Arc::new(StoredCredential {
            salt,
            m_kib: 1024,
            t_cost: 1,
            p_cost: 1,
            auth_key,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (hub, _to_agent_rx) = test_hub();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        // Policy: allowlist 127.0.0.1/32, force_view_only = false, power_control = false, send_message = true
        let mut policy = ServerPolicy::for_test(false);
        policy.ip_allowlist = vec!["127.0.0.1/32".into()];
        policy.permissions.power_control = false;
        policy.permissions.text_chat = true;
        policy.permissions.send_message = true;

        let server_task = tokio::spawn(serve_listener_with_policy(
            listener,
            id.cert_der.clone(),
            id.key_der.clone(),
            cred,
            hub,
            false,
            policy,
            Duration::from_secs(2),
            shutdown_rx,
        ));

        let (mut tls_a, mode_a) =
            test_auth_connect(addr, id.fingerprint, b"PolicyPass!8", SessionMode::FullControl).await;
        assert_eq!(mode_a, SessionMode::FullControl);

        let (mut tls_b, _) =
            test_auth_connect(addr, id.fingerprint, b"PolicyPass!8", SessionMode::ViewOnly).await;

        // 1. PowerCommand blocked when permissions.power_control == false
        let p_cmd = pack_message(
            ChannelId::PowerControl,
            FLAG_NONE,
            &PowerMsg::PowerCommand {
                action: deskbolt_proto::PowerAction::Restart,
                force: false,
                delay_secs: 5,
            },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls_a, p_cmd.channel, p_cmd.flags, &p_cmd.payload)
            .await
            .unwrap();
        let pack_reply = read_frame_async(&mut tls_a).await.unwrap().unwrap();
        assert_eq!(pack_reply.channel, ChannelId::PowerControl);
        assert!(matches!(
            unpack_message::<PowerMsg>(&pack_reply).unwrap(),
            PowerMsg::PowerAck { accepted: false, .. }
        ));

        // 2. PopupMessage acknowledged on Channel 0x09
        let pop = pack_message(
            ChannelId::Comms,
            FLAG_NONE,
            &CommsMsg::PopupMessage {
                title: "Maintenance".into(),
                body: "Rebooting soon".into(),
                urgent: true,
            },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls_a, pop.channel, pop.flags, &pop.payload)
            .await
            .unwrap();
        let pop_reply = read_frame_async(&mut tls_a).await.unwrap().unwrap();
        assert!(matches!(
            unpack_message::<CommsMsg>(&pop_reply).unwrap(),
            CommsMsg::PopupAck { shown: true, .. }
        ));

        // 3. ChatMessage relayed from session A -> session B
        let chat = pack_message(
            ChannelId::Comms,
            FLAG_NONE,
            &CommsMsg::ChatMessage {
                sender: "Ops".into(),
                timestamp: 100,
                text: "Hello desktop".into(),
            },
            None,
        )
        .unwrap();
        write_frame_async(&mut tls_a, chat.channel, chat.flags, &chat.payload)
            .await
            .unwrap();
        let chat_b = read_frame_async(&mut tls_b).await.unwrap().unwrap();
        assert_eq!(
            unpack_message::<CommsMsg>(&chat_b).unwrap(),
            CommsMsg::ChatMessage {
                sender: "Ops".into(),
                timestamp: 100,
                text: "Hello desktop".into(),
            }
        );

        drop(tls_a);
        drop(tls_b);
        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
    }
}
