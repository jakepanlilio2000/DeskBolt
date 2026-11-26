use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use deskbolt_proto::{
    ChannelId, ClientTlsStream, ClipboardSync, CommsMsg, ControlMsg, FLAG_NONE, FileTransferMsg,
    InputMsg, IpcMsg, PROTO_VERSION, PointerEvent, PowerAction, PowerMsg, SessionMode,
    StoredCredential, SysAdminMsg, TerminalMsg, TofuPinPolicy, TofuServerCertVerifier, TransferDir,
    TunnelMsg, VideoMsg, compute_client_proof, connect_tls, derive_auth_key,
    generate_self_signed_cert, pack_message, random_bytes, read_frame_async, unpack_message,
    verify_server_proof, write_frame_async,
};
use ring::digest::{SHA256, digest};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{ErrorData, JsonObject},
    serde::{self, Deserialize, Serialize, de::DeserializeOwned},
    serde_json, tool, tool_router,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, watch};

use crate::{config::ServerPolicy, ipc::AgentHub, net::serve_listener_with_policy};

type Check<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
pub struct ProtocolChecks;

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(crate = "self::serde", deny_unknown_fields)]
struct NoArgs {}

fn empty_input_schema() -> Arc<JsonObject> {
    let schema = schemars::schema_for!(NoArgs);
    Arc::new(
        serde_json::to_value(schema.schema)
            .unwrap()
            .as_object()
            .unwrap()
            .clone(),
    )
}

#[tool_router(server_handler)]
impl ProtocolChecks {
    #[tool(
        description = "Check TLS 1.3 pinning, password authentication, video, input and clipboard on a disposable loopback host.",
        input_schema = empty_input_schema()
    )]
    async fn test_auth_handshake(&self, _: Parameters<NoArgs>) -> Result<String, ErrorData> {
        bounded(
            check_auth_and_desktop(),
            "TLS, authentication and desktop channels passed",
        )
        .await
    }

    #[tool(
        description = "Open and resize a local PTY, then check fixed command output and exit status.",
        input_schema = empty_input_schema()
    )]
    async fn test_pty_roundtrip(&self, _: Parameters<NoArgs>) -> Result<String, ErrorData> {
        bounded(check_pty(), "PTY output and exit status passed").await
    }

    #[tool(
        description = "Check file hashes, verified upload and appended log data in a temporary directory.",
        input_schema = empty_input_schema()
    )]
    async fn test_file_sync_roundtrip(&self, _: Parameters<NoArgs>) -> Result<String, ErrorData> {
        bounded(
            check_files(),
            "File hashes, verified upload and log tail passed",
        )
        .await
    }

    #[tool(
        description = "Round-trip bytes through a loopback TCP tunnel and check a system snapshot.",
        input_schema = empty_input_schema()
    )]
    async fn test_tunnel_and_sysinfo(&self, _: Parameters<NoArgs>) -> Result<String, ErrorData> {
        bounded(
            check_tunnel_and_system(),
            "TCP tunnel and system snapshot passed",
        )
        .await
    }

    #[tool(
        description = "Check chat, popup framing and a simulated power action without changing the host power state.",
        input_schema = empty_input_schema()
    )]
    async fn test_comms_and_power_sim(&self, _: Parameters<NoArgs>) -> Result<String, ErrorData> {
        bounded(
            check_comms_and_power(),
            "Chat, popup and simulated power action passed",
        )
        .await
    }
}

async fn bounded(check: impl Future<Output = Check>, success: &str) -> Result<String, ErrorData> {
    match tokio::time::timeout(Duration::from_secs(25), check).await {
        Ok(Ok(())) => Ok(success.into()),
        Ok(Err(error)) => Err(ErrorData::internal_error(error.to_string(), None)),
        Err(_) => Err(ErrorData::internal_error("Loopback check timed out.", None)),
    }
}

fn require(condition: bool, message: &'static str) -> Check {
    condition.then_some(()).ok_or_else(|| message.into())
}

struct Host {
    addr: SocketAddr,
    pin: [u8; 32],
    password: [u8; 32],
    to_agent: mpsc::Receiver<IpcMsg>,
    from_agent: broadcast::Sender<Arc<IpcMsg>>,
    shutdown: watch::Sender<bool>,
}

