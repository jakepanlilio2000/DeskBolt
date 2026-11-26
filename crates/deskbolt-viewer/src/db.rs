use std::path::{Path, PathBuf};
#[cfg(target_os = "android")]
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use deskbolt_proto::{DEFAULT_CLIPBOARD_MAX_BYTES, MAX_CLIPBOARD_BYTES, SessionMode, VideoQualityMode};
use rusqlite::{Connection, OptionalExtension, params};

#[cfg(target_os = "android")]
static ANDROID_PHONEBOOK_DIR: OnceLock<PathBuf> = OnceLock::new();

#[cfg(target_os = "android")]
pub fn init_android_phonebook_dir(dir: PathBuf) {
    let _ = ANDROID_PHONEBOOK_DIR.set(dir);
}

/// Persistent SQLite address book (`%APPDATA%\DeskBolt\phonebook.db` on Windows, XDG/macOS/Android equivalents elsewhere).
pub struct PhonebookDb {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRow {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub sort_order: i64,
}

/// Protocol family stored per Address Book host entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i64)]
pub enum HostProtocol {
    DeskBolt = 0,
    Ssh = 1,
    Sftp = 2,
    Serial = 3,
}

impl HostProtocol {
    pub const ALL: [Self; 4] = [Self::DeskBolt, Self::Ssh, Self::Sftp, Self::Serial];

    pub const fn label(self) -> &'static str {
        match self {
            Self::DeskBolt => "DeskBolt",
            Self::Ssh => "SSH2",
            Self::Sftp => "SFTP",
            Self::Serial => "Serial",
        }
    }

    pub const fn from_int(v: i64) -> Self {
        match v {
            1 => Self::Ssh,
            2 => Self::Sftp,
            3 => Self::Serial,
            _ => Self::DeskBolt,
        }
    }

