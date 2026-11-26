use std::fmt::Write as _;
use std::sync::{Arc, Mutex, Once};

use rcgen::{CertifiedKey, generate_simple_self_signed};
use ring::digest::{SHA256, digest};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, PeerIncompatible, ServerConfig, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{TlsAcceptor, TlsConnector, client, server};

use crate::error::ProtoError;

pub const PROTO_VERSION: u16 = 2;
pub const DEFAULT_PORT: u16 = 4899;

pub type TlsServerConfig = ServerConfig;
pub type ServerTlsStream<S> = server::TlsStream<S>;
pub type ClientTlsStream<S> = client::TlsStream<S>;

static RING_INIT: Once = Once::new();

/// Ensures `ring` is installed as the process-wide `rustls` crypto provider.
pub fn ensure_ring_provider() {
    RING_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Generated or loaded X.509 identity materials.
#[derive(Debug, Clone)]
pub struct GeneratedIdentity {
    pub cert_pem: String,
    pub key_pem: String,
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub fingerprint: [u8; 32],
}

/// Generates a persistent ECDSA P-256 self-signed X.509 identity certificate.
pub fn generate_self_signed_cert(extra_sans: &[String]) -> Result<GeneratedIdentity, ProtoError> {
    let mut sans = vec!["localhost".to_string(), "127.0.0.1".to_string(), "deskbolt.local".to_string()];
    for san in extra_sans {
        if !san.trim().is_empty() && !sans.iter().any(|existing| existing.eq_ignore_ascii_case(san)) {
            sans.push(san.clone());
        }
    }

    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(sans).map_err(|e| ProtoError::Tls(format!("rcgen failed: {e}")))?;

    let cert_der = cert.der().to_vec();
    let key_der = signing_key.serialize_der();
    let fingerprint = sha256_fingerprint(&cert_der);

    Ok(GeneratedIdentity {
        cert_pem: cert.pem(),
        key_pem: signing_key.serialize_pem(),
        cert_der,
        key_der,
        fingerprint,
    })
}

/// Parses PEM-encoded certificate and private key bytes into DER + fingerprint.
pub fn load_identity_from_pem_bytes(
    cert_pem: &str,
    key_pem: &str,
) -> Result<GeneratedIdentity, ProtoError> {
    let cert = CertificateDer::from_pem_slice(cert_pem.as_bytes())
        .map_err(|e| ProtoError::Tls(format!("invalid cert PEM: {e}")))?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| ProtoError::Tls(format!("invalid key PEM: {e}")))?;
    let cert_der = cert.as_ref().to_vec();
    let key_der = key.secret_der().to_vec();
    let fingerprint = sha256_fingerprint(&cert_der);

    Ok(GeneratedIdentity {
        cert_pem: cert_pem.to_string(),
        key_pem: key_pem.to_string(),
        cert_der,
        key_der,
        fingerprint,
    })
}

