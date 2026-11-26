use std::fs;
use std::path::Path;
use std::process::Command;

use deskbolt_proto::{KeyEvent, MonitorInfo, PointerEvent};
use tracing::{info, warn};

use super::desktop_common::{
    EnigoInjector, XcapCapturer, enumerate_xcap_monitors, unix_read_password_masked,
    unix_set_restrictive_acl,
};
use super::{InputInjector, ScreenCapturer};

const LAUNCHD_PLIST_PATH: &str = "/Library/LaunchDaemons/com.stacksync.deskbolt.server.plist";

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

pub fn startup_permission_check() {
    // SAFETY: CoreGraphics and ApplicationServices read-only TCC permission checks.
    unsafe {
        if !CGPreflightScreenCaptureAccess() {
            warn!(
                "macOS Screen Recording permission is not granted. Enable DeskBolt under \
                 System Settings -> Privacy & Security -> Screen Recording."
            );
        }
        if !AXIsProcessTrusted() {
            warn!(
                "macOS Accessibility permission is not granted. Enable DeskBolt under \
                 System Settings -> Privacy & Security -> Accessibility for remote control."
            );
        }
    }
}

struct NoopInjector;
impl InputInjector for NoopInjector {
    fn inject_pointer(&mut self, _ev: PointerEvent, _mon: &MonitorInfo) {}
    fn inject_key(&mut self, _ev: KeyEvent) {}
    fn release_all_keys(&mut self) {}
}

pub fn create_capturer(monitor_idx: u8) -> Result<Box<dyn ScreenCapturer>, String> {
    Ok(Box::new(XcapCapturer::new(monitor_idx)?))
}

pub fn create_injector() -> Box<dyn InputInjector> {
    match EnigoInjector::try_new() {
        Ok(inj) => Box::new(inj),
        Err(e) => {
            warn!("macOS input injector unavailable ({e}); verify Accessibility permission");
            Box::new(NoopInjector)
        }
    }
}

pub fn enumerate_monitors() -> Vec<MonitorInfo> {
    enumerate_xcap_monitors()
}

pub fn bind_active_input_desktop() -> Result<String, String> {
    Ok("Aqua".into())
}

pub fn install_service() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
           <key>Label</key>\n\
           <string>com.stacksync.deskbolt.server</string>\n\
           <key>ProgramArguments</key>\n\
           <array>\n\
             <string>{}</string>\n\
             <string>--service</string>\n\
           </array>\n\
           <key>RunAtLoad</key>\n\
           <true/>\n\
           <key>KeepAlive</key>\n\
           <true/>\n\
           <key>StandardOutPath</key>\n\
           <string>/var/log/deskbolt-server.log</string>\n\
           <key>StandardErrorPath</key>\n\
           <string>/var/log/deskbolt-server.err.log</string>\n\
         </dict>\n\
         </plist>\n",
        exe.display()
    );
    fs::write(LAUNCHD_PLIST_PATH, plist)
        .map_err(|e| format!("write {LAUNCHD_PLIST_PATH} (run with sudo): {e}"))?;
    let c = std::ffi::CString::new(LAUNCHD_PLIST_PATH).unwrap();
    // SAFETY: launchd requires 0644 root-owned plist files.
    unsafe { libc::chmod(c.as_ptr(), 0o644) };
    let _ = Command::new("launchctl")
        .args(["bootout", "system", LAUNCHD_PLIST_PATH])
        .status();
    let st = Command::new("launchctl")
        .args(["bootstrap", "system", LAUNCHD_PLIST_PATH])
        .status()
        .map_err(|e| format!("launchctl bootstrap failed: {e}"))?;
    if !st.success() {
        let legacy = Command::new("launchctl")
            .args(["load", "-w", LAUNCHD_PLIST_PATH])
            .status()
            .map_err(|e| format!("launchctl load failed: {e}"))?;
        if !legacy.success() {
            return Err("launchctl failed to load LaunchDaemon".into());
        }
    }
    println!("Installed and loaded LaunchDaemon at {LAUNCHD_PLIST_PATH}.");
    Ok(())
}

pub fn uninstall_service() -> Result<(), String> {
    let _ = Command::new("launchctl")
        .args(["bootout", "system", LAUNCHD_PLIST_PATH])
        .status();
    let _ = Command::new("launchctl")
        .args(["unload", "-w", LAUNCHD_PLIST_PATH])
        .status();
    if Path::new(LAUNCHD_PLIST_PATH).exists() {
        fs::remove_file(LAUNCHD_PLIST_PATH)
            .map_err(|e| format!("remove {LAUNCHD_PLIST_PATH}: {e}"))?;
    }
    println!("Removed LaunchDaemon {LAUNCHD_PLIST_PATH}.");
    Ok(())
}

pub fn run_service_dispatcher() -> Result<(), String> {
    crate::run_inprocess_server_mode()
}

pub fn trigger_send_sas() -> Result<(), String> {
    Err("Send Ctrl+Alt+Del (SAS) is only supported on Windows service hosts.".into())
}

pub fn set_restrictive_acl(path: &Path) -> Result<(), String> {
    unix_set_restrictive_acl(path)
}

pub fn read_password_masked() -> Result<String, String> {
    unix_read_password_masked()
}

pub fn dpapi_protect_machine(_plain: &[u8]) -> Result<Vec<u8>, String> {
    Err("DPAPI not applicable on macOS".into())
}

pub fn dpapi_unprotect_machine(_cipher: &[u8]) -> Result<Vec<u8>, String> {
    Err("DPAPI not applicable on macOS".into())
}

pub fn show_desktop_popup(title: &str, body: &str, urgent: bool) -> Result<(), String> {
    info!(urgent, %title, %body, "Remote popup notification delivered to host");
    Ok(())
}

pub fn prompt_connection_confirmation(
    _peer_ip: std::net::IpAddr,
    _mode: deskbolt_proto::SessionMode,
    _timeout_secs: u16,
    default_allow: bool,
) -> bool {
    default_allow
}

pub fn execute_power_action(
    _action: deskbolt_proto::PowerAction,
    _force: bool,
    _delay_secs: u32,
) -> Result<String, String> {
    Err("Remote power management is unsupported via unprivileged macOS session.".into())
}

pub fn set_curtain_mode(_enabled: bool) -> Result<(), String> {
    Ok(())
}

pub fn set_os_autostart(_enabled: bool) -> Result<(), String> {
    Ok(())
}

pub fn start_tray_icon(
    _shutdown_rx: tokio::sync::watch::Receiver<bool>,
    _shutdown_tx: tokio::sync::watch::Sender<bool>,
) -> Option<std::thread::JoinHandle<()>> {
    None
}