impl Host {
    async fn start() -> Check<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let identity = generate_self_signed_cert(&[])?;
        let password = random_bytes()?;
        let credential = Arc::new(StoredCredential::from_password(&password)?);
        let (to_agent_tx, to_agent) = mpsc::channel(32);
        let (from_agent, _) = broadcast::channel(16);
        let (_, monitors_rx) = watch::channel((Vec::new(), 0));
        let (viewer_wake_tx, _) = mpsc::unbounded_channel();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let hub = AgentHub {
            to_agent_tx,
            from_agent_tx: from_agent.clone(),
            monitors_rx,
            viewer_wake_tx,
        };
        let mut policy = ServerPolicy::for_test(false);
        policy.ip_allowlist = vec!["127.0.0.1/32".into()];
        tokio::spawn(serve_listener_with_policy(
            listener,
            identity.cert_der,
            identity.key_der,
            credential,
            hub,
            false,
            policy,
            Duration::from_secs(10),
            shutdown_rx,
        ));
        Ok(Self {
            addr,
            pin: identity.fingerprint,
            password,
            to_agent,
            from_agent,
            shutdown,
        })
    }

    async fn tls(
        &self,
        verifier: Arc<TofuServerCertVerifier>,
    ) -> Check<(ClientTlsStream<TcpStream>, [u8; 32])> {
        let tcp = TcpStream::connect(self.addr).await?;
        tcp.set_nodelay(true)?;
        Ok(connect_tls(tcp, "127.0.0.1", verifier).await?)
    }

    async fn connect(&self) -> Check<Client> {
        let (mut client, accepted) = self.authenticate(&self.password).await?;
        require(accepted, "Host refused the fixture password.")?;
        client
            .send(
                ChannelId::AuthControl,
                &ControlMsg::Ping { timestamp_ms: 42 },
            )
            .await?;
        loop {
            if let ControlMsg::Pong { timestamp_ms: 42 } =
                client.recv(ChannelId::AuthControl).await?
            {
                return Ok(client);
            }
        }
    }

    async fn authenticate(&self, password: &[u8]) -> Check<(Client, bool)> {
        let verifier = Arc::new(TofuServerCertVerifier::new(TofuPinPolicy::ExpectPinned(
            self.pin,
        )));
        let (tls, pin) = self.tls(verifier).await?;
        let mut client = Client(tls);
        let nonce = random_bytes()?;
        use ChannelId::AuthControl as Auth;
        client
            .send(
                Auth,
                &ControlMsg::ClientHello {
                    proto_version: PROTO_VERSION,
                    client_nonce: nonce,
                    requested_mode: SessionMode::FullControl,
                    client_name: "Loopback protocol check".into(),
                },
            )
            .await?;
        let ControlMsg::ServerChallenge {
            server_nonce,
            argon2_salt,
            argon2_m_kib,
            argon2_t,
            argon2_p,
        } = client.recv(Auth).await?
        else {
            return Err("Expected a host authentication challenge.".into());
        };
        let key = derive_auth_key(password, &argon2_salt, argon2_m_kib, argon2_t, argon2_p)?;
        let proof = compute_client_proof(&key, &nonce, &server_nonce, &pin);
        client
            .send(
                Auth,
                &ControlMsg::ClientAuth {
                    client_proof: proof,
                },
            )
            .await?;
        let ControlMsg::AuthResult {
            accepted,
            server_proof,
            granted_mode,
            ..
        } = client.recv(Auth).await?
        else {
            return Err("Expected an authentication result.".into());
        };
        if accepted {
            require(
                granted_mode == SessionMode::FullControl
                    && verify_server_proof(&key, &server_nonce, &nonce, &pin, &server_proof),
                "The host proof or granted session mode did not match.",
            )?;
        }
        Ok((client, accepted))
    }

    async fn agent_received(&mut self, expected: IpcMsg) -> Check {
        while let Some(message) = self.to_agent.recv().await {
            if message == expected {
                return Ok(());
            }
        }
        Err("The host agent channel closed.".into())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

struct Client(ClientTlsStream<TcpStream>);

impl Client {
    async fn send(&mut self, channel: ChannelId, message: &impl Serialize) -> Check {
        let frame = pack_message(channel, FLAG_NONE, message, None)?;
        write_frame_async(&mut self.0, frame.channel, frame.flags, &frame.payload).await?;
        Ok(())
    }

    async fn recv<T: DeserializeOwned>(&mut self, channel: ChannelId) -> Check<T> {
        while let Some(frame) = read_frame_async(&mut self.0).await? {
            if frame.channel == ChannelId::AuthControl
                && let ControlMsg::Error { message } = unpack_message::<ControlMsg>(&frame)?
            {
                return Err(message.into());
            }
            if frame.channel == channel {
                return Ok(unpack_message(&frame)?);
            }
        }
        Err(format!("Host closed the connection while reading channel {channel:?}.").into())
    }
}

async fn check_auth_and_desktop() -> Check {
    let mut host = Host::start().await?;
    let (probe, observed) = host
        .tls(Arc::new(TofuServerCertVerifier::new(
            TofuPinPolicy::ProbeFirstUse,
        )))
        .await?;
    require(
        probe.get_ref().1.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3)
            && observed == host.pin,
        "TLS version or first-use certificate fingerprint did not match.",
    )?;
    drop(probe);
    let mut wrong_pin = observed;
    wrong_pin[0] ^= 1;
    let verifier = Arc::new(TofuServerCertVerifier::new(TofuPinPolicy::ExpectPinned(
        wrong_pin,
    )));
    require(
        host.tls(verifier.clone()).await.is_err()
            && verifier.observed_fingerprint() == Some(observed),
        "TLS accepted a different certificate fingerprint.",
    )?;
    let (_, accepted) = host.authenticate(b"incorrect fixture password").await?;
    require(!accepted, "Host accepted an incorrect password.")?;
    let mut client = host.connect().await?;

    let video = VideoMsg::CursorUpdate {
        visible: true,
        x: 10,
        y: 20,
        hotspot_x: 0,
        hotspot_y: 0,
        width: 0,
        height: 0,
        zstd_rgba_shape: None,
    };
    host.from_agent
        .send(Arc::new(IpcMsg::Video(video.clone())))?;
    require(
        client.recv::<VideoMsg>(ChannelId::VideoTiles).await? == video,
        "Video frame changed in transit.",
    )?;

    let input = InputMsg::Pointer(PointerEvent::MoveAbs {
        norm_x: 0.25,
        norm_y: 0.75,
    });
    client.send(ChannelId::Input, &input).await?;
    host.agent_received(IpcMsg::Input(input)).await?;

    let clipboard = ClipboardSync::SetText {
        seq: 1,
        utf8_text: "DeskBolt loopback clipboard".into(),
    };
    client.send(ChannelId::Clipboard, &clipboard).await?;
    host.agent_received(IpcMsg::Clipboard(clipboard.clone()))
        .await?;
    host.from_agent
        .send(Arc::new(IpcMsg::Clipboard(clipboard.clone())))?;
    require(
        client.recv::<ClipboardSync>(ChannelId::Clipboard).await? == clipboard,
        "Clipboard payload changed in transit.",
    )
}

