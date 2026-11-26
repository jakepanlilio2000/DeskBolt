# DeskBolt Native Viewer UI/UX & SQLite Phonebook Specification

**Document ID:** `SPEC-UI-004`  
**Crate:** `crates/deskbolt-viewer` (`eframe 0.37.0`, `egui 0.37.0`, `rusqlite 0.32` `bundled`)

---

## 1. Visual Design Principles & Copy Rules

DeskBolt follows classic native desktop ergonomics: fast, utilitarian, keyboard-friendly, high-information-density native desktop UI without bloated hero banners, animations, or cloud sign-in clutter.

1. **Zero Jargon / Clean Production Strings:** Never show crate names, internal channel numbers, debug enums, or developer phrasing in any label or dialog. Use concise Windows admin terminology (`Full Control`, `View Only`, `File Transfer`, `Send Ctrl+Alt+Del`, `1:1 Scale`, `Fit to Window`).
2. **Instant Startup:** Launches directly into the Address Book window in `< 150 ms` with offline local SQLite storage.
3. **Zero Plaintext Credential Persistence:** `phonebook.db` stores host metadata and pinned TLS SHA-256 fingerprints — **never** passwords.

---

## 2. Local SQLite Phonebook Schema (`%APPDATA%\DeskBolt\phonebook.db`)

Opened with `rusqlite::Connection::open(db_path)` using `PRAGMA`:
```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
```

### 2.1 DDL Schema

```sql
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS folders (
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
    default_mode INTEGER NOT NULL DEFAULT 0 CHECK(default_mode IN (0, 1, 2)), -- 0: FullControl, 1: ViewOnly, 2: FileTransfer
    quality_mode INTEGER NOT NULL DEFAULT 0 CHECK(quality_mode IN (0, 1, 2)), -- 0: Auto, 1: CrispText, 2: SmoothVideo
    pinned_fingerprint TEXT, -- Uppercase colon-separated SHA-256 hex (95 chars) or NULL
    last_connected INTEGER,  -- Unix timestamp (seconds) or NULL
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
CREATE INDEX IF NOT EXISTS idx_hosts_recent ON hosts(last_connected DESC);
```

### 2.2 Instant Live Search Query

Filtered in `< 1 ms` using SQL `LIKE` (case-insensitive ASCII/Unicode) or in-memory filtered snapshot synchronized on DB mutation:
- When a folder is selected, filter within that folder (or optionally include subfolders / `All Connections` virtual root).
- Typing in the search bar (`Ctrl+F`) filters across `name` and `host` instantaneously across all hosts.

---

## 3. Window & Layout Specifications (`eframe 0.37.0`)

### 3.1 Address Book Window (Root Viewport)

Configured via `eframe::NativeOptions`:
- `viewport`: `ViewportBuilder::default().with_title("DeskBolt Viewer").with_inner_size([920.0, 600.0]).with_min_inner_size([640.0, 420.0])`.

```text
+-----------------------------------------------------------------------------+
| [+] Add Host  [Folder+] New Folder   Search: [_________________ Ctrl+F] [X] |
+-----------------------------------------------------------------------------+
| Quick Connect: [Host / IP________] Port:[4899] [Full Control v] [Connect]   |
+--------------------+--------------------------------------------------------+
| Folders            | Hosts (Table / Cards View)                  [List|Grid]|
| > All Connections  | Name             Address           Mode         Trust  |
|   v Production     | ------------------------------------------------------ |
|     SQL-Node-01    | POS-Register-04  10.20.4.14:4899   Full Control Pinned |
|     POS Terminals  | DC-Primary       10.20.0.10:4899   View Only    Pinned |
|   > Branch Office  | Lab-Test-VM      192.168.50.8:4899 Full Control New    |
|                    |                                                        |
|                    | (Double-click row to connect; Right-click for menu)    |
+--------------------+--------------------------------------------------------+
| Status: Ready  |  3 saved connections  |  Active sessions: 1               |
+-----------------------------------------------------------------------------+
```

#### Address Book Interactions
- **Quick-Connect Bar:** Connects immediately without requiring saving to `phonebook.db` (if an existing host matches `host:port`, reuses its `pinned_fingerprint`; otherwise runs TOFU prompt with an optional checkbox *"Save host to address book"*).
- **Double-Click Row:** Opens session in host's `default_mode` (`Full Control`, `View Only`, or `File Transfer`).
- **Context Menu on Host Row:**
  - `Connect - Full Control`
  - `Connect - View Only`
  - `Connect - File Transfer`
  - Separator
  - `Edit Connection...`
  - `Clear Trusted Security Key` (resets `pinned_fingerprint = NULL`)
  - `Delete` (with confirmation prompt)
- **Context Menu on Folder Tree:** `New Subfolder`, `Rename Folder`, `Delete Folder` (moves child hosts to parent/root or deletes if confirmed).

### 3.2 Authentication & Security Dialogs