    pub const fn default_port(self) -> u16 {
        match self {
            Self::DeskBolt => 4899,
            Self::Ssh | Self::Sftp => 22,
            Self::Serial => 9600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRow {
    pub id: i64,
    pub uuid: String,
    pub folder_id: Option<i64>,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub default_mode: SessionMode,
    pub quality_mode: VideoQualityMode,
    pub pinned_fingerprint: Option<String>,
    pub last_connected: Option<i64>,
    pub mac_address: Option<String>,
    pub protocol_type: HostProtocol,
    pub ssh_key_path: Option<String>,
    pub saved_tunnels_json: Option<String>,
    pub oob_enabled: bool,
    pub oob_host: Option<String>,
    pub oob_port: u16,
    pub oob_tls: u16,
    pub oob_username: Option<String>,
    pub oob_password: Option<String>,
    pub custom_fps: Option<u8>,
    pub custom_quality: Option<VideoQualityMode>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HostInput {
    pub id: Option<i64>,
    pub folder_id: Option<i64>,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub default_mode: SessionMode,
    pub quality_mode: VideoQualityMode,
    pub mac_address: Option<String>,
    pub protocol_type: HostProtocol,
    pub ssh_key_path: Option<String>,
    pub saved_tunnels_json: Option<String>,
    pub oob_enabled: bool,
    pub oob_host: Option<String>,
    pub oob_port: u16,
    pub oob_tls: u16,
    pub oob_username: Option<String>,
    pub oob_password: Option<String>,
    pub custom_fps: Option<u8>,
    pub custom_quality: Option<VideoQualityMode>,
    pub notes: Option<String>,
}

impl Default for HostInput {
    fn default() -> Self {
        Self {
            id: None,
            folder_id: None,
            name: String::new(),
            host: String::new(),
            port: 4899,
            default_mode: SessionMode::FullControl,
            quality_mode: VideoQualityMode::Auto,
            mac_address: None,
            protocol_type: HostProtocol::DeskBolt,
            ssh_key_path: None,
            saved_tunnels_json: None,
            oob_enabled: false,
            oob_host: None,
            oob_port: 16992,
            oob_tls: 16993,
            oob_username: None,
            oob_password: None,
            custom_fps: None,
            custom_quality: None,
            notes: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleMode {
    Fit,
    OneToOne,
    Stretch,
}

impl ScaleMode {
    pub const ALL: [Self; 3] = [Self::Fit, Self::OneToOne, Self::Stretch];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fit => "fit",
            Self::OneToOne => "1to1",
            Self::Stretch => "stretch",
        }
    }
    pub fn from_key(s: &str) -> Self {
        match s {
            "1to1" => Self::OneToOne,
            "stretch" => Self::Stretch,
            _ => Self::Fit,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fit => "Fit Window",
            Self::OneToOne => "1:1 Pixel",
            Self::Stretch => "Stretch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureFilterMode {
    Linear,
    Nearest,
}

impl TextureFilterMode {
    pub const ALL: [Self; 2] = [Self::Linear, Self::Nearest];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Nearest => "nearest",
        }
    }
    pub fn from_key(s: &str) -> Self {
        if s == "nearest" { Self::Nearest } else { Self::Linear }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear (Smooth)",
            Self::Nearest => "Nearest (Sharp Pixels)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemKeysMode {
    Focused,
    Fullscreen,
    Never,
}

impl SystemKeysMode {
    pub const ALL: [Self; 3] = [Self::Focused, Self::Fullscreen, Self::Never];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Focused => "focused",
            Self::Fullscreen => "fullscreen",
            Self::Never => "never",
        }
    }
    pub fn from_key(s: &str) -> Self {
        match s {
            "fullscreen" => Self::Fullscreen,
            "never" => Self::Never,
            _ => Self::Focused,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::Focused => "When Session Focused",
            Self::Fullscreen => "Only in Fullscreen",
            Self::Never => "Never Capture",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultimonitorViewMode {
    Switch,
    SpanAll,
}

impl MultimonitorViewMode {
    pub const ALL: [Self; 2] = [Self::Switch, Self::SpanAll];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Switch => "switch",
            Self::SpanAll => "span",
        }
    }
    pub fn from_key(s: &str) -> Self {
        if s == "span" { Self::SpanAll } else { Self::Switch }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::Switch => "Switch Per Monitor",
            Self::SpanAll => "Span All Monitors",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeChoice {
    Dark,
    Light,
    System,
}

impl ThemeChoice {
    pub const ALL: [Self; 3] = [Self::Dark, Self::Light, Self::System];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::System => "system",
        }
    }
    pub fn from_key(s: &str) -> Self {
        match s {
            "light" => Self::Light,
            "system" => Self::System,
            _ => Self::Dark,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::System => "System",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewLayout {
    LargeIcons,
    List,
    Details,
}

impl ViewLayout {
    pub const ALL: [Self; 3] = [Self::LargeIcons, Self::List, Self::Details];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LargeIcons => "large_icons",
            Self::List => "list",
            Self::Details => "details",
        }
    }
    pub fn from_key(s: &str) -> Self {
        match s {
            "large_icons" => Self::LargeIcons,
            "list" => Self::List,
            _ => Self::Details,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::LargeIcons => "Large Icons",
            Self::List => "List",
            Self::Details => "Details",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupView {
    AddressBook,
    NetworkTools,
}

impl StartupView {
    pub const ALL: [Self; 2] = [Self::AddressBook, Self::NetworkTools];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AddressBook => "address_book",
            Self::NetworkTools => "net_tools",
        }
    }
    pub fn from_key(s: &str) -> Self {
        if s == "net_tools" { Self::NetworkTools } else { Self::AddressBook }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::AddressBook => "Address Book",
            Self::NetworkTools => "Network Scanner",
        }
    }
}

/// Persistent Viewer preferences stored in `phonebook.db` `settings` key-value table.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewerSettings {
    pub default_mode: SessionMode,
    pub default_quality: VideoQualityMode,
    pub color_depth: u8,
    pub connect_timeout_secs: u16,
    pub auto_reconnect_attempts: u8,
    pub auto_reconnect_delay_secs: u8,
    pub scale_mode: ScaleMode,
    pub texture_filter: TextureFilterMode,
    pub sys_keys_mode: SystemKeysMode,
    pub remote_cursor_sync: bool,
    pub clipboard_max_bytes: usize,
    pub clipboard_key_delay_ms: u16,
    pub hotkey_type_clipboard: String,
    pub multimonitor_mode: MultimonitorViewMode,
    pub mic_device: String,
    pub speaker_device: String,
    pub noise_gate: f32,
    pub ptt_hotkey: String,
    pub chat_sound: bool,
    pub auto_save_chat: bool,
    pub hotkey_fullscreen: String,
    pub hotkey_cad: String,
    pub hotkey_next_monitor: String,
    pub hotkey_screenshot: String,
    pub hotkey_mute_mic: String,
    pub theme: ThemeChoice,
    pub startup_view: StartupView,
    pub view_layout: ViewLayout,
    pub show_folder_tree: bool,
    pub show_toolbar: bool,
    pub show_status_bar: bool,
}

impl Default for ViewerSettings {
    fn default() -> Self {
        Self {
            default_mode: SessionMode::FullControl,
            default_quality: VideoQualityMode::Auto,
            color_depth: 32,
            connect_timeout_secs: 10,
            auto_reconnect_attempts: 3,
            auto_reconnect_delay_secs: 3,
            scale_mode: ScaleMode::Fit,
            texture_filter: TextureFilterMode::Linear,
            sys_keys_mode: SystemKeysMode::Focused,
            remote_cursor_sync: true,
            clipboard_max_bytes: DEFAULT_CLIPBOARD_MAX_BYTES,
            clipboard_key_delay_ms: 10,
            hotkey_type_clipboard: "Ctrl+Shift+V".into(),
            multimonitor_mode: MultimonitorViewMode::Switch,
            mic_device: "Default".into(),
            speaker_device: "Default".into(),
            noise_gate: 0.015,
            ptt_hotkey: "V".into(),
            chat_sound: true,
            auto_save_chat: false,
            hotkey_fullscreen: "F11".into(),
            hotkey_cad: "Ctrl+Alt+End".into(),
            hotkey_next_monitor: "Ctrl+Alt+M".into(),
            hotkey_screenshot: "F12".into(),
            hotkey_mute_mic: "Ctrl+Shift+M".into(),
            theme: ThemeChoice::Dark,
            startup_view: StartupView::AddressBook,
            view_layout: ViewLayout::Details,
            show_folder_tree: true,
            show_toolbar: true,
            show_status_bar: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSnippet {
    pub id: i64,
    pub name: String,
    pub command: String,
    pub category: String,
}

pub fn default_phonebook_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("DeskBolt").join("phonebook.db")
    }
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("deskbolt").join("phonebook.db")
    }
    #[cfg(target_os = "macos")]
    {
        let base = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("DeskBolt").join("phonebook.db")
    }
    #[cfg(target_os = "android")]
    {
        let base = ANDROID_PHONEBOOK_DIR
            .get()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("/data/local/tmp/deskbolt"));
        base.join("phonebook.db")
    }
}

impl PhonebookDb {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create phonebook directory failed: {e}"))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("open phonebook DB: {e}"))?;
        Self::init_conn(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| format!("open memory DB: {e}"))?;
        Self::init_conn(conn)
    }

    fn init_conn(conn: Connection) -> Result<Self, String> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(|e| format!("DB pragma init: {e}"))?;

        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                    version INTEGER PRIMARY KEY,
                    applied_at INTEGER NOT NULL
                );",
            )
            .map_err(|e| format!("migration table: {e}"))?;

        let current: Option<i64> = self
            .conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| r.get(0))
            .map_err(|e| format!("query schema version: {e}"))?;

