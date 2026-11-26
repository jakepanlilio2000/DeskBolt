# DeskBolt Implementation Plan, Locked Manifest & Audit Checklists

**Document ID:** `SPEC-PLAN-005`  
**Toolchain:** Rust `edition = "2024"`, Target `x86_64-pc-windows-msvc`  
**License:** MPL-2.0 (`StackSync Solutions` / `jakepanlilio2000`)

---

## 1. Locked Workspace `Cargo.toml` Dependency Manifest

All dependency versions below are locked for deterministic builds across Phases 1–4.

```toml
[workspace]
resolver = "3"
members = [
    "crates/deskbolt-proto",
    "crates/deskbolt-server",
    "crates/deskbolt-viewer",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MPL-2.0"
authors = ["StackSync Solutions <jakepanlilio2000>"]
rust-version = "1.85"

[workspace.dependencies]
# Async Runtime & Framing
tokio = { version = "1.54.1", features = ["full"] }

# Windows APIs & SCM Service
windows = { version = "0.62.2", features = [
    "Win32_Foundation",
    "Win32_Security",
    "Win32_Security_Authorization",
    "Win32_Security_Cryptography",
    "Win32_Storage_FileSystem",
    "Win32_System_DataExchange",
    "Win32_System_Environment",
    "Win32_System_LibraryLoader",
    "Win32_System_Memory",
    "Win32_System_Pipes",
    "Win32_System_RemoteDesktop",
    "Win32_System_StationsAndDesktops",
    "Win32_System_Threading",
    "Win32_UI_Input_KeyboardAndMouse",
    "Win32_UI_WindowsAndMessaging",
    "Win32_Graphics_Gdi",
] }
windows-service = "0.8.1"

# Screen Capture & Video / Tile Codecs
dxgi-capture-rs = "1.2.2"
openh264 = "0.9.10"
zstd = "0.14.4"

# Native GUI & Local Storage
eframe = "0.37.0"
egui = "0.37.0"
rusqlite = { version = "0.32", features = ["bundled"] }

# TLS 1.3 & Cryptography
rcgen = "0.14.10"
tokio-rustls = "0.26"
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
argon2 = "0.5"
ring = "0.17"

# Serialization, Errors & Diagnostics
serde = { version = "1.0", features = ["derive"] }
bincode = "1.3"
thiserror = "2.0"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["fmt", "env-filter"] }

[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
strip = "symbols"
```

*(Note on `rustls` feature flags: keep `ring` and `std` enabled; protocol config explicitly restricts versions to `[&rustls::version::TLS13]` at runtime. Note on static linkage: `.cargo/config.toml` targets `x86_64-pc-windows-msvc`; verify `openh264` & `rusqlite` `bundled` C/C++ builds compile cleanly under MSVC `+crt-static` or standard MSVC toolchain with zero non-OS runtime DLLs.)*

---

## 2. Four-Phase Execution Roadmap

Every phase enforces these mandatory verification gates:
1. `cargo check --workspace` passes with **0 errors and 0 warnings**.
2. `cargo test --workspace` passes with **0 failures and 0 warnings**.
3. `cargo clippy --workspace --all-targets -- -D warnings` passes cleanly.

---

### Phase 1: Workspace Skeleton, Wire Protocol, TLS 1.3 TOFU & Crypto (`deskbolt-proto`)

**Scope:**
- Initialize root `Cargo.toml`, `LICENSE` (MPL-2.0), `.cargo/config.toml`, and all three crate manifests (`deskbolt-proto`, plus minimal compiling binary entrypoints in `deskbolt-server` and `deskbolt-viewer`).
- Implement `crates/deskbolt-proto`:
  - `frame.rs`: async/sync reader & writer for `[u32 BE length][u8 channel_id][u8 flags][payload]`, `16 MiB` limit, flag masks (`FLAG_COMPRESSED`, `FLAG_KEYFRAME`, `FLAG_END_OF_STREAM`, `FLAG_URGENT`), bounded `bincode 1.3` helpers.
  - `messages.rs`: `ControlMsg`, `VideoMsg`, `InputMsg`, `ClipboardMsg`, `FileMsg`, `IpcMsg`, `SessionMode`, `VideoQualityMode`, `MonitorInfo`, `MoveRect`, `DirtyTile`.
  - `tls.rs`: `rcgen 0.14.10` cert generator/loader (`ServerCertIdentity`), SHA-256 colon-hex formatter, `rustls 0.23` `ServerConfig` / `ClientConfig` (TLS 1.3 only, `ring` provider), `TofuServerCertVerifier`.
  - `auth.rs`: Argon2id (`m=65536, t=3, p=1`) key derivation, channel-bound HMAC-SHA256 `client_proof` / `server_proof` generation and constant-time verification, `IpRateLimiter` (IPv4-mapped IPv6 normalization, progressive cooldowns, bounded capacity).
  - `codec.rs`: `TileEncoder`/`TileDecoder` (`zstd` level 3 + BGRA/RGBA swizzle & move-rect helper) and `H264Encoder`/`H264Decoder` (`openh264 0.9.10` `BgraSliceU8` -> `YUVBuffer` -> Annex-B -> RGBA8).
  - `error.rs`: unified `ProtoError` using `thiserror 2.0`.

