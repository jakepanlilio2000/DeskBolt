<div align="center">

<img src="assets/icon.svg" alt="DeskBolt Logo" width="96" />

# DeskBolt

Fast, self-hosted native remote desktop, terminal, file transfer, and network administration suite in Rust.

[![Release](https://img.shields.io/github/v/release/jakepanlilio2000/DeskBolt?include_prereleases)](https://github.com/jakepanlilio2000/DeskBolt/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/jakepanlilio2000/DeskBolt/version-and-release.yml)](https://github.com/jakepanlilio2000/DeskBolt/actions/workflows/version-and-release.yml)
[![License](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](Cargo.toml)
[![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20Linux%20%7C%20macOS%20%7C%20Android-lightgrey.svg)](#quick-start)

</div>

---

## Core Capabilities

- **Remote Desktop & Privacy:** Direct TLS 1.3 TCP connectivity on port `4899` with zero cloud relays or telemetry; adaptive lossless `zstd` tile updates (`4:4:4`) paired with OpenH264 motion streaming; Windows Session-0 service capture (`DeskBoltSvc`) across sign-in, lock, and UAC prompts; `Full Control`, `View Only`, and `File Transfer` modes; multi-monitor switching; focus-gated system-key capture (`Ctrl+Alt+Home` safety release); curtain mode; IP CIDR allow/blocklists; interactive incoming confirmation prompts; and Argon2id + HMAC-SHA256 mutual authentication.
- **Native Terminal, SSH & Serial:** Embedded VT100 terminal over host PTY shells (`portable-pty` / ConPTY), direct SSH2 client (`russh`) with password/private-key login and pinned host keys, local and remote serial ports (`serialport`), reusable command snippets drawer, and folder-wide non-interactive broadcast command execution.
- **File Transfer & SHA-256 Sync:** Dual-pane file manager and SFTP client (`russh-sftp`) with SHA-256 verified chunked transfers, delta directory tree diffing (`Mirror Local -> Remote`, `Mirror Remote -> Local`, `Two-Way Newest`), quick remote text editing (`Ctrl+S`), and offset-based live log tailing.
- **Network Diagnostics & Port Tunnels:** Wake-on-LAN (UDP port 9), bounded IPv4 CIDR port scanner with service banner grabbing, ICMP/TCP-fallback live latency monitoring, DNS lookups (`hickory-resolver`), native OS ARP table inspection, TTL traceroute, encrypted forward/reverse TCP tunnels, and headless system/process/service management.
- **Text & Voice Communications:** Bidirectional session text chat with unread indicators, timestamps, and local transcript saving; one-shot host desktop alert popups across user and Winlogon sessions; and full-duplex 16 kHz mono 16-bit PCM voice chat (`cpal`) with mute, noise gate, speaker volume, and push-to-talk.
- **Power & Out-of-Band Hardware Control:** Authenticated remote host power actions (`Shutdown`, `Restart`, `ColdPowerOff`, `LogOff`, `Sleep`, `Hibernate`, `Lock`) plus pure-Rust Out-of-Band Hardware Control over WS-Management CIM SOAP XML (HTTP/TLS with MD5/SHA-256 Digest auth) and Serial-over-LAN (`BIOS Console`, `Startup Remote Control`, `Network Boot`).

---

## Quick Start

DeskBolt listens on TCP/TLS port **`4899`** by default. On first connection, hosts present a self-signed X.509 certificate protected by **Trust-On-First-Use (TOFU)**: the viewer prompts you to confirm the host's SHA-256 fingerprint, pins it in your local address book, and alerts immediately if a host key ever changes.

### Windows

- **Installer (Interactive or Silent IT Rollout):**
  ```powershell
  # Silent deployment with Server Service + Viewer + firewall rule on TCP 4899
  .\DeskBolt-v<VERSION>-windows-x64-setup.exe /S /SERVER=1 /PASSWORD=YourStrongPassword /PORT=4899
  ```
  Supports `/VIEWER=1|0`, `/SERVER=1|0`, `/PASSWORD=<pass>`, `/PORT=<1-65535>`, and `/D=<install_dir>`.
- **Standalone CLI & Service Setup:**
  ```powershell
  .\deskbolt-server.exe --set-password
  New-NetFirewallRule -DisplayName "DeskBolt Remote Host" -Direction Inbound -Protocol TCP -LocalPort 4899 -Action Allow
  .\deskbolt-server.exe --install
  Start-Service DeskBoltSvc
  ```
  Run `.\deskbolt-server.exe --settings` to open the 4-tab host settings window (`General`, `Security`, `Access Control`, `Audio & Logging`). Host configuration lives under `%ProgramData%\DeskBolt\`; viewer address books and settings live in `%APPDATA%\DeskBolt\phonebook.db`. Launch saved hosts directly via desktop shortcuts (`deskbolt-viewer.exe --connect-id <uuid> --mode full`).

### Linux (All Distros)

```bash
# Debian / Ubuntu / Mint
sudo apt install ./deskbolt-server_*.deb ./deskbolt-viewer_*.deb

# Fedora / RHEL / openSUSE
sudo dnf install ./deskbolt-server-*.rpm ./deskbolt-viewer-*.rpm

# Arch Linux
sudo pacman -U ./deskbolt-*.pkg.tar.zst

# Portable archive or AppImage (Alpine, NixOS, Gentoo, Void, universal)
tar -xzf DeskBolt-v*-linux-x86_64.tar.gz -C /usr/local/bin/
chmod +x DeskBolt-v*-linux-x86_64.AppImage && ./DeskBolt-v*-linux-x86_64.AppImage
```

Activate host password, firewall port, and background daemon (auto-detects **systemd** or **OpenRC**):

```bash
sudo deskbolt-server --set-password
sudo ufw allow 4899/tcp || (sudo firewall-cmd --add-port=4899/tcp --permanent && sudo firewall-cmd --reload)
sudo deskbolt-server --install
```

- **Display & Input:** Supports X11 and Wayland (`xdg-desktop-portal` / PipeWire; approve screen-sharing prompts when connecting) with automatic `/dev/uinput` input injection (`sudo modprobe uinput`). Host config lives under `/etc/deskbolt/config.toml`; viewer address books live under `~/.config/deskbolt/phonebook.db`.

### macOS

1. Mount `DeskBolt-v<VERSION>-macos-universal.dmg` and drag **DeskBolt Viewer.app** into `/Applications` (copy `deskbolt-server` to `/usr/local/bin/` if hosting).
2. Configure the host password and `LaunchDaemon` (`/Library/LaunchDaemons/com.stacksync.deskbolt.server.plist`):
   ```bash
   sudo /usr/local/bin/deskbolt-server --set-password
   sudo /usr/local/bin/deskbolt-server --install
   ```
3. **Permissions:** Open **System Settings -> Privacy & Security** and grant `deskbolt-server` both **Screen Recording** and **Accessibility**. Host data lives in `/Library/Application Support/DeskBolt/`; viewer data lives in `~/Library/Application Support/DeskBolt/phonebook.db`.

### Android

1. Install `DeskBolt-v<VERSION>-android.apk` (Android 8.0+ / API 26+, 16 KB page-size aligned for Android 15/16).
2. **Touch Gestures:** Single-finger tap/drag for click and cursor movement, two-finger tap for right click, two-finger drag to scroll, pinch in/out to toggle **Fit** / **1:1 Scale**, and tap **Virtual Keyboard** on the toolbar during Full Control sessions.
3. **Optional Host Screen Sharing:** Grant `MediaProjection` capture and enable **DeskBolt Accessibility Service** in Android Settings when sharing your device screen.

---

## Developer Reference

### Workspace Layout

| Crate | Binary / Library | Responsibility |
|---|---|---|
| [`crates/deskbolt-proto`](crates/deskbolt-proto) | `deskbolt-proto` (`rlib`) | Multiplexed TLS 1.3 binary framing, Argon2id/HMAC auth, `rcgen` TOFU identity certs, `openh264`/`zstd` codecs, shared `cpal` 16 kHz PCM voice engine |
| [`crates/deskbolt-server`](crates/deskbolt-server) | `deskbolt-server` + `deskbolt_server_lib` | Cross-platform screen capture & input injection, Windows Session-0 `--agent` IPC, `--settings` UI, PTY/serial/tunnels/sysadmin/comms/power handlers |
| [`crates/deskbolt-viewer`](crates/deskbolt-viewer) | `deskbolt-viewer` + `deskbolt_viewer_lib` | Native `eframe`/`egui` desktop manager, `phonebook.db` SQLite store (`v3`), low-level key hook, `russh`/SFTP/Sync, net diagnostics, Out-of-Band Hardware Control (`oob.rs`), desktop shortcuts |

### Multiplexed Wire Protocol

Frames use a 6-byte big-endian header: `[u32 length BE][u8 channel_id][u8 flags][payload]` (`MAX_FRAME_PAYLOAD = 16 MiB`, `MAX_DECOMPRESSED_PAYLOAD = 32 MiB`). Flags: `0x01` `zstd` compressed, `0x02` keyframe, `0x04` end-of-stream, `0x08` urgent.

| Channel ID | Name | Payloads |
|---|---|---|
| `0x01` | `AuthControl` | Protocol hello, Argon2id/HMAC mutual auth challenges, mode switches, monitor selection, keepalive pings |
| `0x02` | `VideoTiles` | `TileBatch` lossless dirty/moved `zstd` blocks and OpenH264 NAL units + frame pace acks |
| `0x03` | `Input` | Normalized pointer movement/buttons/wheel, scan-code keyboard events, Windows SAS (`Ctrl+Alt+Del`) |
| `0x04` | `Clipboard` | UTF-8 clipboard synchronization updates |
| `0x05` | `FileTransfer` | Directory listing, chunked read/write, SHA-256 verification, bounded `HashTreeReq/Resp`, live log `TailChunkReq/Resp` |
| `0x06` | `Terminal` | Interactive host PTY (`OpenPty`, `PtyData`, `PtyResize`, `PtyExit`) and remote serial port streaming |
| `0x07` | `Tunnel` | Encrypted forward (`TunnelOpen` / `TunnelData`) and reverse (`TunnelReverseListen`) TCP port forwarding |
| `0x08` | `SysAdmin` | System CPU/RAM/disk snapshots, process termination, native OS service control, broadcast command execution |
| `0x09` | `Comms` | Text chat messages, desktop alert popups (`PopupMessage` / `PopupAck`), full-duplex PCM voice frames |
| `0x0A` | `PowerControl` | Authenticated host power actions (`Shutdown`, `Restart`, `ColdPowerOff`, `LogOff`, `Sleep`, `Hibernate`, `Lock`) |

### Build & Test from Source

```bash
# Run full workspace unit + loopback integration tests
cargo test --workspace

# Build optimized binaries (Windows links static +crt-static via .cargo/config.toml; requires NASM for OpenH264)
cargo build --release --workspace
```

- **Linux Prerequisites (Debian/Ubuntu):**
  ```bash
  sudo apt-get install -y --no-install-recommends \
    libxcb1-dev libxcb-randr0-dev libxcb-render0-dev libxcb-shm0-dev \
    libx11-dev libxrandr-dev libdbus-1-dev libpipewire-0.3-dev \
    libwayland-dev libegl-dev libdrm-dev libgbm-dev libfontconfig1-dev \
    libxkbcommon-dev libxkbcommon-x11-dev libxcb-shape0-dev libxcb-xfixes0-dev \
    libasound2-dev libclang-dev clang nasm pkg-config
  ```
- **Android Build (`16 KB` Page Alignment via `.cargo/config.toml`):**
  ```bash
  cargo ndk -t arm64-v8a -t x86_64 -p 26 -o android/app/src/main/jniLibs \
    build --release -p deskbolt-viewer -p deskbolt-server --locked
  cd android && gradle assembleRelease -PdeskboltVersionName=0.1.0-beta1
  ```
- **Commit-Driven Release Versioning (`scripts/bump_version.py` & `.github/workflows/version-and-release.yml`):**
  - `fix:` / default — increment `-betaN` (`0.1.0-beta1 -> 0.1.0-beta2`) or `X.Y.(Z+1)-beta1` from stable
  - `feat:` — increment `PATCH + 1` and append `-beta1` (`0.1.0-beta2 -> 0.1.1-beta1`)
  - `feat!:` / `[major]` / `BREAKING CHANGE` — increment `MINOR + 1`, reset `PATCH = 0`, append `-beta1`
  - `[stable]` — strip `-betaN` suffix (`0.2.0-beta4 -> 0.2.0`)
  - `[overhaul]` — increment `MAJOR + 1`, reset `MINOR.PATCH = 0.0` (add `[beta]` to stay in beta)

---

## License

Licensed under the **Mozilla Public License 2.0 (`MPL-2.0`)** — see [LICENSE](LICENSE). Copyright © StackSync Solutions (`jakepanlilio2000`).
