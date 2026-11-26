use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OobAction {
    TurnOn,
    ColdRestart,
    ColdTurnOff,
    StartupRemoteControl,
    NetworkBoot,
    BiosRemoteControl,
}

impl OobAction {
    pub const ALL_POWER_AND_BOOT: [Self; 5] = [
        Self::TurnOn,
        Self::ColdRestart,
        Self::ColdTurnOff,
        Self::StartupRemoteControl,
        Self::NetworkBoot,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::TurnOn => "Turn On",
            Self::ColdRestart => "Cold Restart",
            Self::ColdTurnOff => "Cold Turn Off",
            Self::StartupRemoteControl => "Startup Remote Control (Boot Menu / BIOS)",
            Self::NetworkBoot => "Network Boot (PXE)",
            Self::BiosRemoteControl => "BIOS Remote Control (Serial-over-LAN)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OobTarget {
    pub host: String,
    pub port: u16,
    pub tls_port: u16,
    pub use_tls: bool,
    pub username: String,
    pub password: String,
}

impl OobTarget {
    pub fn effective_port(&self) -> u16 {
        if self.use_tls && self.tls_port > 0 {
            self.tls_port
        } else if self.port > 0 {
            self.port
        } else {
            16992
        }
    }
}

/// Builds a standards-compliant WS-Management SOAP XML envelope for CIM power and boot control.
pub fn build_wsman_envelope(action: OobAction, message_uuid: &str) -> Result<String, String> {
    let body = match action {
        OobAction::TurnOn => cim_power_body(2),
        OobAction::ColdRestart => cim_power_body(5),
        OobAction::ColdTurnOff => cim_power_body(8),
        OobAction::StartupRemoteControl => cim_boot_source_body("FirmwareSetup"),
        OobAction::NetworkBoot => cim_boot_source_body("PXE"),
        OobAction::BiosRemoteControl => {
            return Err("BIOS Remote Control uses Serial-over-LAN stream redirection.".into());
        }
    };

    let (wsa_action, resource_uri) = match action {
        OobAction::TurnOn | OobAction::ColdRestart | OobAction::ColdTurnOff => (
            "http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService/RequestPowerStateChange",
            "http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService",
        ),
        OobAction::StartupRemoteControl | OobAction::NetworkBoot => (
            "http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_BootService/SetBootConfigRole",
            "http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_BootService",
        ),
        OobAction::BiosRemoteControl => unreachable!(),
    };

    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" \
         xmlns:a=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" \
         xmlns:w=\"http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd\">\
         <s:Header>\
           <a:To>http://localhost/wsman</a:To>\
           <a:ReplyTo><a:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:Address></a:ReplyTo>\
           <w:ResourceURI>{resource_uri}</w:ResourceURI>\
           <a:Action>{wsa_action}</a:Action>\
           <w:MaxEnvelopeSize>153600</w:MaxEnvelopeSize>\
           <a:MessageID>uuid:{message_uuid}</a:MessageID>\
           <w:OperationTimeout>PT30.000S</w:OperationTimeout>\
         </s:Header>\
         <s:Body>{body}</s:Body>\
         </s:Envelope>"
    ))
}

fn cim_power_body(power_state: u16) -> String {
    format!(
        "<p:RequestPowerStateChange_INPUT xmlns:p=\"http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_PowerManagementService\" xmlns:a=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" xmlns:w=\"http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd\">\
         <p:PowerState>{power_state}</p:PowerState>\
         <p:ManagedElement>\
           <a:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:Address>\
           <a:ReferenceParameters>\
             <w:ResourceURI>http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_ComputerSystem</w:ResourceURI>\
             <w:SelectorSet><w:Selector Name=\"Name\">ManagedSystem</w:Selector></w:SelectorSet>\
           </a:ReferenceParameters>\
         </p:ManagedElement>\
         </p:RequestPowerStateChange_INPUT>"
    )
}

fn cim_boot_source_body(boot_target: &str) -> String {
    format!(
        "<b:SetBootConfigRole_INPUT xmlns:b=\"http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_BootService\" xmlns:a=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" xmlns:w=\"http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd\">\
         <b:BootConfigSetting>\
           <a:Address>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:Address>\
           <a:ReferenceParameters>\
             <w:ResourceURI>http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/CIM_BootConfigSetting</w:ResourceURI>\
             <w:SelectorSet><w:Selector Name=\"InstanceID\">{boot_target}</w:Selector></w:SelectorSet>\
           </a:ReferenceParameters>\
         </b:BootConfigSetting>\
         <b:Role>1</b:Role>\
         </b:SetBootConfigRole_INPUT>"
    )
}