**Phase 1 Exit Criteria & Test Matrix:**

| Test ID | Target | Verification |
|---|---|---|
| `P1-T1` | `frame.rs` | Roundtrip zero-byte, normal, `FLAG_COMPRESSED` Zstd frames; reject `length > 16 MiB` & unknown `channel_id` |
| `P1-T2` | `messages.rs` | `bincode` roundtrip for all enum variants; reject trailing garbage & oversize nested `Vec` allocation attempts |
| `P1-T3` | `tls.rs` | Generate self-signed cert via `rcgen`, run real in-memory `tokio-rustls` TLS 1.3 handshake (`TofuServerCertVerifier`: accept matching pin, abort with `TofuConfirmationRequired` on first use, reject mismatch) |
| `P1-T4` | `auth.rs` | Valid Argon2id + HMAC-SHA256 mutual auth succeeds; wrong password, tampered `cert_sha256`, tampered nonce fail cleanly |
| `P1-T5` | `auth.rs` | `IpRateLimiter`: normalizes `::ffff:127.0.0.1` == `127.0.0.1`, escalates cooldowns at 3/4/5/6 failures, resets on auth success, evicts safely at cap |
| `P1-T6` | `codec.rs` | Zstd `TileBatch` move-rect + dirty-tile roundtrip preserves exact pixel values; `openh264` encode/decode roundtrip succeeds on synthetic 640x480 BGRA test frames |

---

### Phase 2: Windows Service Host, Session-0 Agent Bridge & Hybrid Capture (`deskbolt-server`)

**Scope:**
- Implement `crates/deskbolt-server`:
  - `main.rs` & CLI modes: default SCM dispatcher, `--install`, `--uninstall`, `--console` (foreground local run for rapid integration testing), `--set-password`, `--regen-cert`, `--agent` (console desktop worker).
  - `service.rs`: `windows-service 0.8.1` SCM host (`STOP | SHUTDOWN | SESSION_CHANGE`), Windows policy check (`SoftwareSASGeneration`), `SendSAS(FALSE)` dynamic loader (`sas.dll`).
  - `config.rs`: `%PROGRAMDATA%\DeskBolt\server.toml` + DPAPI-sealed `%PROGRAMDATA%\DeskBolt\auth.dat` with `SYSTEM`/`Administrators`-only ACL enforcement.
  - `ipc.rs`: hardened named pipe `\\.\pipe\DeskBolt_IPC` (`D:(A;;GA;;;SY)`, `PIPE_REJECT_REMOTE_CLIENTS`, `GetNamedPipeClientProcessId` verification), `WTSGetActiveConsoleSessionId` monitor, `OpenProcessToken` + `DuplicateTokenEx` + `SetTokenInformation(TokenSessionId)` + `CreateProcessAsUserW` agent supervisor.
  - `net.rs`: TCP `4899` listener, 5s unauthenticated handshake deadline, TLS 1.3 + Argon2id/HMAC auth, `IpRateLimiter`, per-session capability enforcement (`FullControl` vs `ViewOnly` vs `FileTransfer`), priority outbound mux with video frame-drop & dirty-rect coalescing.
  - `agent/`:
    - `desktop.rs`: `OpenInputDesktop` + `SetThreadDesktop` switcher across `WinSta0\Default` and `WinSta0\Winlogon`.
    - `capture.rs`: `DXGIManager` (`dxgi-capture-rs`) multi-monitor capture + `FrameMetadata` dirty/move rect extractor + `Auto` / `CrispText` / `SmoothVideo` encoder pipeline.
    - `input.rs`: virtual-desktop coordinate mapper + `SendInput` dispatcher + depressed-key tracker (`ReleaseAllKeys` on disconnect).
    - `clipboard.rs`: hidden HWND `AddClipboardFormatListener` bidirectional UTF-8 clipboard sync with loop guard.

**Phase 2 Exit Criteria & Test Matrix:**

| Test ID | Target | Verification |
|---|---|---|
| `P2-T1` | `net.rs` | Loopback TLS 1.3 + auth integration test against `--console` server; verifies 5s timeout drops silent clients, bad auth increments rate limiter |
| `P2-T2` | `net.rs` | Capability boundary test: `ViewOnly` session sending `InputMsg` or `SendSas` is rejected cleanly without reaching agent |
| `P2-T3` | `input.rs` | Unit tests for virtual desktop coordinate mapping (primary monitor, negative-left secondary monitor, clamping `[0..1]` -> `[0..65535]`) |
| `P2-T4` | `capture.rs` | Unit tests for dirty-rect coalescing, `Auto` mode hysteresis (`CrispText` <-> `SmoothVideo`), and dropped-tile dirty-rect union preservation |
| `P2-T5` | `ipc.rs` | Integration test verifying named-pipe SDDL builder, framing roundtrip over loopback pipe, and PID verification logic |
| `P2-T6` | Live Host Checklist | Documented manual/admin run check (if session is non-elevated, clearly report whether `--install` + UAC/Winlogon capture ran in an elevated Session-0 service or was deferred to elevated verification) |

