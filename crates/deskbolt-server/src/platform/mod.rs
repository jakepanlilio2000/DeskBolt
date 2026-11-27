use std::net::IpAddr;
use std::path::Path;

use deskbolt_proto::{
    CursorSnapshot, DesktopState, DeviceAction, KeyEvent, MonitorInfo, MoveRect, PointerEvent, PowerAction,
    SessionMode,
};
use tokio::sync::watch;

mod desktop_dir;
pub use desktop_dir::desktop_dir;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
mod windows_cursor;
#[cfg(target_os = "windows")]
use windows as imp;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod desktop_common;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::start_annotation_overlay;
#[cfg(target_os = "linux")]
mod x11_cursor;
#[cfg(target_os = "linux")]
mod x11_framebuffer;
#[cfg(target_os = "linux")]
mod wayland_capture;
#[cfg(target_os = "linux")]
use linux as imp;

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "macos")]
mod macos_cursor;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "android")]
use android as imp;

/// Captured frame ready for differential/hybrid encoding.
pub struct RawCapturedFrame {
    pub monitor_id: u8,
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub move_rects: Vec<MoveRect>,
    pub dirty_rects: Vec<(i32, i32, i32, i32)>,
    pub force_full: bool,
}

/// Event produced by a platform screen capture backend.
pub enum CaptureOutput {
    Frame(RawCapturedFrame),
    Timeout,
    DesktopSwitched {
        state: DesktopState,
        name: String,
    },
    Reacquire(String),
}

/// Platform screen and monitor capture interface.
pub trait ScreenCapturer: Send {
    fn monitors(&self) -> Vec<MonitorInfo>;
    fn set_monitor(&mut self, monitor_idx: u8);
    fn capture_next(&mut self, force_full: bool, timeout_ms: u32) -> CaptureOutput;
    fn set_workspace_capture(&mut self, _enabled: bool) -> bool {
        false
    }
    fn capture_cursor(&mut self) -> Option<CursorSnapshot> {
        None
    }
    fn current_desktop_name(&self) -> String {
        "Default".to_string()
    }
}

/// Platform keyboard and mouse injection interface.
pub trait InputInjector: Send {
    fn sync_desktop(&mut self) {}
    fn update_monitor_layout(&mut self, _monitors: &[MonitorInfo]) {}
    fn inject_pointer(&mut self, ev: PointerEvent, active_mon: &MonitorInfo);
    fn inject_key(&mut self, ev: KeyEvent);
    fn inject_unicode(&mut self, _ch: char) {}
    fn device_action(&mut self, _action: DeviceAction) {}
    fn release_all_keys(&mut self);
}

/// Converts tightly packed RGBA8 pixels (e.g. from `xcap` or Android `ImageReader`) to BGRA8.
pub fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut bgra = vec![0u8; rgba.len()];
    for (src, dst) in rgba.as_chunks::<4>().0.iter().zip(bgra.as_chunks_mut::<4>().0.iter_mut()) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = src[3];
    }
    bgra
}

/// Detects changed `block_size x block_size` pixel tiles between two same-sized BGRA8 buffers.
/// Used on X11/Wayland/macOS/Android where the capture API does not return hardware dirty rects.
pub fn diff_bgra_dirty_rects(
    prev: &[u8],
    curr: &[u8],
    width: u32,
    height: u32,
    block_size: u32,
) -> Vec<(i32, i32, i32, i32)> {
    let expected = (width as usize) * (height as usize) * 4;
    if width == 0 || height == 0 || prev.len() != expected || curr.len() != expected {
        return vec![(0, 0, width as i32, height as i32)];
    }
    let bs = block_size.max(8) as usize;
    let w = width as usize;
    let h = height as usize;
    let stride = w * 4;
    let mut dirty = Vec::new();

    let mut y = 0usize;
    while y < h {
        let y_end = (y + bs).min(h);
        let mut x = 0usize;
        while x < w {
            let x_end = (x + bs).min(w);
            let row_bytes = (x_end - x) * 4;
            let mut changed = false;
            for row in y..y_end {
                let off = row * stride + x * 4;
                if prev[off..off + row_bytes] != curr[off..off + row_bytes] {
                    changed = true;
                    break;
                }
            }
            if changed {
                dirty.push((x as i32, y as i32, x_end as i32, y_end as i32));
            }
            x = x_end;
        }
        y = y_end;
    }
    dirty
}