async fn check_pty() -> Check {
    let host = Host::start().await?;
    let mut client = host.connect().await?;
    use ChannelId::Terminal as Channel;
    client
        .send(
            Channel,
            &TerminalMsg::OpenPty {
                cols: 80,
                rows: 24,
                shell_hint: Some(
                    if cfg!(target_os = "windows") {
                        "cmd.exe"
                    } else {
                        "/bin/sh"
                    }
                    .into(),
                ),
            },
        )
        .await?;
    loop {
        match client.recv(Channel).await? {
            TerminalMsg::PtyOpened { ok: true, .. } => break,
            TerminalMsg::PtyData(_) => {}
            message => return Err(format!("Could not open the PTY: {message:?}").into()),
        }
    }
    client
        .send(
            Channel,
            &TerminalMsg::PtyResize {
                cols: 100,
                rows: 32,
            },
        )
        .await?;
    let echo = if cfg!(target_os = "windows") {
        b"set \"DB_MARK=DESKBOLT_PTY_\"\r\necho %DB_MARK%ROUNDTRIP\r\n".as_slice()
    } else {
        b"printf '%s%s\\n' DESKBOLT_PTY_ ROUNDTRIP\n"
    };
    client
        .send(Channel, &TerminalMsg::PtyData(echo.to_vec()))
        .await?;
    let mut output = Vec::new();
    loop {
        match client.recv(Channel).await? {
            TerminalMsg::PtyData(bytes) => {
                output.extend(bytes);
                if output
                    .windows(b"DESKBOLT_PTY_ROUNDTRIP".len())
                    .any(|window| window == b"DESKBOLT_PTY_ROUNDTRIP")
                {
                    break;
                }
                require(
                    output.len() <= 64 * 1024,
                    "PTY output exceeded the check limit.",
                )?;
            }
            message => {
                return Err(format!(
                    "PTY exited before producing the expected output: {message:?}"
                )
                .into());
            }
        }
    }
    client
        .send(Channel, &TerminalMsg::PtyData(b"exit 7\r\n".to_vec()))
        .await?;
    loop {
        if let TerminalMsg::PtyExit(code) = client.recv(Channel).await? {
            return require(code == 7, "PTY returned an unexpected exit code.");
        }
    }
}