/// Computes the SHA-256 digest of DER-encoded leaf certificate bytes.
pub fn sha256_fingerprint(cert_der: &[u8]) -> [u8; 32] {
    let d = digest(&SHA256, cert_der);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// Formats a 32-byte fingerprint as uppercase colon-separated hex (`AA:BB:...:FF`).
pub fn format_fingerprint(fp: &[u8; 32]) -> String {
    let mut out = String::with_capacity(95);
    for (idx, byte) in fp.iter().enumerate() {
        if idx > 0 {
            out.push(':');
        }
        let _ = write!(&mut out, "{byte:02X}");
    }
    out
}

/// Parses a colon-separated or raw 64-char hex SHA-256 fingerprint string.
pub fn parse_fingerprint(input: &str) -> Result<[u8; 32], ProtoError> {
    let hex_only: String = input.chars().filter(|c| *c != ':' && !c.is_whitespace()).collect();
    if hex_only.len() != 64 {
        return Err(ProtoError::InvalidInput(format!(
            "fingerprint must contain 64 hex characters, got {}",
            hex_only.len()
        )));
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let pair = &hex_only[i * 2..i * 2 + 2];
        *slot = u8::from_str_radix(pair, 16)
            .map_err(|_| ProtoError::InvalidInput(format!("invalid hex byte '{pair}'")))?;
    }
    Ok(out)
}

/// Builds a strict TLS 1.3 `rustls::ServerConfig` using `ring`.
pub fn build_server_config(cert_der: Vec<u8>, key_der: Vec<u8>) -> Result<Arc<ServerConfig>, ProtoError> {
    ensure_ring_provider();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = CertificateDer::from(cert_der);
    let key = PrivateKeyDer::try_from(key_der)
        .map_err(|e| ProtoError::Tls(format!("invalid private key DER: {e}")))?;

    let config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| ProtoError::Tls(format!("TLS 1.3 config failed: {e}")))?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| ProtoError::Tls(format!("server cert load failed: {e}")))?;

    Ok(Arc::new(config))
}

/// TOFU certificate verification policy for the Viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TofuPinPolicy {
    /// Captures and proves key ownership on first contact so the UI can prompt before authenticating.
    ProbeFirstUse,
    /// Requires the server's leaf certificate SHA-256 fingerprint to match exactly.
    ExpectPinned([u8; 32]),
}

/// Custom `rustls` verifier enforcing SHA-256 TOFU fingerprint pinning + TLS 1.3 key proof.
#[derive(Debug)]
pub struct TofuServerCertVerifier {
    policy: TofuPinPolicy,
    observed: Mutex<Option<[u8; 32]>>,
    supported: WebPkiSupportedAlgorithms,
}

impl TofuServerCertVerifier {
    pub fn new(policy: TofuPinPolicy) -> Self {
        ensure_ring_provider();
        Self {
            policy,
            observed: Mutex::new(None),
            supported: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
    }

    pub fn observed_fingerprint(&self) -> Option<[u8; 32]> {
        *self.observed.lock().expect("observed fingerprint mutex poisoned")
    }
}

impl ServerCertVerifier for TofuServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let observed = sha256_fingerprint(end_entity.as_ref());
        *self.observed.lock().expect("observed fingerprint mutex poisoned") = Some(observed);

