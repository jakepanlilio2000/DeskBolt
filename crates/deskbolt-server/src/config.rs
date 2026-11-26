use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use deskbolt_proto::{
    DEFAULT_CLIPBOARD_MAX_BYTES, DEFAULT_PORT, GeneratedIdentity, MAX_CLIPBOARD_BYTES, SessionMode, StoredCredential, VideoQualityMode,
    generate_self_signed_cert, load_identity_from_pem_bytes,
};

use crate::platform::{
    dpapi_protect_machine, dpapi_unprotect_machine, read_password_masked, set_restrictive_acl,
};

/// Default configuration and certificate directory across Windows, Linux, macOS, and Android.
pub fn default_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        base.join("DeskBolt")
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            let config_home = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
            if let Some(config_home) = config_home {
                return config_home.join("deskbolt");
            }
        }
        PathBuf::from("/etc/deskbolt")
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0
            && let Some(home) = std::env::var_os("HOME")
        {
            return PathBuf::from(home).join("Library/Application Support/DeskBolt");
        }
        PathBuf::from("/Library/Application Support/DeskBolt")
    }
    #[cfg(target_os = "android")]
    {
        crate::platform::android::android_data_dir()
    }
}

/// Per-feature permission toggles configured on the DeskBolt host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeaturePermissions {
    pub full_control: bool,
    pub view_only: bool,
    pub file_transfer: bool,
    pub terminal: bool,
    pub power_control: bool,
    pub text_chat: bool,
    pub voice_chat: bool,
    pub send_message: bool,
    pub tunnels: bool,
}

impl Default for FeaturePermissions {
    fn default() -> Self {
        Self {
            full_control: true,
            view_only: true,
            file_transfer: true,
            terminal: true,
            power_control: true,
            text_chat: true,
            voice_chat: true,
            send_message: true,
            tunnels: true,
        }
    }
}

