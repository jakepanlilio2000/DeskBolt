use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use argon2::{Algorithm, Argon2, Params, Version};
use ring::hmac::{HMAC_SHA256, Key, sign, verify};
use ring::rand::{SecureRandom, SystemRandom};

use crate::error::ProtoError;

pub const ARGON2_M_KIB: u32 = 65_536;
pub const ARGON2_T_COST: u32 = 3;
pub const ARGON2_P_COST: u32 = 1;
pub const SALT_LEN: usize = 16;
pub const NONCE_LEN: usize = 32;

const CLIENT_DOMAIN: &[u8] = b"DESKBOLT-CLIENT-AUTH-V1";
const SERVER_DOMAIN: &[u8] = b"DESKBOLT-SERVER-AUTH-V1";
const RATE_LIMIT_MAX_IPS: usize = 4096;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(600);

/// Fills a fixed-size buffer with cryptographic random bytes via `ring::rand::SystemRandom`.
pub fn random_bytes<const N: usize>() -> Result<[u8; N], ProtoError> {
    let rng = SystemRandom::new();
    let mut buf = [0u8; N];
    rng.fill(&mut buf)
        .map_err(|_| ProtoError::Auth("system RNG failure".into()))?;
    Ok(buf)
}

/// Derives a 32-byte authentication key from a password and salt via Argon2id.
pub fn derive_auth_key(
    password: &[u8],
    salt: &[u8; SALT_LEN],
    m_kib: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<[u8; 32], ProtoError> {
    let params = Params::new(m_kib, t_cost, p_cost, Some(32))
        .map_err(|e| ProtoError::Auth(format!("invalid Argon2id params: {e}")))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    argon2
        .hash_password_into(password, salt, &mut key)
        .map_err(|e| ProtoError::Auth(format!("Argon2id failed: {e}")))?;
    Ok(key)
}

/// Persisted server credential material encoded as `$deskbolt-v1$m=...,t=...,p=...$<salt_hex>$<key_hex>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCredential {
    pub salt: [u8; SALT_LEN],
    pub m_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
    pub auth_key: [u8; 32],
}

impl StoredCredential {
    pub fn from_password(password: &[u8]) -> Result<Self, ProtoError> {
        let salt = random_bytes::<SALT_LEN>()?;
        let auth_key = derive_auth_key(password, &salt, ARGON2_M_KIB, ARGON2_T_COST, ARGON2_P_COST)?;
        Ok(Self {
            salt,
            m_kib: ARGON2_M_KIB,
            t_cost: ARGON2_T_COST,
            p_cost: ARGON2_P_COST,
            auth_key,
        })
    }

    pub fn encode_string(&self) -> String {
        let mut out = format!("$deskbolt-v1$m={},t={},p={}$", self.m_kib, self.t_cost, self.p_cost);
        for b in self.salt {
            let _ = write!(&mut out, "{b:02x}");
        }
        out.push('$');
        for b in self.auth_key {
            let _ = write!(&mut out, "{b:02x}");
        }
        out
    }

    pub fn decode_string(s: &str) -> Result<Self, ProtoError> {
        let parts: Vec<&str> = s.trim().split('$').collect();
        if parts.len() != 5 || !parts[0].is_empty() || parts[1] != "deskbolt-v1" {
            return Err(ProtoError::Auth("invalid stored credential format".into()));
        }
        let mut m_kib = None;
        let mut t_cost = None;
        let mut p_cost = None;
        for kv in parts[2].split(',') {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| ProtoError::Auth("invalid credential param".into()))?;
            let val: u32 = v
                .parse()
                .map_err(|_| ProtoError::Auth("non-numeric credential param".into()))?;
            match k {
                "m" => m_kib = Some(val),
                "t" => t_cost = Some(val),
                "p" => p_cost = Some(val),
                _ => return Err(ProtoError::Auth("unknown credential param".into())),
            }
        }
        let salt = decode_hex_exact::<SALT_LEN>(parts[3])?;
        let auth_key = decode_hex_exact::<32>(parts[4])?;
        Ok(Self {
            salt,
            m_kib: m_kib.ok_or_else(|| ProtoError::Auth("missing m param".into()))?,
            t_cost: t_cost.ok_or_else(|| ProtoError::Auth("missing t param".into()))?,
            p_cost: p_cost.ok_or_else(|| ProtoError::Auth("missing p param".into()))?,
            auth_key,
        })
    }
}

fn decode_hex_exact<const N: usize>(hex: &str) -> Result<[u8; N], ProtoError> {
    if hex.len() != N * 2 {
        return Err(ProtoError::Auth(format!("expected {} hex chars, got {}", N * 2, hex.len())));
    }
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| ProtoError::Auth("invalid hex character".into()))?;
    }
    Ok(out)
}

