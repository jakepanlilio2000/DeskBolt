use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use deskbolt_proto::{KeyEvent, MonitorInfo, PointerEvent};
use tracing::warn;

use super::desktop_common::{
    EnigoInjector, XcapCapturer, enumerate_xcap_monitors, unix_read_password_masked,
    unix_set_restrictive_acl,
};
use super::{InputInjector, ScreenCapturer};

const LAUNCHD_PLIST_PATH: &str = "/Library/LaunchDaemons/com.stacksync.deskbolt.server.plist";
const LAUNCH_AGENT: &str = "com.stacksync.deskbolt.login";
static DISPLAY_CAPTURED: Mutex<bool> = Mutex::new(false);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGCaptureAllDisplays() -> i32;
    fn CGReleaseAllDisplays() -> i32;
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

pub(crate) fn interactive_session() -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    uid != 0 && fs::metadata("/dev/console").is_ok_and(|console| console.uid() == uid)
}

#[allow(deprecated)]
pub fn show_desktop_popup(title: &str, body: &str, urgent: bool) -> Result<(), String> {
    use objc2_core_foundation::{
        CFString, CFUserNotification, kCFUserNotificationCautionAlertLevel,
        kCFUserNotificationNoteAlertLevel,
    };
    use objc2_foundation::{NSBundle, NSString, NSUserNotification, NSUserNotificationCenter};
    if !interactive_session() {
        return Err("A desktop session is required.".into());
    }
    objc2::rc::autoreleasepool(|_| {
        if !urgent && NSBundle::mainBundle().bundleIdentifier().is_some() {
            let notification = NSUserNotification::new();
            notification.setTitle(Some(&NSString::from_str(title)));
            notification.setInformativeText(Some(&NSString::from_str(body)));
            NSUserNotificationCenter::defaultUserNotificationCenter()
                .deliverNotification(&notification);
            return Ok(());
        }
        // Command-line hosts lack the bundle identity used by the notification center.
        let level = if urgent {
            kCFUserNotificationCautionAlertLevel
        } else {
            kCFUserNotificationNoteAlertLevel
        };
        let status = CFUserNotification::display_notice(
            10.0,
            level,
            None,
            None,
            None,
            Some(&CFString::from_str(title)),
            Some(&CFString::from_str(body)),
            None,
        );
        if status == 0 {
            Ok(())
        } else {
            Err(format!("Desktop notification unavailable ({status})."))
        }
    })
}

pub fn prompt_connection_confirmation(
    peer_ip: std::net::IpAddr,
    mode: deskbolt_proto::SessionMode,
    timeout_secs: u16,
    default_allow: bool,
) -> bool {
    use objc2_core_foundation::*;
    use std::time::{Duration, Instant};
    if !interactive_session() || timeout_secs == 0 {
        return default_allow;
    }
    // SAFETY: These are immutable Core Foundation strings supplied by the system.
    let [Some(header), Some(message), Some(primary), Some(alternate)] = (unsafe {
        [
            kCFUserNotificationAlertHeaderKey,
            kCFUserNotificationAlertMessageKey,
            kCFUserNotificationDefaultButtonTitleKey,
            kCFUserNotificationAlternateButtonTitleKey,
        ]
    }) else {
        return default_allow;
    };
    let title = CFString::from_static_str("DeskBolt connection request");
    let decline = CFString::from_static_str("Decline");
    let allow = CFString::from_static_str("Allow");
    let content = |seconds| {
        let body = CFString::from_str(&super::connection_prompt_text(peer_ip, mode, seconds));
        let typed = CFDictionary::<CFString, CFString>::from_slices(
            &[header, message, primary, alternate],
            &[&title, &body, &decline, &allow],
        );
        // SAFETY: Type erasure keeps the underlying immutable dictionary unchanged.
        unsafe { CFRetained::cast_unchecked::<CFDictionary>(typed) }
    };
    let deadline = Instant::now() + Duration::from_secs(u64::from(timeout_secs));
    let level = kCFUserNotificationCautionAlertLevel;
    let mut error = 0;
    // SAFETY: The property dictionary contains string values for the declared keys.
    let Some(dialog) = (unsafe {
        CFUserNotification::new(
            None,
            f64::from(timeout_secs),
            level,
            &mut error,
            Some(&content(u64::from(timeout_secs))),
        )
    }) else {
        return default_allow;
    };
    loop {
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_secs_f64();
        if remaining == 0.0 {
            break;
        }
        let mut response = 0;
        // SAFETY: The response pointer remains valid for this synchronous call.
        if unsafe { dialog.receive_response(remaining.min(1.0), &mut response) } == 0 {
            let button = response & 3;
            return button == kCFUserNotificationAlternateResponse
                || (button == kCFUserNotificationCancelResponse
                    && Instant::now() >= deadline
                    && default_allow);
        }
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_secs_f64();
        // SAFETY: The updated property dictionary uses the same well-formed string keys.
        if remaining > 0.0
            && unsafe { dialog.update(remaining, level, Some(&content(remaining.ceil() as u64))) }
                != 0
        {
            break;
        }
    }
    dialog.cancel();
    default_allow
}