1. **TOFU First-Use Verification Modal:**
   - Displays: `Connecting to <Name> (<Host>:<Port>)`
   - Prompt: `"Verify the host's security fingerprint before connecting:"`
   - Monospace SHA-256 fingerprint formatted in 8 readable 4-byte groups (`AABB:CCDD ...`).
   - Buttons: `Trust & Continue` (saves `pinned_fingerprint` if saved host), `Cancel`.
2. **TOFU Key Mismatch Warning Modal (Hard Block):**
   - High-visibility warning banner: `"Security Key Changed"`
   - Explains plainly: `"The certificate presented by <Host> does not match the previously trusted key. Continue only if the host was intentionally reinstalled or its key was rotated."`
   - Shows `Expected Fingerprint` vs `Received Fingerprint`.
   - Buttons: `Disconnect` (default focus), `Replace Trusted Key` (requires second click or explicit confirmation toggle).
3. **Password Prompt Modal:**
   - Displays host name and requested mode (`Full Control` / `View Only` / `File Transfer`).
   - Masked password field (`password(true)`), autofocused on open, submits on `Enter`.
   - On failure: displays inline message (`"Authentication failed. Check password and try again."` or `"Too many failed attempts. Try again in X seconds."`) without closing the dialog so the user can retry cleanly.

---

## 4. Multi-Tab Remote Session Window

Opened as a secondary native viewport (`ViewportBuilder::default().with_title("DeskBolt — Sessions").with_inner_size([1280.0, 800.0])`) when the first `FullControl` or `ViewOnly` session connects; subsequent sessions add tabs to the same window (with an option to detach or open File Transfer separately).

```text
+-----------------------------------------------------------------------------+
| [x] DC-Primary (Full Control) | [x] POS-04 (View Only) | [+]                |
+-----------------------------------------------------------------------------+
| [Full Control] [View Only] [File Transfer] | [Send Ctrl+Alt+Del]            |
| Monitor: [Monitor 1 (1920x1080) v] | [Clipboard v] | [1:1 | Fit] | [Auto v] |
+-----------------------------------------------------------------------------+
|                                                                             |
|                                                                             |
|                     Remote Desktop GPU Texture Viewport                     |
|             (Nearest in 1:1 / CrispText, Linear in Fit Scale)               |
|                                                                             |
|                                                                             |
+-----------------------------------------------------------------------------+
| Connected | 1920x1080 | Crisp Text | RTT: 4 ms | 30 fps                     |
+-----------------------------------------------------------------------------+
```

### 4.1 Compact Toolbar Controls

| Control | Behavior |
|---|---|
| `Full Control` / `View Only` | Toggles active session input mode via `ControlMsg::SessionModeSwitch` (shows active pill state) |
| `File Transfer` | Opens the Dual-Pane File Transfer window bound to the same host (reusing authenticated session or opening a `0x05` channel) |
| `Send Ctrl+Alt+Del` | Sends `ControlMsg::SendSas` (enabled only in `FullControl`) |
| `Monitor` Dropdown | Lists `MonitorInfo` entries (`"1: Primary (1920x1080)"`, `"2: Display 2 (2560x1440)"`); sends `MonitorSelect` |
| `Clipboard` Menu | `Auto-Sync Clipboard` toggle (default On in `FullControl`), `Send Local Clipboard`, `Pull Remote Clipboard` |
| `1:1` / `Fit` Toggle | `1:1`: renders texture at exact 1:1 physical pixels inside `egui::ScrollArea` (`both`); `Fit`: scales uniformly preserving aspect ratio inside available `CentralPanel` rect |
| `Quality` Dropdown | `Auto` (hybrid heuristic), `Crisp Text` (forced lossless `zstd` tiles), `Smooth Video` (forced `openh264`) |
| `Capture Keys` / Fullscreen | Toggles `WH_KEYBOARD_LL` system key forwarding and borderless fullscreen (`F11`) |

### 4.2 GPU Texture Upload & Sampling (`LINEAR` vs `NEAREST`)

- Each session tab owns a persistent `Option<egui::TextureHandle>` and `last_uploaded_seq: u64`.
- On each UI `update(ctx, frame)`:
  1. Check session decoder's `latest_frame` atomic sequence counter.
  2. Only if `latest_seq != last_uploaded_seq` (or sampling mode toggled), update GPU texture:
     - **`1:1` Scale OR integer-matched `Crisp Text`:** `egui::TextureOptions::NEAREST` (`TextureFilter::Nearest` for both magnification & minification) so every glyph pixel maps 1:1 with zero blur.
     - **`Fit Scale` (non-1:1 zoom):** `egui::TextureOptions::LINEAR` (`TextureFilter::Linear`) so scaled-down 4K/1440p desktops remain smooth and legible without jagged pixel shimmer.
  3. Compute exact `display_rect` inside `CentralPanel`:
     - In `1:1` mode: `[remote_w / pixels_per_point, remote_h / pixels_per_point]` inside `ScrollArea::both()`.
     - In `Fit` mode: centered aspect-locked box inside `ui.available_size()`.
  4. Map pointer events only when pointer hovers inside `display_rect`:
     - `norm_x = ((pos.x - display_rect.min.x) / display_rect.width()).clamp(0.0, 1.0)`
     - `norm_y = ((pos.y - display_rect.min.y) / display_rect.height()).clamp(0.0, 1.0)`