        if current.unwrap_or(0) < 1 {
            let now = unix_now();
            self.conn
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS folders (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        parent_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
                        name TEXT NOT NULL CHECK(length(trim(name)) > 0),
                        sort_order INTEGER NOT NULL DEFAULT 0,
                        created_at INTEGER NOT NULL
                    );
                    CREATE TABLE IF NOT EXISTS hosts (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL,
                        name TEXT NOT NULL CHECK(length(trim(name)) > 0),
                        host TEXT NOT NULL CHECK(length(trim(host)) > 0),
                        port INTEGER NOT NULL DEFAULT 4899 CHECK(port BETWEEN 1 AND 65535),
                        default_mode INTEGER NOT NULL DEFAULT 0 CHECK(default_mode IN (0, 1, 2)),
                        quality_mode INTEGER NOT NULL DEFAULT 0 CHECK(quality_mode IN (0, 1, 2)),
                        pinned_fingerprint TEXT,
                        last_connected INTEGER,
                        created_at INTEGER NOT NULL,
                        updated_at INTEGER NOT NULL
                    );
                    CREATE TABLE IF NOT EXISTS app_settings (
                        key TEXT PRIMARY KEY,
                        value TEXT NOT NULL
                    );
                    CREATE INDEX IF NOT EXISTS idx_folders_parent ON folders(parent_id, sort_order, name);
                    CREATE INDEX IF NOT EXISTS idx_hosts_folder ON hosts(folder_id, name);
                    CREATE INDEX IF NOT EXISTS idx_hosts_search ON hosts(name, host);
                    CREATE INDEX IF NOT EXISTS idx_hosts_recent ON hosts(last_connected DESC);",
                )
                .map_err(|e| format!("apply v1 schema: {e}"))?;
            self.conn
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (1, ?1)",
                    params![now],
                )
                .map_err(|e| format!("record v1 migration: {e}"))?;
        }

        let current: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(version), 0) FROM schema_migrations", [], |r| r.get(0))
            .map_err(|e| format!("query schema version v2: {e}"))?;

        if current < 2 {
            let mut cols = std::collections::HashSet::new();
            {
                let mut stmt = self
                    .conn
                    .prepare("PRAGMA table_info(hosts)")
                    .map_err(|e| format!("inspect hosts columns: {e}"))?;
                let names = stmt
                    .query_map([], |r| r.get::<_, String>(1))
                    .map_err(|e| format!("read hosts columns: {e}"))?;
                for name in names {
                    cols.insert(name.map_err(|e| format!("column name: {e}"))?);
                }
            }
            for (col, ddl) in [
                ("mac_address", "ALTER TABLE hosts ADD COLUMN mac_address TEXT"),
                (
                    "protocol_type",
                    "ALTER TABLE hosts ADD COLUMN protocol_type INTEGER NOT NULL DEFAULT 0",
                ),
                ("ssh_key_path", "ALTER TABLE hosts ADD COLUMN ssh_key_path TEXT"),
                (
                    "saved_tunnels_json",
                    "ALTER TABLE hosts ADD COLUMN saved_tunnels_json TEXT",
                ),
            ] {
                if !cols.contains(col) {
                    self.conn
                        .execute_batch(ddl)
                        .map_err(|e| format!("migrate hosts.{col}: {e}"))?;
                }
            }

            self.conn
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS command_snippets (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        name TEXT NOT NULL CHECK(length(trim(name)) > 0),
                        command TEXT NOT NULL CHECK(length(trim(command)) > 0),
                        category TEXT NOT NULL DEFAULT 'General',
                        created_at INTEGER NOT NULL
                    );",
                )
                .map_err(|e| format!("create command_snippets: {e}"))?;

            let now = unix_now();
            self.conn
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (2, ?1)",
                    params![now],
                )
                .map_err(|e| format!("record v2 migration: {e}"))?;
        }

        let current_v3: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(version), 0) FROM schema_migrations", [], |r| r.get(0))
            .map_err(|e| format!("query schema version v3: {e}"))?;

        if current_v3 < 3 {
            self.conn
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS settings (
                        key TEXT PRIMARY KEY,
                        value TEXT NOT NULL
                    );",
                )
                .map_err(|e| format!("create settings table: {e}"))?;

            let mut cols = std::collections::HashSet::new();
            {
                let mut stmt = self
                    .conn
                    .prepare("PRAGMA table_info(hosts)")
                    .map_err(|e| format!("inspect hosts v3: {e}"))?;
                let names = stmt
                    .query_map([], |r| r.get::<_, String>(1))
                    .map_err(|e| format!("read hosts columns v3: {e}"))?;
                for name in names {
                    cols.insert(name.map_err(|e| format!("column name v3: {e}"))?);
                }
            }
            for (col, ddl) in [
                ("uuid", "ALTER TABLE hosts ADD COLUMN uuid TEXT"),
                ("oob_enabled", "ALTER TABLE hosts ADD COLUMN oob_enabled INTEGER NOT NULL DEFAULT 0"),
                ("oob_host", "ALTER TABLE hosts ADD COLUMN oob_host TEXT"),
                ("oob_port", "ALTER TABLE hosts ADD COLUMN oob_port INTEGER NOT NULL DEFAULT 16992"),
                ("oob_tls", "ALTER TABLE hosts ADD COLUMN oob_tls INTEGER NOT NULL DEFAULT 16993"),
                ("oob_username", "ALTER TABLE hosts ADD COLUMN oob_username TEXT"),
                ("oob_password_enc", "ALTER TABLE hosts ADD COLUMN oob_password_enc TEXT"),
                ("custom_fps", "ALTER TABLE hosts ADD COLUMN custom_fps INTEGER"),
                ("custom_quality", "ALTER TABLE hosts ADD COLUMN custom_quality INTEGER"),
                ("notes", "ALTER TABLE hosts ADD COLUMN notes TEXT"),
            ] {
                if !cols.contains(col) {
                    self.conn
                        .execute_batch(ddl)
                        .map_err(|e| format!("migrate hosts.{col}: {e}"))?;
                }
            }

            // Backfill existing hosts without a UUID
            let missing_ids: Vec<i64> = {
                let mut st = self
                    .conn
                    .prepare("SELECT id FROM hosts WHERE uuid IS NULL OR trim(uuid) = ''")
                    .map_err(|e| format!("scan missing uuid: {e}"))?;
                let rows = st
                    .query_map([], |r| r.get(0))
                    .map_err(|e| format!("read missing uuid: {e}"))?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("collect missing uuid: {e}"))?
            };
            for id in missing_ids {
                self.conn
                    .execute(
                        "UPDATE hosts SET uuid = ?1 WHERE id = ?2",
                        params![generate_uuid_v4(), id],
                    )
                    .map_err(|e| format!("backfill uuid: {e}"))?;
            }
            let _ = self.conn.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_hosts_uuid ON hosts(uuid);",
            );

            let now = unix_now();
            self.conn
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (3, ?1)",
                    params![now],
                )
                .map_err(|e| format!("record v3 migration: {e}"))?;
        }
        Ok(())
    }

    pub fn list_folders(&self) -> Result<Vec<FolderRow>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, parent_id, name, sort_order FROM folders ORDER BY sort_order, name")
            .map_err(|e| format!("prepare list_folders: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(FolderRow {
                    id: row.get(0)?,
                    parent_id: row.get(1)?,
                    name: row.get(2)?,
                    sort_order: row.get(3)?,
                })
            })
            .map_err(|e| format!("query folders: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("collect folders: {e}"))
    }

    pub fn create_folder(&self, parent_id: Option<i64>, name: &str) -> Result<i64, String> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("Folder name cannot be empty.".into());
        }
        self.conn
            .execute(
                "INSERT INTO folders (parent_id, name, sort_order, created_at) VALUES (?1, ?2, 0, ?3)",
                params![parent_id, trimmed, unix_now()],
            )
            .map_err(|e| format!("create folder: {e}"))?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rename_folder(&self, id: i64, name: &str) -> Result<(), String> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("Folder name cannot be empty.".into());
        }
        self.conn
            .execute("UPDATE folders SET name = ?1 WHERE id = ?2", params![trimmed, id])
            .map_err(|e| format!("rename folder: {e}"))?;
        Ok(())
    }

    pub fn delete_folder(&self, id: i64) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM folders WHERE id = ?1", params![id])
            .map_err(|e| format!("delete folder: {e}"))?;
        Ok(())
    }

    pub fn list_hosts(
        &self,
        folder_filter: Option<i64>,
        search_query: &str,
    ) -> Result<Vec<HostRow>, String> {
        let q = search_query.trim();
        let like = format!("%{q}%");

        let sql = if q.is_empty() {
            if folder_filter.is_some() {
                "SELECT id, uuid, folder_id, name, host, port, default_mode, quality_mode, pinned_fingerprint, last_connected, mac_address, protocol_type, ssh_key_path, saved_tunnels_json, oob_enabled, oob_host, oob_port, oob_tls, oob_username, oob_password_enc, custom_fps, custom_quality, notes \
                 FROM hosts WHERE folder_id = ?1 ORDER BY name COLLATE NOCASE"
            } else {
                "SELECT id, uuid, folder_id, name, host, port, default_mode, quality_mode, pinned_fingerprint, last_connected, mac_address, protocol_type, ssh_key_path, saved_tunnels_json, oob_enabled, oob_host, oob_port, oob_tls, oob_username, oob_password_enc, custom_fps, custom_quality, notes \
                 FROM hosts ORDER BY name COLLATE NOCASE"
            }
        } else if folder_filter.is_some() {
            "SELECT id, uuid, folder_id, name, host, port, default_mode, quality_mode, pinned_fingerprint, last_connected, mac_address, protocol_type, ssh_key_path, saved_tunnels_json, oob_enabled, oob_host, oob_port, oob_tls, oob_username, oob_password_enc, custom_fps, custom_quality, notes \
             FROM hosts WHERE folder_id = ?1 AND (name LIKE ?2 COLLATE NOCASE OR host LIKE ?2 COLLATE NOCASE OR COALESCE(mac_address, '') LIKE ?2 COLLATE NOCASE) \
             ORDER BY name COLLATE NOCASE"
        } else {
            "SELECT id, uuid, folder_id, name, host, port, default_mode, quality_mode, pinned_fingerprint, last_connected, mac_address, protocol_type, ssh_key_path, saved_tunnels_json, oob_enabled, oob_host, oob_port, oob_tls, oob_username, oob_password_enc, custom_fps, custom_quality, notes \
             FROM hosts WHERE (name LIKE ?1 COLLATE NOCASE OR host LIKE ?1 COLLATE NOCASE OR COALESCE(mac_address, '') LIKE ?1 COLLATE NOCASE) \
             ORDER BY name COLLATE NOCASE"
        };

        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| format!("prepare list_hosts: {e}"))?;

        let rows = match (folder_filter, q.is_empty()) {
            (Some(fid), true) => stmt.query_map(params![fid], map_host_row),
            (None, true) => stmt.query_map([], map_host_row),
            (Some(fid), false) => stmt.query_map(params![fid, like], map_host_row),
            (None, false) => stmt.query_map(params![like], map_host_row),
        }
        .map_err(|e| format!("query hosts: {e}"))?;

        let mut list = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("collect hosts: {e}"))?;
        let key = self.ensure_oob_seal_key();
        for row in &mut list {
            if let Some(enc) = row.oob_password.take() {
                row.oob_password = unseal_oob_secret(&key, &enc);
            }
        }
        Ok(list)
    }

    pub fn find_host_by_id_or_uuid(&self, ident: &str) -> Result<Option<HostRow>, String> {
        let trimmed = ident.trim();
        let all = self.list_hosts(None, "")?;
        if let Ok(num_id) = trimmed.parse::<i64>()
            && let Some(h) = all.iter().find(|h| h.id == num_id)
        {
            return Ok(Some(h.clone()));
        }
        Ok(all
            .into_iter()
            .find(|h| h.uuid.eq_ignore_ascii_case(trimmed) || h.name.eq_ignore_ascii_case(trimmed)))
    }

    pub fn rename_host(&self, id: i64, new_name: &str) -> Result<(), String> {
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            return Err("Host name cannot be empty.".into());
        }
        self.conn
            .execute(
                "UPDATE hosts SET name = ?1, updated_at = ?2 WHERE id = ?3",
                params![trimmed, unix_now(), id],
            )
            .map_err(|e| format!("rename host: {e}"))?;
        Ok(())
    }

    pub fn upsert_host(&self, input: &HostInput) -> Result<i64, String> {
        let name = input.name.trim();
        let host = input.host.trim();
        if name.is_empty() || host.is_empty() {
            return Err("Connection name and host address are required.".into());
        }
        if input.port == 0 {
            return Err("Port must be between 1 and 65535.".into());
        }
        let now = unix_now();
        let dm = input.default_mode as i64;
        let qm = input.quality_mode as i64;
        let pt = input.protocol_type as i64;
        let mac = clean_opt(input.mac_address.as_deref());
        let key_path = clean_opt(input.ssh_key_path.as_deref());
        let tunnels = clean_opt(input.saved_tunnels_json.as_deref());
        let oob_host = clean_opt(input.oob_host.as_deref());
        let oob_user = clean_opt(input.oob_username.as_deref());
        let seal_key = self.ensure_oob_seal_key();
        let oob_pw_enc = clean_opt(input.oob_password.as_deref()).map(|p| seal_oob_secret(&seal_key, &p));
        let custom_fps = input.custom_fps.map(|f| f.clamp(15, 120) as i64);
        let custom_quality = input.custom_quality.map(|q| q as i64);
        let notes = clean_opt(input.notes.as_deref());
        let oob_port = if input.oob_port == 0 { 16992 } else { input.oob_port };

        if let Some(id) = input.id {
            self.conn
                .execute(
                    "UPDATE hosts SET folder_id = ?1, name = ?2, host = ?3, port = ?4, default_mode = ?5, quality_mode = ?6, \
                     mac_address = ?7, protocol_type = ?8, ssh_key_path = ?9, saved_tunnels_json = ?10, \
                     oob_enabled = ?11, oob_host = ?12, oob_port = ?13, oob_tls = ?14, oob_username = ?15, \
                     oob_password_enc = ?16, custom_fps = ?17, custom_quality = ?18, notes = ?19, updated_at = ?20 WHERE id = ?21",
                    params![
                        input.folder_id,
                        name,
                        host,
                        input.port as i64,
                        dm,
                        qm,
                        mac,
                        pt,
                        key_path,
                        tunnels,
                        i64::from(input.oob_enabled),
                        oob_host,
                        oob_port as i64,
                        input.oob_tls as i64,
                        oob_user,
                        oob_pw_enc,
                        custom_fps,
                        custom_quality,
                        notes,
                        now,
                        id
                    ],
                )
                .map_err(|e| format!("update host: {e}"))?;
            Ok(id)
        } else {
            let uuid = generate_uuid_v4();
            self.conn
                .execute(
                    "INSERT INTO hosts (uuid, folder_id, name, host, port, default_mode, quality_mode, mac_address, protocol_type, ssh_key_path, saved_tunnels_json, oob_enabled, oob_host, oob_port, oob_tls, oob_username, oob_password_enc, custom_fps, custom_quality, notes, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?21)",
                    params![
                        uuid,
                        input.folder_id,
                        name,
                        host,
                        input.port as i64,
                        dm,
                        qm,
                        mac,
                        pt,
                        key_path,
                        tunnels,
                        i64::from(input.oob_enabled),
                        oob_host,
                        oob_port as i64,
                        input.oob_tls as i64,
                        oob_user,
                        oob_pw_enc,
                        custom_fps,
                        custom_quality,
                        notes,
                        now
                    ],
                )
                .map_err(|e| format!("insert host: {e}"))?;
            Ok(self.conn.last_insert_rowid())
        }
    }

    pub fn load_settings(&self) -> ViewerSettings {
        let mut s = ViewerSettings::default();
        let Ok(mut st) = self.conn.prepare("SELECT key, value FROM settings") else {
            return s;
        };
        let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) else {
            return s;
        };
        for row in rows.flatten() {
            let (k, v) = row;
            match k.as_str() {
                "default_mode" => s.default_mode = mode_from_int(v.parse().unwrap_or(0)),
                "default_quality" => s.default_quality = quality_from_int(v.parse().unwrap_or(0)),
                "color_depth" => s.color_depth = if v == "16" { 16 } else { 32 },
                "connect_timeout_secs" => s.connect_timeout_secs = v.parse().unwrap_or(10).clamp(2, 120),
                "auto_reconnect_attempts" => s.auto_reconnect_attempts = v.parse().unwrap_or(3).min(20),
                "auto_reconnect_delay_secs" => s.auto_reconnect_delay_secs = v.parse().unwrap_or(3).clamp(1, 60),
                "scale_mode" => s.scale_mode = ScaleMode::from_key(&v),
                "texture_filter" => s.texture_filter = TextureFilterMode::from_key(&v),
                "sys_keys_mode" => s.sys_keys_mode = SystemKeysMode::from_key(&v),
                "remote_cursor_sync" => s.remote_cursor_sync = v != "0",
                "clipboard_max_bytes" => {
                    s.clipboard_max_bytes = v.parse().unwrap_or(DEFAULT_CLIPBOARD_MAX_BYTES)
                        .min(MAX_CLIPBOARD_BYTES);
                }
                "clipboard_key_delay_ms" => s.clipboard_key_delay_ms = v.parse().unwrap_or(10).clamp(1, 1000),
                "hotkey_type_clipboard" => s.hotkey_type_clipboard = v,
                "multimonitor_mode" => s.multimonitor_mode = MultimonitorViewMode::from_key(&v),
                "mic_device" => s.mic_device = v,
                "speaker_device" => s.speaker_device = v,
                "noise_gate" => s.noise_gate = v.parse::<f32>().unwrap_or(0.015).clamp(0.0, 0.3),
                "ptt_hotkey" => s.ptt_hotkey = v,
                "chat_sound" => s.chat_sound = v != "0",
                "auto_save_chat" => s.auto_save_chat = v == "1",
                "hotkey_fullscreen" => s.hotkey_fullscreen = v,
                "hotkey_cad" => s.hotkey_cad = v,
                "hotkey_next_monitor" => s.hotkey_next_monitor = v,
                "hotkey_screenshot" => s.hotkey_screenshot = v,
                "hotkey_mute_mic" => s.hotkey_mute_mic = v,
                "theme" => s.theme = ThemeChoice::from_key(&v),
                "startup_view" => s.startup_view = StartupView::from_key(&v),
                "view_layout" => s.view_layout = ViewLayout::from_key(&v),
                "show_folder_tree" => s.show_folder_tree = v != "0",
                "show_toolbar" => s.show_toolbar = v != "0",
                "show_status_bar" => s.show_status_bar = v != "0",
                _ => {}
            }
        }
        s
    }

    pub fn save_settings(&self, s: &ViewerSettings) -> Result<(), String> {
        let pairs = [
            ("default_mode", (s.default_mode as i64).to_string()),
            ("default_quality", (s.default_quality as i64).to_string()),
            ("color_depth", s.color_depth.to_string()),
            ("connect_timeout_secs", s.connect_timeout_secs.to_string()),
            ("auto_reconnect_attempts", s.auto_reconnect_attempts.to_string()),
            ("auto_reconnect_delay_secs", s.auto_reconnect_delay_secs.to_string()),
            ("scale_mode", s.scale_mode.as_str().into()),
            ("texture_filter", s.texture_filter.as_str().into()),
            ("sys_keys_mode", s.sys_keys_mode.as_str().into()),
            ("remote_cursor_sync", i32::from(s.remote_cursor_sync).to_string()),
            ("clipboard_max_bytes", s.clipboard_max_bytes.to_string()),
            ("clipboard_key_delay_ms", s.clipboard_key_delay_ms.to_string()),
            ("hotkey_type_clipboard", s.hotkey_type_clipboard.clone()),
            ("multimonitor_mode", s.multimonitor_mode.as_str().into()),
            ("mic_device", s.mic_device.clone()),
            ("speaker_device", s.speaker_device.clone()),
            ("noise_gate", format!("{:.4}", s.noise_gate)),
            ("ptt_hotkey", s.ptt_hotkey.clone()),
            ("chat_sound", i32::from(s.chat_sound).to_string()),
            ("auto_save_chat", i32::from(s.auto_save_chat).to_string()),
            ("hotkey_fullscreen", s.hotkey_fullscreen.clone()),
            ("hotkey_cad", s.hotkey_cad.clone()),
            ("hotkey_next_monitor", s.hotkey_next_monitor.clone()),
            ("hotkey_screenshot", s.hotkey_screenshot.clone()),
            ("hotkey_mute_mic", s.hotkey_mute_mic.clone()),
            ("theme", s.theme.as_str().into()),
            ("startup_view", s.startup_view.as_str().into()),
            ("view_layout", s.view_layout.as_str().into()),
            ("show_folder_tree", i32::from(s.show_folder_tree).to_string()),
            ("show_toolbar", i32::from(s.show_toolbar).to_string()),
            ("show_status_bar", i32::from(s.show_status_bar).to_string()),
        ];
        for (k, v) in pairs {
            self.conn
                .execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![k, v],
                )
                .map_err(|e| format!("save setting {k}: {e}"))?;
        }
        Ok(())
    }

    fn ensure_oob_seal_key(&self) -> [u8; 32] {
        let existing: Option<String> = self
            .conn
            .query_row("SELECT value FROM settings WHERE key = '_oob_key'", [], |r| r.get(0))
            .optional()
            .unwrap_or(None);
        if let Some(hex) = existing
            && let Some(bytes) = decode_hex_fixed(&hex)
        {
            return bytes;
        }
        let key = deskbolt_proto::random_bytes::<32>().unwrap_or([0x5Au8; 32]);
        let hex = encode_hex(&key);
        let _ = self.conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES ('_oob_key', ?1)",
            params![hex],
        );
        key
    }

    pub fn delete_host(&self, id: i64) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM hosts WHERE id = ?1", params![id])
            .map_err(|e| format!("delete host: {e}"))?;
        Ok(())
    }

    pub fn find_pinned_by_endpoint(&self, host: &str, port: u16) -> Result<Option<(i64, Option<String>)>, String> {
        self.conn
            .query_row(
                "SELECT id, pinned_fingerprint FROM hosts WHERE lower(host) = lower(?1) AND port = ?2 LIMIT 1",
                params![host.trim(), port as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| format!("lookup host pin: {e}"))
    }

    pub fn set_pinned_fingerprint(&self, host_id: i64, fp: Option<&str>) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE hosts SET pinned_fingerprint = ?1, updated_at = ?2 WHERE id = ?3",
                params![fp, unix_now(), host_id],
            )
            .map_err(|e| format!("update pinned fingerprint: {e}"))?;
        Ok(())
    }

    pub fn touch_last_connected(&self, host_id: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE hosts SET last_connected = ?1 WHERE id = ?2",
                params![unix_now(), host_id],
            )
            .map_err(|e| format!("update last_connected: {e}"))?;
        Ok(())
    }

    pub fn list_snippets(&self) -> Result<Vec<CommandSnippet>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, command, category FROM command_snippets ORDER BY category COLLATE NOCASE, name COLLATE NOCASE")
            .map_err(|e| format!("prepare list_snippets: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(CommandSnippet {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    command: row.get(2)?,
                    category: row.get(3)?,
                })
            })
            .map_err(|e| format!("query snippets: {e}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("collect snippets: {e}"))
    }

    pub fn upsert_snippet(
        &self,
        id: Option<i64>,
        name: &str,
        command: &str,
        category: &str,
    ) -> Result<i64, String> {
        let name = name.trim();
        let command = command.trim();
        let category = category.trim();
        if name.is_empty() || command.is_empty() {
            return Err("Snippet name and command are required.".into());
        }
        let cat = if category.is_empty() { "General" } else { category };
        if let Some(sid) = id {
            self.conn
                .execute(
                    "UPDATE command_snippets SET name = ?1, command = ?2, category = ?3 WHERE id = ?4",
                    params![name, command, cat, sid],
                )
                .map_err(|e| format!("update snippet: {e}"))?;
            Ok(sid)
        } else {
            self.conn
                .execute(
                    "INSERT INTO command_snippets (name, command, category, created_at) VALUES (?1, ?2, ?3, ?4)",
                    params![name, command, cat, unix_now()],
                )
                .map_err(|e| format!("insert snippet: {e}"))?;
            Ok(self.conn.last_insert_rowid())
        }
    }

    pub fn delete_snippet(&self, id: i64) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM command_snippets WHERE id = ?1", params![id])
            .map_err(|e| format!("delete snippet: {e}"))?;
        Ok(())
    }
}