impl FeaturePermissions {
    pub fn allows_mode(&self, mode: SessionMode) -> bool {
        match mode {
            SessionMode::FullControl => self.full_control,
            SessionMode::ViewOnly => self.view_only,
            SessionMode::FileTransfer => self.file_transfer,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub bind_addr: String,
    pub port: u16,
    pub password_hash: Option<String>,
    pub single_controller_lock: bool,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    // General & startup
    pub start_with_os: bool,
    pub show_tray_icon: bool,
    // Security & access
    pub ip_allowlist: Vec<String>,
    pub ip_blocklist: Vec<String>,
    pub lockout_threshold: u32,
    pub ban_duration_secs: u16,
    pub confirm_connect: bool,
    pub confirm_timeout_secs: u16,
    pub confirm_default_allow: bool,
    pub force_view_only: bool,
    pub audit_log_path: Option<PathBuf>,
    // Feature toggles
    pub permissions: FeaturePermissions,
    pub clipboard_max_bytes: usize,
    // Capture & privacy
    pub default_quality: VideoQualityMode,
    pub max_fps: u8,
    pub capture_cursor: bool,
    pub curtain_on_control: bool,
}

impl ServerConfig {
    pub fn for_dir(data_dir: &Path) -> Self {
        let certs = data_dir.join("certs");
        Self {
            bind_addr: "0.0.0.0".to_string(),
            port: DEFAULT_PORT,
            password_hash: None,
            single_controller_lock: false,
            cert_path: certs.join("server.crt.pem"),
            key_path: certs.join("server.key.pem"),
            start_with_os: true,
            show_tray_icon: true,
            ip_allowlist: Vec::new(),
            ip_blocklist: Vec::new(),
            lockout_threshold: 5,
            ban_duration_secs: 60,
            confirm_connect: false,
            confirm_timeout_secs: 15,
            confirm_default_allow: false,
            force_view_only: false,
            audit_log_path: None,
            permissions: FeaturePermissions::default(),
            clipboard_max_bytes: DEFAULT_CLIPBOARD_MAX_BYTES,
            default_quality: VideoQualityMode::Auto,
            max_fps: 60,
            capture_cursor: true,
            curtain_on_control: false,
        }
    }

    pub fn config_path_in(data_dir: &Path) -> PathBuf {
        data_dir.join("config.toml")
    }

    /// Parses simple key=value TOML subset without requiring an external parser crate.
    pub fn parse_toml(text: &str, data_dir: &Path) -> Result<Self, String> {
        let mut cfg = Self::for_dir(data_dir);
        for (line_idx, raw_line) in text.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, val) = line
                .split_once('=')
                .ok_or_else(|| format!("config.toml line {}: missing '='", line_idx + 1))?;
            let k = key.trim();
            let v = val.trim();
            let ln = line_idx + 1;
            match k {
                "bind_addr" => cfg.bind_addr = unquote(v)?,
                "port" => {
                    cfg.port = v
                        .parse::<u16>()
                        .map_err(|_| format!("invalid port on line {ln}"))?;
                }
                "password_hash" => {
                    let s = unquote(v)?;
                    cfg.password_hash = if s.is_empty() { None } else { Some(s) };
                }
                "single_controller_lock" => cfg.single_controller_lock = parse_bool(v, ln)?,
                "cert_path" => cfg.cert_path = PathBuf::from(unquote(v)?),
                "key_path" => cfg.key_path = PathBuf::from(unquote(v)?),
                "start_with_os" => cfg.start_with_os = parse_bool(v, ln)?,
                "show_tray_icon" => cfg.show_tray_icon = parse_bool(v, ln)?,
                "ip_allowlist" => cfg.ip_allowlist = parse_csv_list(&unquote(v)?),
                "ip_blocklist" => cfg.ip_blocklist = parse_csv_list(&unquote(v)?),
                "lockout_threshold" => {
                    cfg.lockout_threshold = v
                        .parse::<u32>()
                        .map_err(|_| format!("invalid lockout_threshold on line {ln}"))?
                        .max(1);
                }
                "ban_duration_secs" => {
                    cfg.ban_duration_secs = v
                        .parse::<u16>()
                        .map_err(|_| format!("invalid ban_duration_secs on line {ln}"))?
                        .max(1);
                }
                "confirm_connect" => cfg.confirm_connect = parse_bool(v, ln)?,
                "confirm_timeout_secs" => {
                    cfg.confirm_timeout_secs = v
                        .parse::<u16>()
                        .map_err(|_| format!("invalid confirm_timeout_secs on line {ln}"))?
                        .clamp(3, 120);
                }
                "confirm_default_allow" => cfg.confirm_default_allow = parse_bool(v, ln)?,
                "force_view_only" => cfg.force_view_only = parse_bool(v, ln)?,
                "audit_log_path" => {
                    let s = unquote(v)?;
                    cfg.audit_log_path = if s.is_empty() { None } else { Some(PathBuf::from(s)) };
                }
                "allow_full_control" => cfg.permissions.full_control = parse_bool(v, ln)?,
                "allow_view_only" => cfg.permissions.view_only = parse_bool(v, ln)?,
                "allow_file_transfer" => cfg.permissions.file_transfer = parse_bool(v, ln)?,
                "allow_terminal" => cfg.permissions.terminal = parse_bool(v, ln)?,
                "allow_power_control" => cfg.permissions.power_control = parse_bool(v, ln)?,
                "allow_text_chat" => cfg.permissions.text_chat = parse_bool(v, ln)?,
                "allow_voice_chat" => cfg.permissions.voice_chat = parse_bool(v, ln)?,
                "allow_send_message" => cfg.permissions.send_message = parse_bool(v, ln)?,
                "allow_tunnels" => cfg.permissions.tunnels = parse_bool(v, ln)?,
                "default_quality" => cfg.default_quality = parse_quality_str(&unquote(v)?),
                "max_fps" => {
                    cfg.max_fps = v
                        .parse::<u8>()
                        .map_err(|_| format!("invalid max_fps on line {ln}"))?
                        .clamp(15, 120);
                }
                "clipboard_max_bytes" => {
                    cfg.clipboard_max_bytes = v.parse::<usize>()
                        .map_err(|_| format!("invalid clipboard_max_bytes on line {ln}"))?
                        .min(MAX_CLIPBOARD_BYTES);
                }
                "capture_cursor" => cfg.capture_cursor = parse_bool(v, ln)?,
                "curtain_on_control" => cfg.curtain_on_control = parse_bool(v, ln)?,
                _ => {}
            }
        }
        Ok(cfg)
    }