/// Executes an Out-of-Band hardware action over WS-Management (HTTP or TLS + Digest Auth).
/// For `StartupRemoteControl` and `NetworkBoot`, applies the one-time boot override then issues `ColdRestart`.
pub async fn execute_oob_action(target: &OobTarget, action: OobAction) -> Result<String, String> {
    if target.host.trim().is_empty() || target.username.trim().is_empty() {
        return Err("Out-of-Band host address and username are required.".into());
    }
    match action {
        OobAction::StartupRemoteControl | OobAction::NetworkBoot => {
            let env1 = build_wsman_envelope(action, "deskbolt-oob-boot-1")?;
            let resp1 = send_wsman_post(target, &env1).await?;
            let env2 = build_wsman_envelope(OobAction::ColdRestart, "deskbolt-oob-boot-2")?;
            let resp2 = send_wsman_post(target, &env2).await?;
            Ok(format!(
                "{} configured ({resp1}); Cold Restart dispatched ({resp2}).",
                action.label()
            ))
        }
        _ => {
            let env = build_wsman_envelope(action, "deskbolt-oob-cmd-1")?;
            let resp = send_wsman_post(target, &env).await?;
            Ok(format!("{}: {resp}", action.label()))
        }
    }
}

async fn send_wsman_post(target: &OobTarget, soap_xml: &str) -> Result<String, String> {
    let port = target.effective_port();
    let host_hdr = format!("{}:{port}", target.host.trim());

    // Step 1: Unauthenticated probe to retrieve WWW-Authenticate Digest challenge
    let (status1, headers1, body1) =
        http_post_once(target, port, &host_hdr, "/wsman", None, soap_xml).await?;
    if (200..300).contains(&status1) {
        return Ok(extract_wsman_return(&body1).unwrap_or_else(|| format!("HTTP {status1} OK")));
    }
    if status1 != 401 {
        return Err(format!("Out-of-Band HTTP {status1}: {body1}"));
    }

    let www_auth = headers1
        .get("www-authenticate")
        .ok_or_else(|| "Out-of-Band server returned 401 without WWW-Authenticate header.".to_string())?;
    let params = parse_digest_challenge(www_auth)?;
    let auth_header = build_digest_auth_header(
        &target.username,
        &target.password,
        "POST",
        "/wsman",
        &params,
        "00000001",
        "deskbolt1234",
    )?;

    // Step 2: Authenticated Digest POST
    let (status2, _headers2, body2) =
        http_post_once(target, port, &host_hdr, "/wsman", Some(&auth_header), soap_xml).await?;
    if (200..300).contains(&status2) {
        Ok(extract_wsman_return(&body2).unwrap_or_else(|| format!("HTTP {status2} OK")))
    } else {
        Err(format!("Out-of-Band Digest rejected (HTTP {status2}): {body2}"))
    }
}

fn extract_wsman_return(xml: &str) -> Option<String> {
    if let Some(s) = xml.find("<ReturnValue>").or_else(|| xml.find(":ReturnValue>")) {
        let rest = &xml[s..];
        if let Some(gt) = rest.find('>')
            && let Some(end) = rest[gt + 1..].find('<')
        {
            let code = rest[gt + 1..gt + 1 + end].trim();
            return Some(format!("ReturnValue={code}"));
        }
    }
    None
}

async fn http_post_once(
    target: &OobTarget,
    port: u16,
    host_hdr: &str,
    path: &str,
    auth_hdr: Option<&str>,
    body: &str,
) -> Result<(u16, HashMap<String, String>, String), String> {
    let mut req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_hdr}\r\n\
         Content-Type: application/soap+xml;charset=UTF-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(auth) = auth_hdr {
        let _ = write!(&mut req, "Authorization: {auth}\r\n");
    }
    req.push_str("\r\n");
    req.push_str(body);

    let tcp = tokio::time::timeout(
        Duration::from_secs(8),
        TcpStream::connect((target.host.trim(), port)),
    )
    .await
    .map_err(|_| format!("Out-of-Band connect {host_hdr} timed out"))?
    .map_err(|e| format!("Out-of-Band connect {host_hdr} failed: {e}"))?;

    if target.use_tls {
        deskbolt_proto::ensure_ring_provider();
        let cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(OobCertVerifier))
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
        let server_name = ServerName::try_from(target.host.trim().to_string())
            .map_err(|e| format!("invalid OOB hostname: {e}"))?;
        let mut tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| format!("Out-of-Band TLS handshake failed: {e}"))?;
        exchange_http(&mut tls, req.as_bytes()).await
    } else {
        let mut s = tcp;
        exchange_http(&mut s, req.as_bytes()).await
    }
}