fn clean_opt(v: Option<&str>) -> Option<String> {
    v.map(str::trim).filter(|s| !s.is_empty()).map(ToString::to_string)
}

fn mode_from_int(v: i64) -> SessionMode {
    match v {
        1 => SessionMode::ViewOnly,
        2 => SessionMode::FileTransfer,
        _ => SessionMode::FullControl,
    }
}

fn quality_from_int(v: i64) -> VideoQualityMode {
    match v {
        1 => VideoQualityMode::CrispText,
        2 => VideoQualityMode::SmoothVideo,
        _ => VideoQualityMode::Auto,
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn map_host_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HostRow> {
    let port_i: i64 = row.get(5)?;
    let dm: i64 = row.get(6)?;
    let qm: i64 = row.get(7)?;
    let pt: i64 = row.get(11)?;
    let oob_en: i64 = row.get(14)?;
    let oob_port: i64 = row.get(16)?;
    let oob_tls: i64 = row.get(17)?;
    let cfps: Option<i64> = row.get(20)?;
    let cq: Option<i64> = row.get(21)?;
    Ok(HostRow {
        id: row.get(0)?,
        uuid: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        folder_id: row.get(2)?,
        name: row.get(3)?,
        host: row.get(4)?,
        port: port_i.clamp(1, 65535) as u16,
        default_mode: mode_from_int(dm),
        quality_mode: quality_from_int(qm),
        pinned_fingerprint: row.get(8)?,
        last_connected: row.get(9)?,
        mac_address: row.get(10)?,
        protocol_type: HostProtocol::from_int(pt),
        ssh_key_path: row.get(12)?,
        saved_tunnels_json: row.get(13)?,
        oob_enabled: oob_en != 0,
        oob_host: row.get(15)?,
        oob_port: oob_port.clamp(1, 65535) as u16,
        oob_tls: oob_tls.clamp(0, 65535) as u16,
        oob_username: row.get(18)?,
        oob_password: row.get(19)?,
        custom_fps: cfps.map(|f| f.clamp(15, 120) as u8),
        custom_quality: cq.map(quality_from_int),
        notes: row.get(22)?,
    })
}

fn generate_uuid_v4() -> String {
    let mut b = deskbolt_proto::random_bytes::<16>().unwrap_or([0x42u8; 16]);
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn decode_hex_vec(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        out.push(u8::from_str_radix(&hex[i..i + 2], 16).ok()?);
    }
    Some(out)
}

fn decode_hex_fixed(hex: &str) -> Option<[u8; 32]> {
    let v = decode_hex_vec(hex)?;
    v.try_into().ok()
}

fn seal_oob_secret(key: &[u8; 32], plain: &str) -> String {
    let nonce = deskbolt_proto::random_bytes::<16>().unwrap_or([0x13u8; 16]);
    let stream = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), &nonce);
    let mut masked = plain.as_bytes().to_vec();
    for (idx, b) in masked.iter_mut().enumerate() {
        *b ^= stream.as_ref()[idx % 32];
    }
    format!("v1:{}:{}", encode_hex(&nonce), encode_hex(&masked))
}