    pub fn to_toml(&self) -> String {
        let pw = self.password_hash.as_deref().unwrap_or("");
        let audit = self
            .audit_log_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!(
            "# DeskBolt Host Configuration\n\
             bind_addr = \"{}\"\n\
             port = {}\n\
             single_controller_lock = {}\n\
             cert_path = \"{}\"\n\
             key_path = \"{}\"\n\
             password_hash = \"{}\"\n\
             start_with_os = {}\n\
             show_tray_icon = {}\n\
             ip_allowlist = \"{}\"\n\
             ip_blocklist = \"{}\"\n\
             lockout_threshold = {}\n\
             ban_duration_secs = {}\n\
             confirm_connect = {}\n\
             confirm_timeout_secs = {}\n\
             confirm_default_allow = {}\n\
             force_view_only = {}\n\
             audit_log_path = \"{}\"\n\
             allow_full_control = {}\n\
             allow_view_only = {}\n\
             allow_file_transfer = {}\n\
             allow_terminal = {}\n\
             allow_power_control = {}\n\
             allow_text_chat = {}\n\
             allow_voice_chat = {}\n\
             allow_send_message = {}\n\
             allow_tunnels = {}\n\
             default_quality = \"{}\"\n\
             max_fps = {}\n\
             clipboard_max_bytes = {}\n\
             capture_cursor = {}\n\
             curtain_on_control = {}\n",
            escape_toml(&self.bind_addr),
            self.port,
            self.single_controller_lock,
            escape_toml(&self.cert_path.to_string_lossy()),
            escape_toml(&self.key_path.to_string_lossy()),
            escape_toml(pw),
            self.start_with_os,
            self.show_tray_icon,
            escape_toml(&self.ip_allowlist.join(",")),
            escape_toml(&self.ip_blocklist.join(",")),
            self.lockout_threshold.max(1),
            self.ban_duration_secs.max(1),
            self.confirm_connect,
            self.confirm_timeout_secs.clamp(3, 120),
            self.confirm_default_allow,
            self.force_view_only,
            escape_toml(&audit),
            self.permissions.full_control,
            self.permissions.view_only,
            self.permissions.file_transfer,
            self.permissions.terminal,
            self.permissions.power_control,
            self.permissions.text_chat,
            self.permissions.voice_chat,
            self.permissions.send_message,
            self.permissions.tunnels,
            quality_to_str(self.default_quality),
            self.max_fps.clamp(15, 120),
            self.clipboard_max_bytes.min(MAX_CLIPBOARD_BYTES),
            self.capture_cursor,
            self.curtain_on_control,
        )
    }

    pub fn load_or_default(data_dir: &Path) -> Result<Self, String> {
        let path = Self::config_path_in(data_dir);
        if !path.exists() {
            return Ok(Self::for_dir(data_dir));
        }
        let text = fs::read_to_string(&path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        Self::parse_toml(&text, data_dir)
    }

    pub fn save(&self, data_dir: &Path) -> Result<(), String> {
        fs::create_dir_all(data_dir)
            .map_err(|e| format!("failed to create {}: {e}", data_dir.display()))?;
        let path = Self::config_path_in(data_dir);
        fs::write(&path, self.to_toml())
            .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        // Best effort restrictive ACL when running elevated; do not fail local non-admin tests.
        let _ = set_restrictive_acl(&path);
        Ok(())
    }

    /// Extracts the Argon2id stored credential (unsealing DPAPI wrapper if present).
    pub fn load_credential(&self) -> Result<StoredCredential, String> {
        let raw = self
            .password_hash
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "host password is not configured; run deskbolt-server --set-password".to_string())?;

        let decoded_str = if let Some(hex_blob) = raw.strip_prefix("dpapi:") {
            let cipher = decode_hex(hex_blob)?;
            let plain = dpapi_unprotect_machine(&cipher)?;
            String::from_utf8(plain).map_err(|_| "corrupt DPAPI credential UTF-8".to_string())?
        } else {
            raw.to_string()
        };

        StoredCredential::decode_string(&decoded_str).map_err(|e| e.to_string())
    }

    pub fn set_password_bytes(&mut self, password: &[u8]) -> Result<(), String> {
        if password.is_empty() {
            return Err("password cannot be empty".into());
        }
        let cred = StoredCredential::from_password(password).map_err(|e| e.to_string())?;
        let encoded = cred.encode_string();
        match dpapi_protect_machine(encoded.as_bytes()) {
            Ok(blob) => {
                self.password_hash = Some(format!("dpapi:{}", encode_hex(&blob)));
            }
            Err(_) => {
                self.password_hash = Some(encoded);
            }
        }
        Ok(())
    }
}