        match self.policy {
            TofuPinPolicy::ProbeFirstUse => Ok(ServerCertVerified::assertion()),
            TofuPinPolicy::ExpectPinned(expected) => {
                if observed == expected {
                    Ok(ServerCertVerified::assertion())
                } else {
                    Err(TlsError::General(format!(
                        "pinned certificate mismatch: expected {}, observed {}",
                        format_fingerprint(&expected),
                        format_fingerprint(&observed)
                    )))
                }
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        // TLS 1.3 only in DeskBolt; still delegate signature validation defensively if ever negotiated.
        let _ = (message, cert, dss);
        let _ = verify_tls12_signature;
        Err(TlsError::PeerIncompatible(PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

/// Builds a strict TLS 1.3 `rustls::ClientConfig` wired to `TofuServerCertVerifier`.
pub fn build_client_config(verifier: Arc<TofuServerCertVerifier>) -> Result<Arc<ClientConfig>, ProtoError> {
    ensure_ring_provider();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| ProtoError::Tls(format!("TLS 1.3 client config failed: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();

    Ok(Arc::new(config))
}

/// Accepts a TLS 1.3 connection using a pre-built server config.
pub async fn accept_tls<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    config: Arc<ServerConfig>,
) -> Result<server::TlsStream<S>, ProtoError> {
    TlsAcceptor::from(config)
        .accept(stream)
        .await
        .map_err(|e| ProtoError::Tls(format!("TLS accept failed: {e}")))
}

/// Establishes a TLS 1.3 connection using `TofuServerCertVerifier` and returns the stream + SHA-256 fingerprint.
pub async fn connect_tls<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    server_host: &str,
    verifier: Arc<TofuServerCertVerifier>,
) -> Result<(client::TlsStream<S>, [u8; 32]), ProtoError> {
    let config = build_client_config(verifier.clone())?;
    let server_name = ServerName::try_from(server_host.to_string())
        .or_else(|_| ServerName::try_from("deskbolt.local".to_string()))
        .map_err(|e| ProtoError::Tls(format!("invalid server name: {e}")))?;
    let tls = TlsConnector::from(config)
        .connect(server_name, stream)
        .await
        .map_err(|e| ProtoError::Tls(format!("TLS connect failed: {e}")))?;
    let fp = verifier
        .observed_fingerprint()
        .ok_or_else(|| ProtoError::Tls("server certificate missing".into()))?;
    Ok((tls, fp))
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;

    #[test]
    fn fingerprint_format_and_parse_roundtrip() {
        let id = generate_self_signed_cert(&["10.0.0.5".into()]).unwrap();
        let text = format_fingerprint(&id.fingerprint);
        assert_eq!(text.len(), 95);
        assert_eq!(parse_fingerprint(&text).unwrap(), id.fingerprint);

        let no_colons: String = text.chars().filter(|c| *c != ':').collect();
        assert_eq!(parse_fingerprint(&no_colons.to_lowercase()).unwrap(), id.fingerprint);
        assert!(parse_fingerprint("ABCD").is_err());

        let reloaded = load_identity_from_pem_bytes(&id.cert_pem, &id.key_pem).unwrap();
        assert_eq!(reloaded.fingerprint, id.fingerprint);
    }

    #[tokio::test]
    async fn tls13_tofu_pinning_accept_probe_and_mismatch() {
        let id = generate_self_signed_cert(&[]).unwrap();
        let server_cfg = build_server_config(id.cert_der.clone(), id.key_der.clone()).unwrap();

        // 1. Matching pinned fingerprint succeeds
        let verifier = Arc::new(TofuServerCertVerifier::new(TofuPinPolicy::ExpectPinned(
            id.fingerprint,
        )));

        let (c_stream, s_stream) = duplex(8192);
        let cfg1 = server_cfg.clone();
        let server_task = tokio::spawn(async move {
            let mut tls = accept_tls(s_stream, cfg1).await.unwrap();
            let mut buf = [0u8; 4];
            tls.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping");
        });
        let (mut client_tls, fp) = connect_tls(c_stream, "localhost", verifier).await.unwrap();
        assert_eq!(fp, id.fingerprint);
        client_tls.write_all(b"ping").await.unwrap();
        client_tls.shutdown().await.unwrap();
        server_task.await.unwrap();

        // 2. Mismatched fingerprint aborts handshake
        let bad_verifier = Arc::new(TofuServerCertVerifier::new(TofuPinPolicy::ExpectPinned([0xFF; 32])));
        let cfg2 = server_cfg.clone();
        let (c2, s2) = duplex(8192);
        let s2_task = tokio::spawn(async move {
            let _ = accept_tls(s2, cfg2).await;
        });
        let res = connect_tls(c2, "localhost", bad_verifier.clone()).await;
        assert!(res.is_err());
        assert_eq!(bad_verifier.observed_fingerprint(), Some(id.fingerprint));
        let _ = s2_task.await;

        // 3. Probe mode captures observed fingerprint after TLS 1.3 signature verification
        let probe = Arc::new(TofuServerCertVerifier::new(TofuPinPolicy::ProbeFirstUse));
        let (c3, s3) = duplex(8192);
        let s3_task = tokio::spawn(async move {
            let _ = accept_tls(s3, server_cfg).await;
        });
        let (_tls3, probe_fp) = connect_tls(c3, "localhost", probe).await.unwrap();
        assert_eq!(probe_fp, id.fingerprint);
        let _ = s3_task.await;
    }
}