pub fn execute_power_action(
    action: deskbolt_proto::PowerAction,
    force: bool,
    delay_secs: u32,
) -> Result<String, String> {
    use deskbolt_proto::PowerAction;
    use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventTapLocation};
    if matches!(action, PowerAction::Hibernate | PowerAction::LogOff) {
        return Err("This power action is unavailable on macOS.".into());
    }
    if delay_secs > 60 {
        return Err("The maximum power action delay is 60 seconds.".into());
    }
    std::thread::sleep(std::time::Duration::from_secs(u64::from(delay_secs)));
    match action {
        PowerAction::Shutdown | PowerAction::ColdPowerOff | PowerAction::Restart => {
            let restarting = action == PowerAction::Restart;
            if interactive_session() {
                let event = match (restarting, force || action == PowerAction::ColdPowerOff) {
                    (true, true) => *b"rest",
                    (false, true) => *b"shut",
                    (true, false) => *b"rrst",
                    (false, false) => *b"rsdn",
                };
                send_login_window_event(event)?;
            // SAFETY: geteuid has no preconditions.
            } else if unsafe { libc::geteuid() } == 0 {
                // The built-in shutdown service handles machines with no desktop system process.
                let status = Command::new("/sbin/shutdown")
                    .args([if restarting { "-r" } else { "-h" }, "now"])
                    .status()
                    .map_err(|error| error.to_string())?;
                if !status.success() {
                    return Err("The system declined the power request.".into());
                }
            } else {
                return Err(
                    "A desktop session or system service is required for this power action.".into(),
                );
            }
        }
        PowerAction::Sleep => {
            #[link(name = "IOKit", kind = "framework")]
            unsafe extern "C" {
                fn IOPMFindPowerManagement(master: u32) -> u32;
                fn IOPMSleepSystem(connection: u32) -> i32;
                fn IOServiceClose(connection: u32) -> i32;
            }
            // SAFETY: Zero selects the default device port. The returned handle is checked and closed.
            let status = unsafe {
                let connection = IOPMFindPowerManagement(0);
                if connection == 0 {
                    return Err("The system power service is unavailable.".into());
                }
                let status = IOPMSleepSystem(connection);
                IOServiceClose(connection);
                status
            };
            if status != 0 {
                return Err(format!("The system declined the sleep request ({status})."));
            }
        }
        PowerAction::Lock => {
            // SAFETY: AXIsProcessTrusted is a read-only permission check.
            if !interactive_session() || !unsafe { AXIsProcessTrusted() } {
                return Err(
                    "Screen locking requires a desktop session with Accessibility permission."
                        .into(),
                );
            }
            let down =
                CGEvent::new_keyboard_event(None, 12, true).ok_or("Could not lock the screen.")?;
            let up =
                CGEvent::new_keyboard_event(None, 12, false).ok_or("Could not lock the screen.")?;
            for event in [&*down, &*up] {
                CGEvent::set_flags(
                    Some(event),
                    CGEventFlags::MaskControl | CGEventFlags::MaskCommand,
                );
                CGEvent::post(CGEventTapLocation::SessionEventTap, Some(event));
            }
        }
        _ => unreachable!(),
    }
    Ok("Power action requested.".into())
}