/// Ensures persistent X.509 certificate and private key exist on disk.
pub fn load_or_generate_identity(
    cfg: &ServerConfig,
    force_regen: bool,
) -> Result<GeneratedIdentity, String> {
    if !force_regen && cfg.cert_path.exists() && cfg.key_path.exists() {
        let cert_pem = fs::read_to_string(&cfg.cert_path)
            .map_err(|e| format!("read cert {}: {e}", cfg.cert_path.display()))?;
        let key_pem = fs::read_to_string(&cfg.key_path)
            .map_err(|e| format!("read key {}: {e}", cfg.key_path.display()))?;
        return load_identity_from_pem_bytes(&cert_pem, &key_pem).map_err(|e| e.to_string());
    }

    if let Some(parent) = cfg.cert_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create cert dir {}: {e}", parent.display()))?;
    }
    if let Some(parent) = cfg.key_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create key dir {}: {e}", parent.display()))?;
    }

    let mut sans = Vec::new();
    if let Ok(hostname) = std::env::var("COMPUTERNAME") {
        sans.push(hostname);
    }
    let id = generate_self_signed_cert(&sans).map_err(|e| e.to_string())?;
    fs::write(&cfg.cert_path, &id.cert_pem)
        .map_err(|e| format!("write cert {}: {e}", cfg.cert_path.display()))?;
    fs::write(&cfg.key_path, &id.key_pem)
        .map_err(|e| format!("write key {}: {e}", cfg.key_path.display()))?;
    let _ = set_restrictive_acl(&cfg.key_path);
    Ok(id)
}

/// Source for host access password during `--set-password`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordSource {
    Interactive,
    Stdin,
    Env(String),
}

/// Parsed CLI options for `deskbolt-server --set-password`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetPasswordOptions {
    pub source: PasswordSource,
    pub port: Option<u16>,
}

/// Parses arguments following `--set-password`.
pub fn parse_set_password_args(args: &[String]) -> Result<SetPasswordOptions, String> {
    let mut source: Option<PasswordSource> = None;
    let mut port: Option<u16> = None;
    let mut i = 0;

    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--password-stdin" {
            if matches!(source, Some(PasswordSource::Env(_))) {
                return Err("cannot combine --password-stdin and --password-env".into());
            }
            source = Some(PasswordSource::Stdin);
        } else if let Some(var) = arg.strip_prefix("--password-env=") {
            if matches!(source, Some(PasswordSource::Stdin)) {
                return Err("cannot combine --password-stdin and --password-env".into());
            }
            if var.trim().is_empty() {
                return Err("--password-env requires a non-empty variable name".into());
            }
            source = Some(PasswordSource::Env(var.to_string()));
        } else if arg == "--password-env" {
            if matches!(source, Some(PasswordSource::Stdin)) {
                return Err("cannot combine --password-stdin and --password-env".into());
            }
            i += 1;
            let var = args
                .get(i)
                .map(|s| s.as_str())
                .filter(|s| !s.trim().is_empty() && !s.starts_with('-'))
                .ok_or_else(|| "--password-env requires a variable name".to_string())?;
            source = Some(PasswordSource::Env(var.to_string()));
        } else if let Some(raw_port) = arg.strip_prefix("--port=") {
            port = Some(parse_port_arg(raw_port)?);
        } else if arg == "--port" {
            i += 1;
            let raw_port = args
                .get(i)
                .map(|s| s.as_str())
                .ok_or_else(|| "--port requires a numeric port (1-65535)".to_string())?;
            port = Some(parse_port_arg(raw_port)?);
        } else {
            return Err(format!("unknown --set-password option: {arg}"));
        }
        i += 1;
    }

    Ok(SetPasswordOptions {
        source: source.unwrap_or(PasswordSource::Interactive),
        port,
    })
}

fn parse_port_arg(raw: &str) -> Result<u16, String> {
    let p = raw
        .parse::<u16>()
        .map_err(|_| format!("invalid port '{raw}': expected 1-65535"))?;
    if p == 0 {
        return Err("port must be between 1 and 65535".into());
    }
    Ok(p)
}

/// Interactive or `--password-stdin` host password setup command.
pub fn run_set_password(data_dir: &Path, from_stdin: bool) -> Result<(), String> {
    let source = if from_stdin {
        PasswordSource::Stdin
    } else {
        PasswordSource::Interactive
    };
    run_set_password_with(data_dir, source, None)
}