async fn exchange_http<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    req_bytes: &[u8],
) -> Result<(u16, HashMap<String, String>, String), String> {
    stream
        .write_all(req_bytes)
        .await
        .map_err(|e| format!("HTTP write failed: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("HTTP flush failed: {e}"))?;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if raw.len() > 256 * 1024 {
            break;
        }
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        if rem.is_zero() {
            break;
        }
        match tokio::time::timeout(rem, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => raw.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) => return Err(format!("HTTP read failed: {e}")),
            Err(_) => break,
        }
    }

    let text = String::from_utf8_lossy(&raw).into_owned();
    let (hdr_part, body_part) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = hdr_part.lines();
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    Ok((status, headers, body_part.to_string()))
}

#[derive(Debug)]
struct OobCertVerifier;

impl ServerCertVerifier for OobCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Out-of-band BMC interfaces frequently ship factory self-signed certificates on management LANs.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::ED25519,
        ]
    }
}

/// Parses an HTTP `WWW-Authenticate: Digest ...` challenge header.
pub fn parse_digest_challenge(header: &str) -> Result<HashMap<String, String>, String> {
    let rest = header
        .trim()
        .strip_prefix("Digest")
        .or_else(|| header.trim().strip_prefix("digest"))
        .ok_or_else(|| format!("Expected Digest auth challenge, got '{header}'"))?
        .trim();

    let mut map = HashMap::new();
    let mut i = 0usize;
    let bytes = rest.as_bytes();
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let k_start = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key = rest[k_start..i].trim().to_ascii_lowercase();
        i += 1; // skip '='
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        let val = if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let v_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let s = &rest[v_start..i];
            if i < bytes.len() {
                i += 1;
            }
            s.to_string()
        } else {
            let v_start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            rest[v_start..i].trim().to_string()
        };
        map.insert(key, val);
    }
    if !map.contains_key("nonce") {
        return Err("Digest challenge missing nonce".into());
    }
    Ok(map)
}