async fn hash_tree(client: &mut Client, root: &Path) -> Check<Vec<deskbolt_proto::SyncEntry>> {
    use ChannelId::FileTransfer as Channel;
    client
        .send(
            Channel,
            &FileTransferMsg::HashTreeReq {
                path: root.to_string_lossy().into_owned(),
            },
        )
        .await?;
    match client.recv(Channel).await? {
        FileTransferMsg::HashTreeResp {
            entries,
            error: None,
            ..
        } => Ok(entries),
        message => {
            Err(format!("Could not read the SHA-256 directory manifest: {message:?}").into())
        }
    }
}

async fn check_files() -> Check {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("loopback.log");
    let host = Host::start().await?;
    let mut client = host.connect().await?;
    let old = b"old entry\n";
    let data = b"first entry\n";
    let second = b"second entry\n";
    let old_hash = digest(&SHA256, old);
    let sha256: [u8; 32] = digest(&SHA256, data).as_ref().try_into()?;
    tokio::fs::write(&path, old).await?;
    let before = hash_tree(&mut client, directory.path()).await?;
    require(
        matches!(before.as_slice(), [entry] if entry.rel_path == "loopback.log"
            && entry.sha256.as_ref().is_some_and(|hash| hash == old_hash.as_ref())),
        "The initial directory hash did not match.",
    )?;
    let remote_path = path.to_string_lossy().into_owned();
    use ChannelId::FileTransfer as Channel;
    client
        .send(
            Channel,
            &FileTransferMsg::TransferStart {
                transfer_id: 1,
                direction: TransferDir::Upload,
                remote_path: remote_path.clone(),
                total_bytes: data.len() as u64,
                modified_unix: 0,
                overwrite: true,
            },
        )
        .await?;
    require(
        matches!(
            client.recv(Channel).await?,
            FileTransferMsg::TransferStartAck {
                transfer_id: 1,
                accepted: true,
                ..
            }
        ),
        "The host refused the upload.",
    )?;
    for (seq, chunk) in data.chunks(6).enumerate() {
        let chunk_seq = seq as u32;
        client
            .send(
                Channel,
                &FileTransferMsg::FileChunk {
                    transfer_id: 1,
                    offset: (seq * 6) as u64,
                    chunk_seq,
                    data: chunk.to_vec(),
                },
            )
            .await?;
        require(
            matches!(client.recv(Channel).await?, FileTransferMsg::FileAck { transfer_id: 1, acked_seq } if acked_seq == chunk_seq),
            "The host did not acknowledge the upload chunk.",
        )?;
    }
    client
        .send(
            Channel,
            &FileTransferMsg::FileComplete {
                transfer_id: 1,
                total_bytes: data.len() as u64,
                sha256,
            },
        )
        .await?;
    require(
        matches!(
            client.recv(Channel).await?,
            FileTransferMsg::FileVerifyResult {
                transfer_id: 1,
                verified: true,
                ..
            }
        ) && tokio::fs::read(&path).await? == data,
        "The uploaded file did not pass SHA-256 verification.",
    )?;
    let after = hash_tree(&mut client, directory.path()).await?;
    require(
        matches!(after.as_slice(), [entry] if entry.rel_path == "loopback.log"
            && entry.sha256 == Some(sha256) && entry.sha256 != before[0].sha256),
        "The directory diff did not identify the changed file hash.",
    )?;
    let mut offset = 0;
    for expected in [data.as_slice(), second.as_slice()] {
        client
            .send(
                Channel,
                &FileTransferMsg::TailChunkReq {
                    path: remote_path.clone(),
                    offset,
                    max_bytes: 256,
                },
            )
            .await?;
        let FileTransferMsg::TailChunkResp {
            data: bytes,
            next_offset,
            truncated: false,
            error: None,
            ..
        } = client.recv(Channel).await?
        else {
            return Err("The host did not return a log chunk.".into());
        };
        require(
            bytes == expected && next_offset == offset + expected.len() as u64,
            "Log tail bytes or offset did not match.",
        )?;
        if offset == 0 {
            let mut log = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .await?;
            log.write_all(second).await?;
            log.flush().await?;
        }
        offset = next_offset;
    }
    Ok(())
}