fn send_login_window_event(event_id: [u8; 4]) -> Result<(), String> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Descriptor {
        kind: u32,
        handle: *mut c_void,
    }
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AECreateDesc(kind: u32, data: *const c_void, len: isize, result: *mut Descriptor)
        -> i16;
        fn AECreateAppleEvent(
            class: u32,
            id: u32,
            target: *const Descriptor,
            return_id: i16,
            transaction: i32,
            result: *mut Descriptor,
        ) -> i16;
        fn AESendMessage(
            event: *const Descriptor,
            reply: *mut Descriptor,
            mode: i32,
            timeout: i32,
        ) -> i32;
        fn AEDisposeDesc(desc: *mut Descriptor) -> i16;
    }
    let system_process = [0u32, 1];
    let mut target = Descriptor {
        kind: 0,
        handle: std::ptr::null_mut(),
    };
    let mut event = Descriptor {
        kind: 0,
        handle: std::ptr::null_mut(),
    };
    // SAFETY: The descriptors are initialized by the native API and disposed after use.
    let status = unsafe {
        let created = AECreateDesc(
            u32::from_be_bytes(*b"psn "),
            system_process.as_ptr().cast(),
            8,
            &mut target,
        );
        if created != 0 {
            return Err(format!(
                "Could not address the desktop system process ({created})."
            ));
        }
        let created_event = AECreateAppleEvent(
            u32::from_be_bytes(*b"aevt"),
            u32::from_be_bytes(event_id),
            &target,
            -1,
            0,
            &mut event,
        );
        AEDisposeDesc(&mut target);
        if created_event != 0 {
            return Err(format!(
                "Could not build the power request ({created_event})."
            ));
        }
        let sent = AESendMessage(&event, std::ptr::null_mut(), 1, 60);
        AEDisposeDesc(&mut event);
        sent
    };
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "The desktop rejected the power request ({status})."
        ))
    }
}

pub fn set_curtain_mode(enabled: bool) -> Result<(), String> {
    let mut captured = DISPLAY_CAPTURED.lock().unwrap();
    if enabled == *captured {
        return Ok(());
    }
    if enabled && !interactive_session() {
        return Err("A desktop session is required to blank the screen.".into());
    }
    // SAFETY: Core Graphics owns the displays; the mutex pairs each capture with release.
    let status = unsafe {
        if enabled {
            CGCaptureAllDisplays()
        } else {
            CGReleaseAllDisplays()
        }
    };
    if status != 0 {
        return Err(format!("Could not change screen blanking ({status})."));
    }
    *captured = enabled;
    Ok(())
}

fn launch_agent_plist(exe: &str) -> String {
    let exe = exe
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <plist version=\"1.0\"><dict><key>Label</key><string>{LAUNCH_AGENT}</string>\n\
         <key>ProgramArguments</key><array><string>{exe}</string><string>--console</string></array>\n\
         <key>RunAtLoad</key><true/></dict></plist>\n"
    )
}