/// Configures the host access password (and optional listening port) from `source`.
pub fn run_set_password_with(
    data_dir: &Path,
    source: PasswordSource,
    port_override: Option<u16>,
) -> Result<(), String> {
    run_set_password_with_env(data_dir, source, port_override, |var| std::env::var(var).ok())
}

fn run_set_password_with_env(
    data_dir: &Path,
    source: PasswordSource,
    port_override: Option<u16>,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let password = match source {
        PasswordSource::Stdin => {
            let mut line = String::new();
            io::stdin()
                .lock()
                .read_line(&mut line)
                .map_err(|e| format!("read password from stdin: {e}"))?;
            line.trim_end_matches(['\r', '\n']).to_string()
        }
        PasswordSource::Env(var) => env_lookup(&var)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("environment variable '{var}' is not set or empty"))?,
        PasswordSource::Interactive => {
            print!("Enter new DeskBolt host password: ");
            let _ = io::stdout().flush();
            let p1 = read_password_masked()?;
            println!();
            print!("Confirm new DeskBolt host password: ");
            let _ = io::stdout().flush();
            let p2 = read_password_masked()?;
            println!();
            if p1 != p2 {
                return Err("passwords do not match".into());
            }
            p1
        }
    };

    let mut cfg = ServerConfig::load_or_default(data_dir)?;
    if let Some(p) = port_override {
        if p == 0 {
            return Err("port must be between 1 and 65535".into());
        }
        cfg.port = p;
    }
    cfg.set_password_bytes(password.as_bytes())?;
    cfg.save(data_dir)?;
    println!(
        "Host password saved to {}",
        ServerConfig::config_path_in(data_dir).display()
    );
    Ok(())
}

/// Runtime enforcement policy derived from `ServerConfig`.
#[derive(Debug, Clone)]
pub struct ServerPolicy {
    pub single_controller_lock: bool,
    pub ip_allowlist: Vec<String>,
    pub ip_blocklist: Vec<String>,
    pub lockout_threshold: Option<u32>,
    pub ban_duration_secs: u16,
    pub confirm_connect: bool,
    pub confirm_timeout_secs: u16,
    pub confirm_default_allow: bool,
    pub force_view_only: bool,
    pub audit_log_path: Option<PathBuf>,
    pub permissions: FeaturePermissions,
    pub clipboard_max_bytes: usize,
    pub default_quality: VideoQualityMode,
    pub max_fps: u8,
    pub capture_cursor: bool,
    pub curtain_on_control: bool,
    #[cfg(any(test, all(feature = "dev-mcp", not(target_os = "android"))))]
    pub(crate) suppress_os_prompts: bool,
}

impl ServerPolicy {
    pub fn from_config(cfg: &ServerConfig) -> Self {
        Self {
            single_controller_lock: cfg.single_controller_lock,
            ip_allowlist: cfg.ip_allowlist.clone(),
            ip_blocklist: cfg.ip_blocklist.clone(),
            lockout_threshold: Some(cfg.lockout_threshold.max(1)),
            ban_duration_secs: cfg.ban_duration_secs.max(1),
            confirm_connect: cfg.confirm_connect,
            confirm_timeout_secs: cfg.confirm_timeout_secs.clamp(3, 120),
            confirm_default_allow: cfg.confirm_default_allow,
            force_view_only: cfg.force_view_only,
            audit_log_path: cfg.audit_log_path.clone(),
            permissions: cfg.permissions.clone(),
            clipboard_max_bytes: cfg.clipboard_max_bytes.min(MAX_CLIPBOARD_BYTES),
            default_quality: cfg.default_quality,
            max_fps: cfg.max_fps.clamp(15, 120),
            capture_cursor: cfg.capture_cursor,
            curtain_on_control: cfg.curtain_on_control,
            #[cfg(any(test, all(feature = "dev-mcp", not(target_os = "android"))))]
            suppress_os_prompts: false,
        }
    }

    #[cfg(any(test, all(feature = "dev-mcp", not(target_os = "android"))))]
    pub(crate) fn os_prompts_enabled(&self) -> bool {
        !self.suppress_os_prompts
    }

    #[cfg(not(any(test, all(feature = "dev-mcp", not(target_os = "android")))))]
    pub(crate) fn os_prompts_enabled(&self) -> bool {
        true
    }