---

### Phase 3: Native Viewer UI, SQLite Phonebook, Multi-Tab Viewport & Key Hook (`deskbolt-viewer`)

**Scope:**
- Implement `crates/deskbolt-viewer`:
  - `db.rs`: `%APPDATA%\DeskBolt\phonebook.db` (`rusqlite 0.32` `bundled`), migrations, nested folders, host CRUD, instant search, TOFU fingerprint storage.
  - `session.rs`: async Tokio connection controller, two-step TOFU verification flow, `0x02` background decode worker (`TileBatch` + `openh264::Decoder`), `FrameAck` pacing, clipboard loop guard.
  - `hook.rs`: focus-gated `SetWindowsHookExW(WH_KEYBOARD_LL)` capture thread (`Alt+Tab`, `Win` combos) with `Ctrl+Alt+Home` safety escape and focus-loss key-release.
  - `ui/`:
    - `phonebook.rs`: classic Address Book window (Quick-Connect bar, folder tree, host table/cards, search, modals).
    - `viewport.rs`: Multi-tab remote session window, compact toolbar (`Full Control`, `View Only`, `File Transfer`, `Send Ctrl+Alt+Del`, `Monitor`, `Clipboard`, `1:1 / Fit`, `Quality`), `egui::TextureHandle` (`Nearest` vs `Linear`), pointer coordinate mapping.
    - `dialogs.rs`: clean non-jargon Password, TOFU Trust, Key Mismatch, and Host Edit modals.

**Phase 3 Exit Criteria & Test Matrix:**

| Test ID | Target | Verification |
|---|---|---|
| `P3-T1` | `db.rs` | In-memory & temp-file SQLite tests: schema migration, nested folder cascade delete, host CRUD, `pinned_fingerprint` update/clear, instant search |
| `P3-T2` | `viewport.rs` | Unit tests for aspect-locked `Fit` display rect calculation & `1:1` coordinate mapping + out-of-bounds letterbox click filtering |
| `P3-T3` | `session.rs` | End-to-end headless session client <-> `deskbolt-server` loopback test: TOFU pin match, auth, `MonitorList` receipt, `TileBatch`/`H264Frame` decode to RGBA buffer, `ViewOnly` <-> `FullControl` switch |
| `P3-T4` | `hook.rs` | Unit tests for `KBDLLHOOKSTRUCT` -> `InputMsg::KeyEvent` translation, extended key flags, and `Ctrl+Alt+Home` escape trigger |

---

### Phase 4: Dual-Pane File Transfer, Hardening & Static Binary Release Audit

**Scope:**
- Implement `0x05` File Transfer engine across `deskbolt-server` (`files.rs`) and `deskbolt-viewer` (`ui/file_transfer.rs` + `transfer.rs`):
  - Server logical drive enumeration, directory listing, mkdir/rename/delete, path canonicalization & traversal prevention (`..`, device paths), chunked streaming (`64 KiB` chunks, `<= 8` window), incremental SHA-256 verification, atomic `.part` finalize.
  - Viewer secondary native window with Local vs. Remote dual panes, multi-select, queue manager, bandwidth/progress UI, overwrite resolution prompt (`Overwrite`, `Overwrite All`, `Skip`, `Skip All`, `Cancel`), and SHA-256 verification status.
- Full-suite hardening & release verification:
  - Slow-link backpressure stress simulation (`TileBatch` coalescing + `H264` IDR recovery).
  - `cargo clippy --workspace --all-targets -- -D warnings`.
  - `cargo build --release --workspace --target x86_64-pc-windows-msvc`.
  - Binary dependency check (`dumpbin /dependents` or PE import inspection) confirming zero non-Windows runtime DLLs.
  - UI copy audit verifying zero debug telemetry, crate jargon, or external URLs.

**Phase 4 Exit Criteria & Test Matrix:**

| Test ID | Target | Verification |
|---|---|---|
| `P4-T1` | `files.rs` | Path traversal unit tests (`..`, mixed slashes, `\\.\`, null bytes, drive escape rejected) |
| `P4-T2` | End-to-End FT | Real loopback upload & download of small, multi-megabyte, and empty files + nested directories: byte-for-byte & SHA-256 match, atomic `.part` cleanup on simulated abort/corrupt hash |
| `P4-T3` | `transfer.rs` | Sliding-window queue state machine & overwrite policy (`Skip`, `OverwriteAll`) tests |
| `P4-T4` | Release Audit | `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --release --workspace`, PE import table audit confirming static offline binaries |