pub fn set_os_autostart(enabled: bool) -> Result<(), String> {
    use std::path::PathBuf;
    if !interactive_session() {
        return if enabled {
            Err("A desktop session is required to manage startup at login.".into())
        } else {
            Ok(())
        };
    }
    let home = std::env::var_os("HOME").ok_or("The home folder is unavailable.")?;
    let dir = PathBuf::from(home).join("Library/LaunchAgents");
    let file = dir.join(format!("{LAUNCH_AGENT}.plist"));
    // SAFETY: geteuid has no preconditions.
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    let service = format!("{domain}/{LAUNCH_AGENT}");
    let loaded = Command::new("/bin/launchctl")
        .args(["print", &service])
        .output()
        .map_err(|error| error.to_string())?
        .status
        .success();
    if enabled {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let exe = exe
            .to_str()
            .ok_or("The application path is not valid text.")?;
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        fs::write(&file, launch_agent_plist(exe)).map_err(|error| error.to_string())?;
        if !loaded {
            let output = Command::new("/bin/launchctl")
                .args(["bootstrap", &domain])
                .arg(&file)
                .output()
                .map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(format!(
                    "Could not enable startup at login: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
    } else {
        if loaded {
            let output = Command::new("/bin/launchctl")
                .args(["bootout", &service])
                .output()
                .map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(format!(
                    "Could not disable startup at login: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        if file.exists() {
            fs::remove_file(&file).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub(crate) fn run_tray_icon(
    show_tray: bool,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) -> Result<(), String> {
    use super::TrayAction;
    use std::time::{Duration, Instant};
    use tray_icon::{
        Icon, TrayIcon, TrayIconBuilder,
        menu::{Menu, MenuEvent, MenuId, MenuItem},
    };
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
    use winit::window::WindowId;

    struct HostMenu {
        show_tray: bool,
        overlay: crate::annotations::macos::Overlay,
        tray: Option<TrayIcon>,
        items: Vec<(MenuId, TrayAction)>,
        error: Option<String>,
        shutdown_rx: tokio::sync::watch::Receiver<bool>,
        shutdown_tx: tokio::sync::watch::Sender<bool>,
    }

    impl HostMenu {
        fn install(&mut self) -> Result<(), String> {
            let menu = Menu::new();
            for (label, action) in TrayAction::MENU {
                let item = MenuItem::new(label, true, None);
                menu.append(&item).map_err(|error| error.to_string())?;
                self.items.push((item.id().clone(), action));
            }
            let icon = Icon::from_rgba(super::tray_icon_rgba(), 16, 16)
                .map_err(|error| error.to_string())?;
            self.tray = Some(
                TrayIconBuilder::new()
                    .with_menu(Box::new(menu))
                    .with_tooltip("DeskBolt Host")
                    .with_icon(icon)
                    .build()
                    .map_err(|error| error.to_string())?,
            );
            Ok(())
        }
    }

    impl ApplicationHandler for HostMenu {
        fn resumed(&mut self, _: &ActiveEventLoop) {
            if self.show_tray && self.tray.is_none()
                && let Err(error) = self.install()
            {
                self.error = Some(error);
                self.show_tray = false;
            }
        }

        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            self.overlay.refresh();
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if let Some((_, action)) = self.items.iter().find(|(id, _)| *id == event.id) {
                    let action = *action;
                    let shutdown_tx = self.shutdown_tx.clone();
                    std::thread::spawn(move || action.run(&shutdown_tx));
                }
            }
            if *self.shutdown_rx.borrow() {
                event_loop.exit();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(
                    Instant::now() + Duration::from_millis(50),
                ));
            }
        }
    }

    let event_loop = EventLoop::builder()
        .with_activation_policy(ActivationPolicy::Accessory)
        .build()
        .map_err(|error| error.to_string())?;
    let mut host = HostMenu {
        show_tray,
        overlay: crate::annotations::macos::Overlay::default(),
        tray: None,
        items: Vec::new(),
        error: None,
        shutdown_rx,
        shutdown_tx,
    };
    event_loop
        .run_app(&mut host)
        .map_err(|error| error.to_string())?;
    host.error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    #[test]
    fn startup_plist_escapes_the_executable_path() {
        let plist = super::launch_agent_plist("/Applications/A & <B>/host");
        assert!(plist.contains("<string>/Applications/A &amp; &lt;B&gt;/host</string>"));
        assert!(plist.contains("<string>--console</string>"));
    }
}