async fn check_tunnel_and_system() -> Check {
    let host = Host::start().await?;
    let mut client = host.connect().await?;
    let echo = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = echo.local_addr()?.port();
    const DATA: &[u8] = b"DeskBolt loopback tunnel";
    let echo_once = async {
        let (mut socket, _) = echo.accept().await?;
        let mut bytes = [0; DATA.len()];
        socket.read_exact(&mut bytes).await?;
        socket.write_all(&bytes).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };
    let forward = async {
        use ChannelId::Tunnel as Channel;
        client
            .send(
                Channel,
                &TunnelMsg::TunnelOpen {
                    tunnel_id: 1,
                    target_host: Ipv4Addr::LOCALHOST.to_string(),
                    target_port: port,
                },
            )
            .await?;
        require(
            matches!(
                client.recv(Channel).await?,
                TunnelMsg::TunnelOpenAck {
                    tunnel_id: 1,
                    ok: true,
                    ..
                }
            ),
            "The host could not open the loopback tunnel.",
        )?;
        client
            .send(
                Channel,
                &TunnelMsg::TunnelData {
                    tunnel_id: 1,
                    bytes: DATA.to_vec(),
                },
            )
            .await?;
        let mut received = Vec::new();
        while received.len() < DATA.len() {
            let TunnelMsg::TunnelData {
                tunnel_id: 1,
                bytes,
            } = client.recv(Channel).await?
            else {
                return Err("The tunnel closed before returning the payload.".into());
            };
            received.extend(bytes);
        }
        require(received == DATA, "The tunneled payload changed in transit.")
    };
    tokio::try_join!(echo_once, forward)?;
    client
        .send(ChannelId::SysAdmin, &SysAdminMsg::SystemSnapshotReq)
        .await?;
    require(
        matches!(client.recv(ChannelId::SysAdmin).await?, SysAdminMsg::SystemSnapshotResp { snapshot, error: None }
            if !snapshot.hostname.is_empty() && snapshot.total_mem_bytes > 0 && !snapshot.processes.is_empty()),
        "The host did not return a system and process snapshot.",
    )
}

async fn check_comms_and_power() -> Check {
    let mut host = Host::start().await?;
    let mut sender = host.connect().await?;
    let mut receiver = host.connect().await?;
    use ChannelId::{Comms, PowerControl};
    let chat = CommsMsg::ChatMessage {
        sender: "Loopback".into(),
        timestamp: 1,
        text: "DeskBolt chat".into(),
    };
    sender.send(Comms, &chat).await?;
    require(
        receiver.recv::<CommsMsg>(Comms).await? == chat,
        "Chat payload changed in transit.",
    )?;

    let title = "Loopback popup".to_owned();
    let body = "DeskBolt popup".to_owned();
    sender
        .send(
            Comms,
            &CommsMsg::PopupMessage {
                title: title.clone(),
                body: body.clone(),
                urgent: false,
            },
        )
        .await?;
    require(
        matches!(
            sender.recv(Comms).await?,
            CommsMsg::PopupAck { shown: true, .. }
        ),
        "The host did not acknowledge the popup.",
    )?;
    host.agent_received(IpcMsg::ShowPopup {
        title,
        body,
        urgent: false,
    })
    .await?;

    sender
        .send(
            PowerControl,
            &PowerMsg::SimulatePowerAction(PowerAction::Restart),
        )
        .await?;
    require(
        matches!(sender.recv(PowerControl).await?, PowerMsg::PowerAck { accepted: true, message }
            if message == "Simulated power action: Restart"),
        "The host did not acknowledge the simulated restart.",
    )
}