fn unseal_oob_secret(key: &[u8; 32], encoded: &str) -> Option<String> {
    let rest = encoded.strip_prefix("v1:")?;
    let (nonce_hex, cipher_hex) = rest.split_once(':')?;
    let nonce = decode_hex_vec(nonce_hex)?;
    let mut masked = decode_hex_vec(cipher_hex)?;
    let stream = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), &nonce);
    for (idx, b) in masked.iter_mut().enumerate() {
        *b ^= stream.as_ref()[idx % 32];
    }
    String::from_utf8(masked).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phonebook_folders_hosts_cascade_pinning_and_search() {
        let db = PhonebookDb::open_in_memory().unwrap();
        let prod = db.create_folder(None, "Production").unwrap();
        let branch = db.create_folder(Some(prod), "Branch").unwrap();

        let h1 = db
            .upsert_host(&HostInput {
                id: None,
                folder_id: Some(prod),
                name: "DC-Primary".into(),
                host: "10.0.0.10".into(),
                port: 4899,
                default_mode: SessionMode::FullControl,
                quality_mode: VideoQualityMode::Auto,
                mac_address: Some("00:11:22:33:44:55".into()),
                protocol_type: HostProtocol::DeskBolt,
                ssh_key_path: None,
                saved_tunnels_json: Some("127.0.0.1:8080->10.0.0.10:80".into()),
                ..Default::default()
            })
            .unwrap();
        let h2 = db
            .upsert_host(&HostInput {
                id: None,
                folder_id: Some(branch),
                name: "POS-Register-01".into(),
                host: "10.0.5.41".into(),
                port: 22,
                default_mode: SessionMode::FullControl,
                quality_mode: VideoQualityMode::Auto,
                mac_address: None,
                protocol_type: HostProtocol::Ssh,
                ssh_key_path: Some("~/.ssh/id_ed25519".into()),
                saved_tunnels_json: None,
                ..Default::default()
            })
            .unwrap();

        db.set_pinned_fingerprint(h1, Some("AA:BB")).unwrap();
        db.touch_last_connected(h1).unwrap();
        let found = db.find_pinned_by_endpoint("10.0.0.10", 4899).unwrap().unwrap();
        assert_eq!(found, (h1, Some("AA:BB".into())));

        // Live search (including MAC search)
        assert_eq!(db.list_hosts(None, "pos").unwrap().len(), 1);
        assert_eq!(db.list_hosts(None, "33:44").unwrap().len(), 1);
        assert_eq!(db.list_hosts(Some(prod), "dc").unwrap().len(), 1);
        assert_eq!(db.list_hosts(Some(prod), "pos").unwrap().len(), 0);

        // Snippets CRUD
        let sid = db.upsert_snippet(None, "Disk Free", "df -h", "Linux").unwrap();
        let list = db.list_snippets().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].command, "df -h");
        db.delete_snippet(sid).unwrap();
        assert!(db.list_snippets().unwrap().is_empty());

        // Cascade delete folder: subfolder deleted, host folder_id becomes NULL
        db.delete_folder(prod).unwrap();
        assert!(db.list_folders().unwrap().is_empty());
        let remaining = db.list_hosts(None, "").unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().any(|h| h.id == h2 && h.folder_id.is_none() && h.protocol_type == HostProtocol::Ssh));

        db.set_pinned_fingerprint(h1, None).unwrap();
        assert_eq!(db.find_pinned_by_endpoint("10.0.0.10", 4899).unwrap().unwrap().1, None);
    }

    #[test]
    fn migrates_phase3_v1_schema_preserving_hosts_and_snippets() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
             INSERT INTO schema_migrations (version, applied_at) VALUES (1, 100);
             CREATE TABLE folders (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 parent_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
                 name TEXT NOT NULL,
                 sort_order INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE hosts (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL,
                 name TEXT NOT NULL,
                 host TEXT NOT NULL,
                 port INTEGER NOT NULL DEFAULT 4899,
                 default_mode INTEGER NOT NULL DEFAULT 0,
                 quality_mode INTEGER NOT NULL DEFAULT 0,
                 pinned_fingerprint TEXT,
                 last_connected INTEGER,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             INSERT INTO hosts (name, host, port, default_mode, quality_mode, pinned_fingerprint, created_at, updated_at)
             VALUES ('LegacyHost', '192.168.10.20', 4899, 0, 1, 'AA:11:22', 100, 100);",
        )
        .unwrap();

        let db = PhonebookDb::init_conn(conn).unwrap();
        let ver: i64 = db
            .conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 3);

        let hosts = db.list_hosts(None, "").unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "LegacyHost");
        assert!(!hosts[0].uuid.is_empty());
        assert_eq!(hosts[0].oob_port, 16992);
        assert_eq!(hosts[0].oob_tls, 16993);
        assert_eq!(hosts[0].protocol_type, HostProtocol::DeskBolt);
        assert_eq!(hosts[0].pinned_fingerprint.as_deref(), Some("AA:11:22"));
        assert!(hosts[0].mac_address.is_none());

        let sid = db.upsert_snippet(None, "Services", "sc query", "Windows").unwrap();
        assert!(sid > 0);
        assert_eq!(db.list_snippets().unwrap().len(), 1);
    }

    #[test]
    fn migrates_phase7_v2_schema_to_v3_with_settings_and_oob() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
             INSERT INTO schema_migrations (version, applied_at) VALUES (1, 100);
             INSERT INTO schema_migrations (version, applied_at) VALUES (2, 100);
             CREATE TABLE folders (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 parent_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
                 name TEXT NOT NULL,
                 sort_order INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE hosts (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 folder_id INTEGER REFERENCES folders(id) ON DELETE SET NULL,
                 name TEXT NOT NULL,
                 host TEXT NOT NULL,
                 port INTEGER NOT NULL DEFAULT 4899,
                 default_mode INTEGER NOT NULL DEFAULT 0,
                 quality_mode INTEGER NOT NULL DEFAULT 0,
                 pinned_fingerprint TEXT,
                 last_connected INTEGER,
                 mac_address TEXT,
                 protocol_type INTEGER NOT NULL DEFAULT 0,
                 ssh_key_path TEXT,
                 saved_tunnels_json TEXT,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE TABLE command_snippets (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 name TEXT NOT NULL,
                 command TEXT NOT NULL,
                 category TEXT NOT NULL DEFAULT 'General',
                 created_at INTEGER NOT NULL
             );
             INSERT INTO hosts (name, host, port, mac_address, protocol_type, created_at, updated_at)
             VALUES ('RackNode-01', '10.1.2.3', 4899, 'AA:BB:CC:DD:EE:01', 0, 100, 100);",
        )
        .unwrap();

        let db = PhonebookDb::init_conn(conn).unwrap();
        let ver: i64 = db
            .conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 3);

        let hosts = db.list_hosts(None, "").unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "RackNode-01");
        assert_eq!(hosts[0].oob_port, 16992);
        assert_eq!(hosts[0].oob_tls, 16993);

        // Save OOB settings, notes, custom FPS & verify encrypted credential roundtrip
        let updated_id = db
            .upsert_host(&HostInput {
                id: Some(hosts[0].id),
                folder_id: None,
                name: "RackNode-01".into(),
                host: "10.1.2.3".into(),
                port: 4899,
                default_mode: SessionMode::FullControl,
                quality_mode: VideoQualityMode::Auto,
                mac_address: Some("AA:BB:CC:DD:EE:01".into()),
                protocol_type: HostProtocol::DeskBolt,
                ssh_key_path: None,
                saved_tunnels_json: None,
                oob_enabled: true,
                oob_host: Some("10.1.2.9".into()),
                oob_port: 16992,
                oob_tls: 16993,
                oob_username: Some("admin".into()),
                oob_password: Some("OobSecret!26".into()),
                custom_fps: Some(90),
                custom_quality: Some(VideoQualityMode::SmoothVideo),
                notes: Some("Primary rack hypervisor".into()),
            })
            .unwrap();
        let by_uuid = db.find_host_by_id_or_uuid(&hosts[0].uuid).unwrap().unwrap();
        assert_eq!(by_uuid.id, updated_id);
        assert!(by_uuid.oob_enabled);
        assert_eq!(by_uuid.oob_password.as_deref(), Some("OobSecret!26"));
        assert_eq!(by_uuid.custom_fps, Some(90));
        assert_eq!(by_uuid.custom_quality, Some(VideoQualityMode::SmoothVideo));
        assert_eq!(by_uuid.notes.as_deref(), Some("Primary rack hypervisor"));

        // ViewerSettings roundtrip
        let mut settings = db.load_settings();
        settings.scale_mode = ScaleMode::OneToOne;
        settings.texture_filter = TextureFilterMode::Nearest;
        settings.theme = ThemeChoice::Light;
        settings.auto_save_chat = true;
        settings.noise_gate = 0.04;
        settings.hotkey_fullscreen = "F10".into();
        settings.clipboard_max_bytes = 42_000;
        settings.clipboard_key_delay_ms = 25;
        settings.hotkey_type_clipboard = "Alt+F9".into();
        db.save_settings(&settings).unwrap();
        let reloaded = db.load_settings();
        assert_eq!(reloaded.scale_mode, ScaleMode::OneToOne);
        assert_eq!(reloaded.texture_filter, TextureFilterMode::Nearest);
        assert_eq!(reloaded.theme, ThemeChoice::Light);
        assert!(reloaded.auto_save_chat);
        assert!((reloaded.noise_gate - 0.04).abs() < 0.001);
        assert_eq!(reloaded.hotkey_fullscreen, "F10");
        assert_eq!(reloaded.clipboard_max_bytes, 42_000);
        assert_eq!(reloaded.clipboard_key_delay_ms, 25);
        assert_eq!(reloaded.hotkey_type_clipboard, "Alt+F9");
    }
}