fn build_transcript(
    domain: &[u8],
    first_nonce: &[u8; NONCE_LEN],
    second_nonce: &[u8; NONCE_LEN],
    cert_sha256: &[u8; 32],
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(domain.len() + NONCE_LEN * 2 + 32);
    msg.extend_from_slice(domain);
    msg.extend_from_slice(first_nonce);
    msg.extend_from_slice(second_nonce);
    msg.extend_from_slice(cert_sha256);
    msg
}

/// Computes the TLS-channel-bound client HMAC-SHA256 authentication proof.
pub fn compute_client_proof(
    auth_key: &[u8; 32],
    client_nonce: &[u8; NONCE_LEN],
    server_nonce: &[u8; NONCE_LEN],
    cert_sha256: &[u8; 32],
) -> [u8; 32] {
    let key = Key::new(HMAC_SHA256, auth_key);
    let transcript = build_transcript(CLIENT_DOMAIN, client_nonce, server_nonce, cert_sha256);
    let tag = sign(&key, &transcript);
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

/// Constant-time verification of `client_proof`.
pub fn verify_client_proof(
    auth_key: &[u8; 32],
    client_nonce: &[u8; NONCE_LEN],
    server_nonce: &[u8; NONCE_LEN],
    cert_sha256: &[u8; 32],
    client_proof: &[u8; 32],
) -> bool {
    let key = Key::new(HMAC_SHA256, auth_key);
    let transcript = build_transcript(CLIENT_DOMAIN, client_nonce, server_nonce, cert_sha256);
    verify(&key, &transcript, client_proof).is_ok()
}

/// Computes the mutual server HMAC-SHA256 authentication proof.
pub fn compute_server_proof(
    auth_key: &[u8; 32],
    server_nonce: &[u8; NONCE_LEN],
    client_nonce: &[u8; NONCE_LEN],
    cert_sha256: &[u8; 32],
) -> [u8; 32] {
    let key = Key::new(HMAC_SHA256, auth_key);
    let transcript = build_transcript(SERVER_DOMAIN, server_nonce, client_nonce, cert_sha256);
    let tag = sign(&key, &transcript);
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

/// Constant-time verification of `server_proof`.
pub fn verify_server_proof(
    auth_key: &[u8; 32],
    server_nonce: &[u8; NONCE_LEN],
    client_nonce: &[u8; NONCE_LEN],
    cert_sha256: &[u8; 32],
    server_proof: &[u8; 32],
) -> bool {
    let key = Key::new(HMAC_SHA256, auth_key);
    let transcript = build_transcript(SERVER_DOMAIN, server_nonce, client_nonce, cert_sha256);
    verify(&key, &transcript, server_proof).is_ok()
}

#[derive(Debug, Clone)]
struct FailureEntry {
    failures: u32,
    window_start: Instant,
    blocked_until: Option<Instant>,
    last_seen: Instant,
}

/// Per-source-IP anti-brute-force rate limiter with optional configurable lockout threshold and ban duration.
#[derive(Debug, Default)]
pub struct IpRateLimiter {
    entries: HashMap<IpAddr, FailureEntry>,
    custom_policy: Option<(u32, u16)>,
}

impl IpRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(lockout_threshold: u32, ban_duration_secs: u16) -> Self {
        Self {
            entries: HashMap::new(),
            custom_policy: Some((lockout_threshold.max(1), ban_duration_secs.max(1))),
        }
    }

    pub fn normalize_ip(ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
            v4 => v4,
        }
    }

    /// Checks whether an IP is currently in a lockout/cooldown window.
    pub fn check(&mut self, ip: IpAddr, now: Instant) -> Result<(), u16> {
        let key = Self::normalize_ip(ip);
        if let Some(entry) = self.entries.get_mut(&key) {
            if now.duration_since(entry.window_start) >= RATE_LIMIT_WINDOW {
                self.entries.remove(&key);
                return Ok(());
            }
            if let Some(until) = entry.blocked_until
                && now < until
            {
                let secs = until.duration_since(now).as_secs().max(1).min(u16::MAX as u64);
                return Err(secs as u16);
            }
        }
        Ok(())
    }

    /// Records a failed authentication attempt and returns any cooldown seconds applied.
    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) -> u16 {
        let key = Self::normalize_ip(ip);
        self.evict_if_full(now);

        let entry = self.entries.entry(key).or_insert(FailureEntry {
            failures: 0,
            window_start: now,
            blocked_until: None,
            last_seen: now,
        });

        if now.duration_since(entry.window_start) >= RATE_LIMIT_WINDOW {
            entry.failures = 0;
            entry.window_start = now;
            entry.blocked_until = None;
        }

        entry.failures = entry.failures.saturating_add(1);
        entry.last_seen = now;

        let cooldown_secs: u16 = if let Some((threshold, ban_secs)) = self.custom_policy {
            if entry.failures >= threshold {
                ban_secs
            } else {
                0
            }
        } else {
            match entry.failures {
                0..=2 => 0,
                3 => 2,
                4 => 5,
                5 => 15,
                _ => 60,
            }
        };
        if cooldown_secs > 0 {
            entry.blocked_until = Some(now + Duration::from_secs(cooldown_secs as u64));
        }
        cooldown_secs
    }

    /// Clears failure history after a successful authentication.
    pub fn record_success(&mut self, ip: IpAddr) {
        let key = Self::normalize_ip(ip);
        self.entries.remove(&key);
    }

    fn evict_if_full(&mut self, now: Instant) {
        if self.entries.len() < RATE_LIMIT_MAX_IPS {
            return;
        }
        self.entries
            .retain(|_, v| now.duration_since(v.window_start) < RATE_LIMIT_WINDOW);
        if self.entries.len() >= RATE_LIMIT_MAX_IPS
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.last_seen)
                .map(|(k, _)| *k)
        {
            self.entries.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use super::*;

    #[test]
    fn argon2id_and_hmac_mutual_auth_verification() {
        // Use fast Argon2 params in unit test while keeping production defaults 64 MiB
        let salt = [0x11u8; SALT_LEN];
        let key = derive_auth_key(b"AdminPass!2026", &salt, 1024, 1, 1).unwrap();
        let bad_key = derive_auth_key(b"WrongPass", &salt, 1024, 1, 1).unwrap();

        let c_nonce = [0x22u8; NONCE_LEN];
        let s_nonce = [0x33u8; NONCE_LEN];
        let cert_fp = [0x44u8; 32];

        let c_proof = compute_client_proof(&key, &c_nonce, &s_nonce, &cert_fp);
        assert!(verify_client_proof(&key, &c_nonce, &s_nonce, &cert_fp, &c_proof));
        assert!(!verify_client_proof(&bad_key, &c_nonce, &s_nonce, &cert_fp, &c_proof));
        assert!(!verify_client_proof(&key, &c_nonce, &s_nonce, &[0x99; 32], &c_proof));

        let s_proof = compute_server_proof(&key, &s_nonce, &c_nonce, &cert_fp);
        assert!(verify_server_proof(&key, &s_nonce, &c_nonce, &cert_fp, &s_proof));
        assert!(!verify_server_proof(&key, &c_nonce, &s_nonce, &cert_fp, &s_proof));
    }

    #[test]
    fn stored_credential_encode_decode_roundtrip() {
        let cred = StoredCredential {
            salt: [0xAB; SALT_LEN],
            m_kib: ARGON2_M_KIB,
            t_cost: ARGON2_T_COST,
            p_cost: ARGON2_P_COST,
            auth_key: [0xCD; 32],
        };
        let encoded = cred.encode_string();
        assert_eq!(StoredCredential::decode_string(&encoded).unwrap(), cred);
        assert!(StoredCredential::decode_string("$invalid$format").is_err());
    }

    #[test]
    fn rate_limiter_normalizes_ipv6_mapped_and_escalates() {
        let mut limiter = IpRateLimiter::new();
        let t0 = Instant::now();
        let v4: IpAddr = "10.0.0.5".parse().unwrap();
        let v6_mapped = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0a00, 0x0005));

        assert_eq!(limiter.record_failure(v4, t0), 0);
        assert_eq!(limiter.record_failure(v6_mapped, t0), 0);
        assert_eq!(limiter.record_failure(v4, t0), 2);
        assert!(matches!(limiter.check(v6_mapped, t0), Err(2)));

        let t3 = t0 + Duration::from_secs(3);
        assert!(limiter.check(v4, t3).is_ok());
        assert_eq!(limiter.record_failure(v4, t3), 5);
        assert_eq!(limiter.record_failure(v4, t3 + Duration::from_secs(6)), 15);
        assert_eq!(limiter.record_failure(v4, t3 + Duration::from_secs(22)), 60);

        limiter.record_success(v6_mapped);
        assert!(limiter.check(v4, t3 + Duration::from_secs(22)).is_ok());

        let mut custom = IpRateLimiter::with_limits(2, 90);
        assert_eq!(custom.record_failure(v4, t0), 0);
        assert_eq!(custom.record_failure(v4, t0), 90);
        assert!(matches!(custom.check(v4, t0), Err(90)));
    }
}