/// Computes an HTTP Digest `Authorization` header supporting MD5 and SHA-256 (`qop=auth` or legacy).
pub fn build_digest_auth_header(
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    params: &HashMap<String, String>,
    nc: &str,
    cnonce: &str,
) -> Result<String, String> {
    let realm = params.get("realm").map(String::as_str).unwrap_or("");
    let nonce = params.get("nonce").map(String::as_str).unwrap_or("");
    let qop = params
        .get("qop")
        .and_then(|q| q.split(',').map(str::trim).find(|&m| m == "auth"));
    let alg = params
        .get("algorithm")
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_else(|| "MD5".to_string());

    let hash_hex = |data: &str| -> String {
        if alg.contains("SHA-256") {
            let d = ring::digest::digest(&ring::digest::SHA256, data.as_bytes());
            hex_lower(d.as_ref())
        } else {
            hex_lower(&md5_digest(data.as_bytes()))
        }
    };

    let ha1 = hash_hex(&format!("{username}:{realm}:{password}"));
    let ha2 = hash_hex(&format!("{method}:{uri}"));
    let response = if let Some(q) = qop {
        hash_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{q}:{ha2}"))
    } else {
        hash_hex(&format!("{ha1}:{nonce}:{ha2}"))
    };

    let mut out = format!(
        "Digest username=\"{username}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\", algorithm={alg}"
    );
    if let Some(q) = qop {
        let _ = write!(&mut out, ", qop={q}, nc={nc}, cnonce=\"{cnonce}\"");
    }
    if let Some(op) = params.get("opaque") {
        let _ = write!(&mut out, ", opaque=\"{op}\"");
    }
    Ok(out)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// Pure-Rust RFC 1321 MD5 digest strictly for HTTP Digest hardware BMC challenge compatibility.
pub fn md5_digest(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20,
        5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for chunk in msg.as_chunks::<64>().0 {
        let mut m = [0u32; 16];
        for (idx, slot) in m.iter_mut().enumerate() {
            let off = idx * 4;
            *slot = u32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | ((!b) & d), i),
                16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | (!d)), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            let sum = a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(S[i]));
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

/// Connects to an Out-of-Band Serial-over-LAN TCP/TLS stream and bridges bytes into a `DirectTerminalTab`.
pub fn spawn_oob_sol_terminal(
    target: OobTarget,
    rt: &tokio::runtime::Handle,
    repaint: impl Fn() + Send + Sync + 'static,
) -> (
    tokio::sync::mpsc::Sender<crate::terminal::DirectTerminalCmd>,
    std::sync::mpsc::Receiver<crate::terminal::DirectTerminalEvent>,
) {
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(128);
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    rt.spawn(async move {
        let _ = ev_tx.send(crate::terminal::DirectTerminalEvent::Connecting);
        repaint();
        let port = if target.use_tls && target.tls_port > 0 {
            target.tls_port
        } else if target.port == 16992 {
            16994
        } else {
            target.port
        };
        let mut stream = match tokio::time::timeout(
            Duration::from_secs(8),
            TcpStream::connect((target.host.trim(), port)),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                let _ = ev_tx.send(crate::terminal::DirectTerminalEvent::Exited(format!(
                    "Serial-over-LAN connect {}:{port} failed: {e}",
                    target.host
                )));
                repaint();
                return;
            }
            Err(_) => {
                let _ = ev_tx.send(crate::terminal::DirectTerminalEvent::Exited(format!(
                    "Serial-over-LAN connect {}:{port} timed out",
                    target.host
                )));
                repaint();
                return;
            }
        };

        let _ = ev_tx.send(crate::terminal::DirectTerminalEvent::Ready(format!(
            "BIOS Console (SOL {}:{port})",
            target.host
        )));
        repaint();

        let (mut rd, mut wr) = stream.split();
        let mut buf = [0u8; 4096];
        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    match cmd {
                        crate::terminal::DirectTerminalCmd::Write(b) => {
                            if wr.write_all(&b).await.is_err() {
                                break;
                            }
                        }
                        crate::terminal::DirectTerminalCmd::Close => break,
                        crate::terminal::DirectTerminalCmd::Resize { .. } => {}
                    }
                }
                n_res = rd.read(&mut buf) => {
                    match n_res {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let _ = ev_tx.send(crate::terminal::DirectTerminalEvent::Data(buf[..n].to_vec()));
                            repaint();
                        }
                    }
                }
            }
        }
        let _ = ev_tx.send(crate::terminal::DirectTerminalEvent::Exited(
            "BIOS Serial-over-LAN console closed.".into(),
        ));
        repaint();
    });
    (cmd_tx, ev_rx)
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn wsman_envelopes_and_md5_digest_match_specs() {
        // RFC 1321 empty + "abc" vector
        assert_eq!(hex_lower(&md5_digest(b"abc")), "900150983cd24fb0d6963f7d28e17f72");

        let on = build_wsman_envelope(OobAction::TurnOn, "u1").unwrap();
        assert!(on.contains("CIM_PowerManagementService/RequestPowerStateChange"));
        assert!(on.contains("<p:PowerState>2</p:PowerState>"));

        let restart = build_wsman_envelope(OobAction::ColdRestart, "u2").unwrap();
        assert!(restart.contains("<p:PowerState>5</p:PowerState>"));

        let off = build_wsman_envelope(OobAction::ColdTurnOff, "u3").unwrap();
        assert!(off.contains("<p:PowerState>8</p:PowerState>"));

        let pxe = build_wsman_envelope(OobAction::NetworkBoot, "u4").unwrap();
        assert!(pxe.contains("CIM_BootService/SetBootConfigRole"));
        assert!(pxe.contains("<w:Selector Name=\"InstanceID\">PXE</w:Selector>"));

        let setup = build_wsman_envelope(OobAction::StartupRemoteControl, "u5").unwrap();
        assert!(setup.contains("<w:Selector Name=\"InstanceID\">FirmwareSetup</w:Selector>"));

        let chal = parse_digest_challenge(
            "Digest realm=\"oob@host\", nonce=\"dcd98b71\", qop=\"auth\", algorithm=MD5",
        )
        .unwrap();
        let hdr = build_digest_auth_header("admin", "Secret", "POST", "/wsman", &chal, "00000001", "cn1").unwrap();
        assert!(hdr.starts_with("Digest username=\"admin\""));
        assert!(hdr.contains("qop=auth"));
    }

    #[tokio::test]
    async fn execute_oob_action_completes_digest_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let srv = tokio::spawn(async move {
            for idx in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                if idx == 0 {
                    assert!(!req.contains("Authorization:"));
                    let reply = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"oob\", nonce=\"112233\", qop=\"auth\"\r\nContent-Length: 0\r\n\r\n";
                    sock.write_all(reply.as_bytes()).await.unwrap();
                } else {
                    assert!(req.contains("Authorization: Digest"));
                    let xml = "<s:Envelope><s:Body><ReturnValue>0</ReturnValue></s:Body></s:Envelope>";
                    let reply = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{xml}", xml.len());
                    sock.write_all(reply.as_bytes()).await.unwrap();
                }
            }
        });

        let target = OobTarget {
            host: "127.0.0.1".into(),
            port,
            tls_port: 0,
            use_tls: false,
            username: "admin".into(),
            password: "Pass".into(),
        };
        let res = execute_oob_action(&target, OobAction::ColdRestart).await.unwrap();
        assert!(res.contains("ReturnValue=0"));
        let _ = srv.await;
    }
}