    #[cfg(any(test, all(feature = "dev-mcp", not(target_os = "android"))))]
    pub(crate) fn for_test(single_controller_lock: bool) -> Self {
        Self {
            single_controller_lock,
            ip_allowlist: Vec::new(),
            ip_blocklist: Vec::new(),
            lockout_threshold: None,
            ban_duration_secs: 60,
            confirm_connect: false,
            confirm_timeout_secs: 15,
            confirm_default_allow: false,
            force_view_only: false,
            audit_log_path: None,
            permissions: FeaturePermissions::default(),
            clipboard_max_bytes: DEFAULT_CLIPBOARD_MAX_BYTES,
            default_quality: VideoQualityMode::Auto,
            max_fps: 60,
            capture_cursor: true,
            curtain_on_control: false,
            suppress_os_prompts: true,
        }
    }
}

fn parse_bool(v: &str, line: usize) -> Result<bool, String> {
    v.parse::<bool>()
        .map_err(|_| format!("invalid bool on line {line}"))
}

fn parse_csv_list(raw: &str) -> Vec<String> {
    raw.split([',', ';', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_quality_str(s: &str) -> VideoQualityMode {
    match s.trim().to_ascii_lowercase().as_str() {
        "crisp" | "crisp_text" | "crisptext" => VideoQualityMode::CrispText,
        "smooth" | "smooth_video" | "smoothvideo" | "motion" => VideoQualityMode::SmoothVideo,
        _ => VideoQualityMode::Auto,
    }
}

fn quality_to_str(q: VideoQualityMode) -> &'static str {
    match q {
        VideoQualityMode::Auto => "auto",
        VideoQualityMode::CrispText => "crisp_text",
        VideoQualityMode::SmoothVideo => "smooth_video",
    }
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

/// Evaluates whether `peer_ip` matches CIDR string `rule` (`192.168.1.0/24`, `10.0.0.5`, `fe80::/10`).
pub fn cidr_matches(rule: &str, peer_ip: IpAddr) -> Result<bool, String> {
    let trimmed = rule.trim();
    if trimmed.is_empty() {
        return Ok(false);
    }
    let ip = normalize_ip(peer_ip);
    let (addr_str, prefix_opt) = match trimmed.split_once('/') {
        Some((a, p)) => (a.trim(), Some(p.trim())),
        None => (trimmed, None),
    };
    let net_ip = normalize_ip(
        addr_str
            .parse::<IpAddr>()
            .map_err(|_| format!("invalid IP or CIDR '{trimmed}'"))?,
    );
    match (ip, net_ip) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let prefix = match prefix_opt {
                Some(p) => p
                    .parse::<u8>()
                    .map_err(|_| format!("invalid prefix '{trimmed}'"))?,
                None => 32,
            };
            if prefix > 32 {
                return Err(format!("invalid IPv4 CIDR prefix /{prefix}"));
            }
            let mask = if prefix == 0 {
                0u32
            } else {
                u32::MAX.checked_shl(32 - prefix as u32).unwrap_or(0)
            };
            let bits_a: u32 = Ipv4Addr::from(a.octets()).into();
            let bits_b: u32 = Ipv4Addr::from(b.octets()).into();
            Ok((bits_a & mask) == (bits_b & mask))
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let prefix = match prefix_opt {
                Some(p) => p
                    .parse::<u8>()
                    .map_err(|_| format!("invalid prefix '{trimmed}'"))?,
                None => 128,
            };
            if prefix > 128 {
                return Err(format!("invalid IPv6 CIDR prefix /{prefix}"));
            }
            let mask = if prefix == 0 {
                0u128
            } else {
                u128::MAX.checked_shl(128 - prefix as u32).unwrap_or(0)
            };
            let bits_a: u128 = Ipv6Addr::from(a.octets()).into();
            let bits_b: u128 = Ipv6Addr::from(b.octets()).into();
            Ok((bits_a & mask) == (bits_b & mask))
        }
        _ => Ok(false),
    }
}

/// Applies blocklist-first, optional allowlist IP access control rules.
pub fn evaluate_ip_access(
    peer_ip: IpAddr,
    allowlist: &[String],
    blocklist: &[String],
) -> Result<(), String> {
    for rule in blocklist {
        if cidr_matches(rule, peer_ip).unwrap_or(false) {
            return Err(format!("Connection denied by blocklist rule '{rule}'"));
        }
    }
    let active_allows: Vec<&String> = allowlist.iter().filter(|r| !r.trim().is_empty()).collect();
    if !active_allows.is_empty()
        && !active_allows
            .iter()
            .any(|rule| cidr_matches(rule, peer_ip).unwrap_or(false))
    {
        return Err("Connection denied: peer IP is not in server allowlist".into());
    }
    Ok(())
}

/// Appends a timestamped event record when `audit_log_path` is configured.
pub fn log_audit_event(audit_log: Option<&Path>, peer: &str, event: &str, detail: &str) {
    let Some(path) = audit_log else {
        return;
    };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = fs::create_dir_all(parent);
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{ts}\t{peer}\t{event}\t{detail}");
    }
}