### 4.3 Low-Level Keyboard Hook Capture (`WH_KEYBOARD_LL`)

Standard `egui` event handling cannot intercept `Alt+Tab`, `Win+R`, `Win+L`, `Ctrl+Esc`, or `PrintScreen` before the local Windows shell acts on them.

1. **Activation Conditions (ALL must hold):**
   - Active tab is in `SessionMode::FullControl`.
   - `Capture System Keys` is enabled.
   - The DeskBolt session window is the OS foreground window (`GetForegroundWindow() == viewer_hwnd`) AND focus is NOT inside a local Viewer modal/dropdown.
2. **Hook Architecture:**
   - Dedicated thread installs `SetWindowsHookExW(WH_KEYBOARD_LL, hook_proc, hinstance, 0)` and runs `GetMessageW` loop.
   - `hook_proc` executes in `< 50 microseconds` (never blocks or awaits):
     - Escape hatch: pressing `Ctrl+Alt+Home` (or `ScrollLock` double-tap) immediately suspends key capture and releases all remote modifiers (`ReleaseAllKeys`), giving the operator a guaranteed local escape!
     - Otherwise, reads `KBDLLHOOKSTRUCT` (`vkCode`, `scanCode`, `flags & LLKHF_EXTENDED`, `WM_KEYDOWN`/`WM_SYSKEYDOWN` vs `WM_KEYUP`/`WM_SYSKEYUP`), pushes `InputMsg::KeyEvent` via `try_send` to the active session sender, and returns `1` (consumed) so local Windows ignores `Alt+Tab` / `Win`!
   - **Focus-Loss Safety:** When `WM_ACTIVATEAPP` / `WM_KILLFOCUS` fires or `GetForegroundWindow()` changes, immediately suspend interception and send `InputMsg::ReleaseAllKeys` to the remote host.
   - When `Capture System Keys` is off (or in `ViewOnly`), zero hook is installed and regular `egui` key events forward standard typing.

---

## 5. Dual-Pane File Transfer Window

Opened either directly from a `FileTransfer` connection or by clicking `File Transfer` on an active session toolbar.

```text
+-----------------------------------------------------------------------------+
| Transfer: DC-Primary (10.20.0.10:4899)                    [Refresh Both]    |
+------------------------------------+---+------------------------------------+
| Local Computer                     |   | Remote: DC-Primary                 |
| [C:\ v] [C:\Users\Admin\Deploy] [^]|   | [D:\ v] [D:\Services\App]      [^] |
| [New Folder] [Rename] [Delete]     |   | [New Folder] [Rename] [Delete]     |
| ---------------------------------- |   | ---------------------------------- |
| [DIR] configs              --      | ->| [DIR] bin                  --      |
| [FIL] app.exe         14.2 MB      | <-| [FIL] service.exe     18.4 MB      |
| [FIL] settings.toml    2.1 KB      |   | [FIL] readme.txt       4.0 KB      |
+------------------------------------+---+------------------------------------+
| Transfer Queue (1 active, 2 queued)                                         |
| -> app.exe          D:\Services\App\app.exe   [=========>  ] 78%  112 MB/s  |
| <- service.exe      C:\Users\Admin\Deploy\... [Waiting]             Queued  |
| [v] Done: config.json -> Verified (SHA-256)                 [Clear Done]    |
+-----------------------------------------------------------------------------+
```

### 5.1 File Transfer UX & Integrity Rules

- **Directory Operations:** Both panes support drive selection, path bar entry (`Enter` to navigate), `Up` parent navigation, `New Folder`, `Rename`, and `Delete` (confirmation required).
- **Recursive Folder Transfers:** Selecting a directory queues a pre-scan creating target directories (`MkdirReq`) and enqueuing leaf files sequentially.
- **Overwrite Resolution Modal:** When `TransferStartAck` reports `exists == true`, pause that queue item and present a clean prompt showing:
  - File name, local size/mtime, remote size/mtime.
  - Actions: `Overwrite`, `Overwrite All` (applies to remainder of current batch), `Skip`, `Skip All`, `Cancel`.
- **Integrity & Status Display:**
  - Every transferred file is hashed incrementally with `SHA-256` on sender and receiver.
  - Queue status transitions: `Queued` -> `Transferring (XX%, MB/s)` -> `Verifying` -> `Verified` (or `Checksum Mismatch — Deleted/Quarantined`).
  - Failed or aborted partial downloads write to `<target>.part` first and atomically rename ONLY after `FileVerifyResult { verified: true }`! (Cleanly removes partial `.part` files on failure/cancel.)