pub fn create_capturer(monitor_idx: u8) -> Result<Box<dyn ScreenCapturer>, String> {
    imp::create_capturer(monitor_idx)
}

pub fn create_injector() -> Box<dyn InputInjector> {
    imp::create_injector()
}

pub fn enumerate_monitors() -> Vec<MonitorInfo> {
    imp::enumerate_monitors()
}

pub fn bind_active_input_desktop() -> Result<String, String> {
    imp::bind_active_input_desktop()
}

pub fn install_service() -> Result<(), String> {
    imp::install_service()
}

pub fn uninstall_service() -> Result<(), String> {
    imp::uninstall_service()
}

/// Installs the OS background service, returning standard platform exit codes on failure.
pub fn install_service_with_code() -> Result<(), (i32, String)> {
    #[cfg(target_os = "windows")]
    {
        imp::install_service_with_code()
    }
    #[cfg(not(target_os = "windows"))]
    {
        imp::install_service().map_err(|e| (1, e))
    }
}

/// Stops and uninstalls the OS background service, returning standard platform exit codes on failure.
pub fn uninstall_service_with_code() -> Result<(), (i32, String)> {
    #[cfg(target_os = "windows")]
    {
        imp::uninstall_service_with_code()
    }
    #[cfg(not(target_os = "windows"))]
    {
        imp::uninstall_service().map_err(|e| (1, e))
    }
}

pub fn run_service_dispatcher() -> Result<(), String> {
    imp::run_service_dispatcher()
}

pub fn trigger_send_sas() -> Result<(), String> {
    imp::trigger_send_sas()
}

pub fn set_restrictive_acl(path: &Path) -> Result<(), String> {
    imp::set_restrictive_acl(path)
}

pub fn read_password_masked() -> Result<String, String> {
    imp::read_password_masked()
}

pub fn dpapi_protect_machine(plain: &[u8]) -> Result<Vec<u8>, String> {
    imp::dpapi_protect_machine(plain)
}

pub fn dpapi_unprotect_machine(cipher: &[u8]) -> Result<Vec<u8>, String> {
    imp::dpapi_unprotect_machine(cipher)
}

pub fn startup_permission_check() {
    imp::startup_permission_check();
}

/// Exposes all server-side OS platform helpers cleanly across Windows, Linux, macOS, and Android.
pub fn execute_power_action(
    action: PowerAction,
    force: bool,
    delay_secs: u32,
) -> Result<String, String> {
    imp::execute_power_action(action, force, delay_secs)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn connection_prompt_text(
    peer: std::net::IpAddr,
    mode: deskbolt_proto::SessionMode,
    seconds: u64,
) -> String {
    let access = match mode {
        deskbolt_proto::SessionMode::FullControl => "control this device",
        deskbolt_proto::SessionMode::ViewOnly => "view this screen",
        deskbolt_proto::SessionMode::FileTransfer => "transfer files",
    };
    format!("{peer} requests permission to {access}. Respond within {seconds} s.")
}

pub fn show_desktop_popup(title: &str, body: &str, urgent: bool) -> Result<(), String> {
    imp::show_desktop_popup(title, body, urgent)
}

pub fn prompt_connection_confirmation(
    peer_ip: IpAddr,
    mode: SessionMode,
    timeout_secs: u16,
    default_allow: bool,
) -> bool {
    imp::prompt_connection_confirmation(peer_ip, mode, timeout_secs, default_allow)
}

pub(crate) mod display_mode;
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub(crate) mod headless;
pub(crate) mod local_input_lock;
#[cfg(target_os = "windows")]
mod windows_curtain;

pub fn set_curtain_mode(enabled: bool) -> Result<(), String> {
    imp::set_curtain_mode(enabled)
}

pub fn set_os_autostart(enabled: bool) -> Result<(), String> {
    imp::set_os_autostart(enabled)
}

#[cfg(not(target_os = "macos"))]
pub fn start_tray_icon(
    shutdown_rx: watch::Receiver<bool>,
    shutdown_tx: watch::Sender<bool>,
) -> Option<std::thread::JoinHandle<()>> {
    imp::start_tray_icon(shutdown_rx, shutdown_tx)
}

static DISCONNECT_VIEWERS: std::sync::OnceLock<watch::Sender<u64>> = std::sync::OnceLock::new();

fn disconnect_signal() -> &'static watch::Sender<u64> {
    DISCONNECT_VIEWERS.get_or_init(|| watch::channel(0).0)
}