fn unquote(v: &str) -> Result<String, String> {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        Ok(inner.replace(r"\\", r"\").replace(r#"\""#, "\""))
    } else {
        Err(format!("expected quoted string, got {v}"))
    }
}

fn escape_toml(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', r#"\""#)
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("invalid DPAPI hex length".into());
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex[i..i + 2], 16)
            .map_err(|_| "invalid DPAPI hex character".to_string())?;
        out.push(byte);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_and_dpapi_credential_roundtrip() {
        let temp = std::env::temp_dir().join(format!("deskbolt_cfg_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        let mut cfg = ServerConfig::for_dir(&temp);
        cfg.port = 5900;
        cfg.single_controller_lock = true;
        cfg.set_password_bytes(b"SecretHost!99").unwrap();
        cfg.save(&temp).unwrap();

        let reloaded = ServerConfig::load_or_default(&temp).unwrap();
        assert_eq!(reloaded.port, 5900);
        assert!(reloaded.single_controller_lock);
        assert_eq!(reloaded.cert_path, cfg.cert_path);

        let cred = reloaded.load_credential().unwrap();
        let expected_key = deskbolt_proto::derive_auth_key(
            b"SecretHost!99",
            &cred.salt,
            cred.m_kib,
            cred.t_cost,
            cred.p_cost,
        )
        .unwrap();
        assert_eq!(cred.auth_key, expected_key);

        let id = load_or_generate_identity(&reloaded, false).unwrap();
        let id2 = load_or_generate_identity(&reloaded, false).unwrap();
        assert_eq!(id.fingerprint, id2.fingerprint);

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn parses_set_password_cli_variants() {
        let s = |vals: &[&str]| vals.iter().map(|v| (*v).to_string()).collect::<Vec<_>>();

        assert_eq!(
            parse_set_password_args(&s(&[])).unwrap(),
            SetPasswordOptions {
                source: PasswordSource::Interactive,
                port: None,
            }
        );
        assert_eq!(
            parse_set_password_args(&s(&["--password-stdin"])).unwrap(),
            SetPasswordOptions {
                source: PasswordSource::Stdin,
                port: None,
            }
        );
        assert_eq!(
            parse_set_password_args(&s(&[
                "--password-env",
                "DESKBOLT_INIT_PASSWORD",
                "--port",
                "4900",
            ]))
            .unwrap(),
            SetPasswordOptions {
                source: PasswordSource::Env("DESKBOLT_INIT_PASSWORD".to_string()),
                port: Some(4900),
            }
        );
        assert_eq!(
            parse_set_password_args(&s(&["--password-env=MY_PASS", "--port=4899"])).unwrap(),
            SetPasswordOptions {
                source: PasswordSource::Env("MY_PASS".to_string()),
                port: Some(4899),
            }
        );

        assert!(parse_set_password_args(&s(&["--password-stdin", "--password-env", "A"])).is_err());
        assert!(parse_set_password_args(&s(&["--password-env"])).is_err());
        assert!(parse_set_password_args(&s(&["--port", "0"])).is_err());
        assert!(parse_set_password_args(&s(&["--unknown"])).is_err());
    }

    #[test]
    fn non_interactive_env_password_and_port_persist() {
        let temp = std::env::temp_dir().join(format!("deskbolt_pw_env_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        let err = run_set_password_with_env(
            &temp,
            PasswordSource::Env("DESKBOLT_INIT_PASSWORD".to_string()),
            Some(5901),
            |_| None,
        )
        .unwrap_err();
        assert!(err.contains("not set or empty"));

        run_set_password_with_env(
            &temp,
            PasswordSource::Env("DESKBOLT_INIT_PASSWORD".to_string()),
            Some(5901),
            |k| (k == "DESKBOLT_INIT_PASSWORD").then(|| "InstallerSecret!88".to_string()),
        )
        .unwrap();

        let reloaded = ServerConfig::load_or_default(&temp).unwrap();
        assert_eq!(reloaded.port, 5901);
        let cred = reloaded.load_credential().unwrap();
        let expected_key = deskbolt_proto::derive_auth_key(
            b"InstallerSecret!88",
            &cred.salt,
            cred.m_kib,
            cred.t_cost,
            cred.p_cost,
        )
        .unwrap();
        assert_eq!(cred.auth_key, expected_key);

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn full_server_settings_toml_and_cidr_access_roundtrip() {
        let temp = std::env::temp_dir().join(format!("deskbolt_full_cfg_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();

        let mut cfg = ServerConfig::for_dir(&temp);
        cfg.bind_addr = "10.10.0.5".into();
        cfg.port = 5500;
        cfg.start_with_os = false;
        cfg.show_tray_icon = false;
        cfg.ip_allowlist = vec!["192.168.10.0/24".into(), "10.0.0.0/8".into()];
        cfg.ip_blocklist = vec!["192.168.10.99".into()];
        cfg.lockout_threshold = 3;
        cfg.ban_duration_secs = 120;
        cfg.confirm_connect = true;
        cfg.confirm_timeout_secs = 25;
        cfg.confirm_default_allow = true;
        cfg.force_view_only = true;
        cfg.audit_log_path = Some(temp.join("audit.log"));
        cfg.permissions.power_control = false;
        cfg.permissions.voice_chat = false;
        cfg.default_quality = VideoQualityMode::CrispText;
        cfg.max_fps = 45;
        cfg.clipboard_max_bytes = 4096;
        cfg.capture_cursor = false;
        cfg.curtain_on_control = true;
        cfg.save(&temp).unwrap();

        let reloaded = ServerConfig::load_or_default(&temp).unwrap();
        assert_eq!(reloaded.bind_addr, "10.10.0.5");
        assert_eq!(reloaded.port, 5500);
        assert!(!reloaded.start_with_os);
        assert!(!reloaded.show_tray_icon);
        assert_eq!(reloaded.ip_allowlist, vec!["192.168.10.0/24", "10.0.0.0/8"]);
        assert_eq!(reloaded.ip_blocklist, vec!["192.168.10.99"]);
        assert_eq!(reloaded.lockout_threshold, 3);
        assert_eq!(reloaded.ban_duration_secs, 120);
        assert!(reloaded.confirm_connect);
        assert_eq!(reloaded.confirm_timeout_secs, 25);
        assert!(reloaded.confirm_default_allow);
        assert!(reloaded.force_view_only);
        assert_eq!(reloaded.audit_log_path, Some(temp.join("audit.log")));
        assert!(!reloaded.permissions.power_control);
        assert!(!reloaded.permissions.voice_chat);
        assert!(reloaded.permissions.text_chat);
        assert_eq!(reloaded.default_quality, VideoQualityMode::CrispText);
        assert_eq!(reloaded.max_fps, 45);
        assert_eq!(reloaded.clipboard_max_bytes, 4096);
        assert!(!reloaded.capture_cursor);
        assert!(reloaded.curtain_on_control);

        // CIDR evaluation checks including IPv4-mapped IPv6 and blocklist priority
        let allowed_v4: IpAddr = "192.168.10.42".parse().unwrap();
        let allowed_mapped: IpAddr = "::ffff:10.5.6.7".parse().unwrap();
        let blocked_v4: IpAddr = "192.168.10.99".parse().unwrap();
        let outside_v4: IpAddr = "172.16.0.1".parse().unwrap();

        assert!(evaluate_ip_access(allowed_v4, &reloaded.ip_allowlist, &reloaded.ip_blocklist).is_ok());
        assert!(evaluate_ip_access(allowed_mapped, &reloaded.ip_allowlist, &reloaded.ip_blocklist).is_ok());
        assert!(evaluate_ip_access(blocked_v4, &reloaded.ip_allowlist, &reloaded.ip_blocklist).is_err());
        assert!(evaluate_ip_access(outside_v4, &reloaded.ip_allowlist, &reloaded.ip_blocklist).is_err());
        assert!(evaluate_ip_access(outside_v4, &[], &[]).is_ok());

        let _ = fs::remove_dir_all(&temp);
    }
}