pub(crate) fn subscribe_disconnect() -> watch::Receiver<u64> {
    disconnect_signal().subscribe()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy)]
enum TrayAction {
    Status,
    Settings,
    Disconnect,
    Exit,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl TrayAction {
    const MENU: [(&str, Self); 4] = [
        ("Status", Self::Status),
        ("Settings", Self::Settings),
        ("Disconnect All", Self::Disconnect),
        ("Exit", Self::Exit),
    ];

    fn run(self, shutdown: &watch::Sender<bool>) {
        let result = match self {
            Self::Status => show_desktop_popup("DeskBolt Host", "The host is running.", false),
            Self::Settings => std::env::current_exe()
                .and_then(|exe| std::process::Command::new(exe).arg("--settings").spawn())
                .map(|mut child| {
                    // Reap the window process without blocking the menu.
                    std::thread::spawn(move || { let _ = child.wait(); });
                })
                .map_err(|error| error.to_string()),
            Self::Disconnect => {
                disconnect_signal().send_modify(|epoch| *epoch = epoch.wrapping_add(1));
                Ok(())
            }
            Self::Exit => {
                shutdown.send_replace(true);
                Ok(())
            }
        };
        if let Err(error) = result {
            tracing::warn!("Host menu action failed: {error}");
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn tray_icon_rgba() -> Vec<u8> {
    const BOLT: [u16; 16] = [
        0, 0, 0x0080, 0x0180, 0x0380, 0x0700, 0x0f00, 0x1ff8,
        0x1ff0, 0x01e0, 0x01c0, 0x0380, 0x0300, 0x0200, 0, 0,
    ];
    BOLT.into_iter().flat_map(|row| {
        (0..16).flat_map(move |x| {
            if row & (1 << (15 - x)) == 0 { [0, 0, 0, 0] } else { [65, 144, 248, 255] }
        })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_only_reaches_current_viewers() {
        let signal = watch::channel(0u64).0;
        let existing = signal.subscribe();
        signal.send_modify(|epoch| *epoch += 1);
        let new = signal.subscribe();
        assert!(existing.has_changed().unwrap());
        assert!(!new.has_changed().unwrap());
    }

    #[test]
    fn rgba_to_bgra_and_tile_differ_detect_single_dirty_block() {
        let rgba = vec![10, 20, 30, 255, 40, 50, 60, 128];
        assert_eq!(rgba_to_bgra(&rgba), vec![30, 20, 10, 255, 60, 50, 40, 128]);

        let w = 128u32;
        let h = 128u32;
        let prev = vec![0x11u8; (w * h * 4) as usize];
        let mut curr = prev.clone();
        // Flip one pixel in tile (64..128, 64..128)
        let off = ((80 * w + 90) * 4) as usize;
        curr[off] = 0xFF;

        let rects = diff_bgra_dirty_rects(&prev, &curr, w, h, 64);
        assert_eq!(rects, vec![(64, 64, 128, 128)]);
        assert!(diff_bgra_dirty_rects(&prev, &prev, w, h, 64).is_empty());
    }
}
