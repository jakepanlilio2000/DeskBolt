use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;

use deskbolt_proto::{KeyEvent, MonitorInfo, MouseBtn, PointerEvent};
use tracing::{info, warn};

use super::desktop_common::{
    EnigoInjector, XcapCapturer, enumerate_xcap_monitors, unix_read_password_masked,
    unix_set_restrictive_acl,
};
use super::{InputInjector, ScreenCapturer};

#[path = "linux_curtain.rs"]
mod curtain;
#[path = "annotation_x11.rs"]
mod annotation_x11;

pub(crate) fn start_annotation_overlay() -> Result<(), String> {
    if super::wayland_capture::is_wayland() {
        curtain::start_annotation()
    } else {
        annotation_x11::start()
    }
}

const SYSTEMD_UNIT_PATH: &str = "/etc/systemd/system/deskbolt-server.service";
const OPENRC_INIT_PATH: &str = "/etc/init.d/deskbolt-server";

// Linux input / uinput constants (independent of glibc vs musl headers)
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const BUS_USB: u16 = 0x03;

// _IO('U', 1/2), _IOW('U', 100/101/102/103, int), _IOW('U', 3/4, structs)
const UI_DEV_CREATE: libc::Ioctl = 0x5501 as libc::Ioctl;
const UI_DEV_DESTROY: libc::Ioctl = 0x5502 as libc::Ioctl;
const UI_SET_EVBIT: libc::Ioctl = 0x4004_5564 as libc::Ioctl;
const UI_SET_KEYBIT: libc::Ioctl = 0x4004_5565 as libc::Ioctl;
const UI_SET_RELBIT: libc::Ioctl = 0x4004_5566 as libc::Ioctl;
const UI_SET_ABSBIT: libc::Ioctl = 0x4004_5567 as libc::Ioctl;
const UI_DEV_SETUP: libc::Ioctl = 0x405C_5503 as libc::Ioctl;
const UI_ABS_SETUP: libc::Ioctl = 0x401C_5504 as libc::Ioctl;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    absinfo: InputAbsinfo,
}

#[repr(C)]
struct LinuxInputEvent {
    tv_sec: libc::time_t,
    tv_usec: libc::suseconds_t,
    type_: u16,
    code: u16,
    value: i32,
}

/// Fallback `/dev/uinput` HID device used when X11/Wayland compositor portals deny `enigo`.
struct UinputInjector {
    file: File,
    depressed: HashSet<u16>,
    desktop: Option<(i32, i32, i32, i32)>,
}

impl UinputInjector {
    fn try_open() -> Result<Self, String> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .map_err(|e| format!("open /dev/uinput: {e}"))?;
        let fd = file.as_raw_fd();

        // SAFETY: valid ioctl setup against an opened /dev/uinput descriptor.
        unsafe {
            for ev in [EV_SYN, EV_KEY, EV_REL, EV_ABS] {
                if libc::ioctl(fd, UI_SET_EVBIT, ev as libc::c_int) < 0 {
                    return Err("UI_SET_EVBIT failed".into());
                }
            }
            for btn in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                let _ = libc::ioctl(fd, UI_SET_KEYBIT, btn as libc::c_int);
            }
            for code in 1..=248 {
                let _ = libc::ioctl(fd, UI_SET_KEYBIT, code as libc::c_int);
            }
            for rel in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
                let _ = libc::ioctl(fd, UI_SET_RELBIT, rel as libc::c_int);
            }
            for abs in [ABS_X, ABS_Y] {
                let _ = libc::ioctl(fd, UI_SET_ABSBIT, abs as libc::c_int);
                let abs_cfg = UinputAbsSetup {
                    code: abs,
                    _pad: 0,
                    absinfo: InputAbsinfo {
                        value: 0,
                        minimum: 0,
                        maximum: 65535,
                        fuzz: 0,
                        flat: 0,
                        resolution: 0,
                    },
                };
                let _ = libc::ioctl(fd, UI_ABS_SETUP, &abs_cfg);
            }

            let mut setup = UinputSetup {
                id: InputId {
                    bustype: BUS_USB,
                    vendor: 0x1209,
                    product: 0x4899,
                    version: 1,
                },
                name: [0u8; 80],
                ff_effects_max: 0,
            };
            let label = b"DeskBolt Virtual Input";
            setup.name[..label.len()].copy_from_slice(label);
            if libc::ioctl(fd, UI_DEV_SETUP, &setup) < 0 || libc::ioctl(fd, UI_DEV_CREATE) < 0 {
                return Err("uinput device creation failed".into());
            }
        }
        info!("Using /dev/uinput kernel input fallback");
        Ok(Self {
            file,
            depressed: HashSet::new(),
            desktop: None,
        })
    }

    fn emit(&mut self, type_: u16, code: u16, value: i32) {
        let ev = LinuxInputEvent {
            tv_sec: 0,
            tv_usec: 0,
            type_,
            code,
            value,
        };
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                (&ev as *const LinuxInputEvent).cast::<u8>(),
                std::mem::size_of::<LinuxInputEvent>(),
            )
        };
        let _ = self.file.write_all(bytes);
    }

    fn sync(&mut self) {
        self.emit(EV_SYN, SYN_REPORT, 0);
    }
}

impl Drop for UinputInjector {
    fn drop(&mut self) {
        // SAFETY: destroys created uinput virtual device before file close.
        unsafe {
            let _ = libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

fn monitor_extent(monitors: &[MonitorInfo]) -> Option<(i32,i32,i32,i32)> {
    let left = monitors.iter().map(|monitor| monitor.left).min()?;
    let top = monitors.iter().map(|monitor| monitor.top).min()?;
    let right = monitors.iter().map(|monitor| i64::from(monitor.left) + i64::from(monitor.width)).max()?;
    let bottom = monitors.iter().map(|monitor| i64::from(monitor.top) + i64::from(monitor.height)).max()?;
    Some((left, top, i32::try_from(right - i64::from(left)).ok()?, i32::try_from(bottom - i64::from(top)).ok()?))
}

impl InputInjector for UinputInjector {
    fn update_monitor_layout(&mut self, monitors: &[MonitorInfo]) {
        self.desktop = monitor_extent(monitors);
    }

    fn inject_pointer(&mut self, ev: PointerEvent, mon: &MonitorInfo) {
        let (left, top, width, height) = self.desktop.unwrap_or((mon.left, mon.top, mon.width as i32, mon.height as i32));
        let absolute = |x, y| crate::agent::map_norm_to_virtual_desktop(x, y, mon, left, top, width, height);
        match ev {
            PointerEvent::MoveAbs { norm_x, norm_y } => {
                let (ax, ay) = absolute(norm_x, norm_y);
                self.emit(EV_ABS, ABS_X, ax);
                self.emit(EV_ABS, ABS_Y, ay);
                self.sync();
            }
            PointerEvent::Button {
                button,
                down,
                norm_x,
                norm_y,
            } => {
                let (ax, ay) = absolute(norm_x, norm_y);
                self.emit(EV_ABS, ABS_X, ax);
                self.emit(EV_ABS, ABS_Y, ay);
                let code = match button {
                    MouseBtn::Left => BTN_LEFT,
                    MouseBtn::Right => BTN_RIGHT,
                    MouseBtn::Middle => BTN_MIDDLE,
                    MouseBtn::X1 => BTN_SIDE,
                    MouseBtn::X2 => BTN_EXTRA,
                };
                self.emit(EV_KEY, code, i32::from(down));
                self.sync();
            }
            PointerEvent::Wheel { delta_x, delta_y } => {
                if delta_y != 0 {
                    self.emit(EV_REL, REL_WHEEL, (delta_y as i32 / 120).clamp(-8, 8));
                }
                if delta_x != 0 {
                    self.emit(EV_REL, REL_HWHEEL, (delta_x as i32 / 120).clamp(-8, 8));
                }
                self.sync();
            }
        }
    }

    fn inject_key(&mut self, ev: KeyEvent) {
        if let Some(code) = vk_to_linux_evdev_code(ev.vk_code) {
            if ev.down {
                self.depressed.insert(code);
            } else {
                self.depressed.remove(&code);
            }
            self.emit(EV_KEY, code, i32::from(ev.down));
            self.sync();
        }
    }

    fn release_all_keys(&mut self) {
        let keys: Vec<u16> = self.depressed.drain().collect();
        for code in keys {
            self.emit(EV_KEY, code, 0);
        }
        self.sync();
    }
}

struct NoopInjector;
impl InputInjector for NoopInjector {
    fn inject_pointer(&mut self, _ev: PointerEvent, _mon: &MonitorInfo) {}
    fn inject_key(&mut self, _ev: KeyEvent) {}
    fn release_all_keys(&mut self) {}
}

pub fn create_capturer(monitor_idx: u8) -> Result<Box<dyn ScreenCapturer>, String> {
    let wayland = super::wayland_capture::is_wayland();
    if wayland {
        match super::wayland_capture::WaylandCapturer::new(monitor_idx, enumerate_monitors().is_empty()) {
            Ok(capture) => return Ok(Box::new(capture)),
            Err(error) => warn!("Screen stream unavailable: {error}"),
        }
    }
    match XcapCapturer::new(monitor_idx) {
        Ok(capture) => Ok(Box::new(capture)),
        Err(error) if wayland => Err(error),
        Err(_) => Ok(Box::new(super::x11_framebuffer::X11Framebuffer::open()?)),
    }
}

pub fn create_injector() -> Box<dyn InputInjector> {
    match EnigoInjector::try_new() {
        Ok(inj) => Box::new(inj),
        Err(e) => {
            warn!("{e}; attempting /dev/uinput fallback");
            match UinputInjector::try_open() {
                Ok(u) => Box::new(u),
                Err(ue) => {
                    warn!("{ue}; remote input injection disabled until input session is available");
                    Box::new(NoopInjector)
                }
            }
        }
    }
}

pub fn enumerate_monitors() -> Vec<MonitorInfo> {
    enumerate_xcap_monitors()
}

pub fn bind_active_input_desktop() -> Result<String, String> {
    Ok(std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "Default".into()))
}

fn is_openrc_host() -> bool {
    !Path::new("/run/systemd/system").exists()
        && (Path::new("/sbin/rc-service").exists() || Path::new("/usr/sbin/rc-service").exists())
}

pub fn install_service() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    if is_openrc_host() {
        let script = format!(
            "#!/sbin/openrc-run\n\
             name=\"deskbolt-server\"\n\
             description=\"DeskBolt Remote Host\"\n\
             command=\"{}\"\n\
             command_args=\"--service\"\n\
             command_background=\"yes\"\n\
             pidfile=\"/run/deskbolt-server.pid\"\n\n\
             depend() {{\n\
                 need net\n\
             }}\n",
            exe.display()
        );
        fs::write(OPENRC_INIT_PATH, script)
            .map_err(|e| format!("write {OPENRC_INIT_PATH} (run with sudo/root): {e}"))?;
        let c = std::ffi::CString::new(OPENRC_INIT_PATH).unwrap();
        // SAFETY: makes OpenRC init script executable.
        unsafe { libc::chmod(c.as_ptr(), 0o755) };
        let _ = Command::new("rc-update").args(["add", "deskbolt-server", "default"]).status();
        let _ = Command::new("rc-service").args(["deskbolt-server", "start"]).status();
        println!("Installed OpenRC service at {OPENRC_INIT_PATH}.");
        return Ok(());
    }

    let unit = format!(
        "[Unit]\n\
         Description=DeskBolt Remote Host\n\
         After=network-online.target graphical.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} --service\n\
         Restart=always\n\
         RestartSec=3\n\
         LimitNOFILE=65536\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        exe.display()
    );
    fs::write(SYSTEMD_UNIT_PATH, unit)
        .map_err(|e| format!("write {SYSTEMD_UNIT_PATH} (run with sudo/root): {e}"))?;
    let _ = Command::new("systemctl").args(["daemon-reload"]).status();
    let st = Command::new("systemctl")
        .args(["enable", "--now", "deskbolt-server"])
        .status()
        .map_err(|e| format!("systemctl enable failed: {e}"))?;
    if !st.success() {
        return Err("systemctl enable --now deskbolt-server failed".into());
    }
    println!("Installed and started systemd service at {SYSTEMD_UNIT_PATH}.");
    Ok(())
}

pub fn uninstall_service() -> Result<(), String> {
    if is_openrc_host() {
        let _ = Command::new("rc-service").args(["deskbolt-server", "stop"]).status();
        let _ = Command::new("rc-update").args(["del", "deskbolt-server", "default"]).status();
        if Path::new(OPENRC_INIT_PATH).exists() {
            fs::remove_file(OPENRC_INIT_PATH)
                .map_err(|e| format!("remove {OPENRC_INIT_PATH}: {e}"))?;
        }
        println!("Removed OpenRC service {OPENRC_INIT_PATH}.");
        return Ok(());
    }

    let _ = Command::new("systemctl").args(["disable", "--now", "deskbolt-server"]).status();
    if Path::new(SYSTEMD_UNIT_PATH).exists() {
        fs::remove_file(SYSTEMD_UNIT_PATH)
            .map_err(|e| format!("remove {SYSTEMD_UNIT_PATH}: {e}"))?;
    }
    let _ = Command::new("systemctl").args(["daemon-reload"]).status();
    println!("Removed systemd service {SYSTEMD_UNIT_PATH}.");
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
    Err("DPAPI not applicable on Linux".into())
}

pub fn dpapi_unprotect_machine(_cipher: &[u8]) -> Result<Vec<u8>, String> {
    Err("DPAPI not applicable on Linux".into())
}

use std::net::IpAddr;

use deskbolt_proto::{PowerAction, SessionMode};
use tokio::sync::watch;

pub fn startup_permission_check() {}

pub fn show_desktop_popup(title: &str, body: &str, urgent: bool) -> Result<(), String> {
    notification_runtime()?.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let bus = ashpd::zbus::Connection::session().await?;
            let proxy = notifications(&bus).await?;
            let body = body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            notify(&proxy, 0, title, &body, &[], if urgent { 2 } else { 1 }, -1).await
        })
        .await
        .map_err(|_| "The desktop notification service did not respond.".to_string())?
        .map(|_| ())
        .map_err(|error| error.to_string())
    })
}

pub fn prompt_connection_confirmation(
    peer_ip: IpAddr,
    mode: SessionMode,
    timeout_secs: u16,
    default_allow: bool,
) -> bool {
    use ashpd::zbus::export::futures_core::Stream;
    use std::future::poll_fn;
    use std::time::Duration;
    use tokio::time::{Instant, timeout_at};

    let Ok(runtime) = notification_runtime() else {
        return default_allow;
    };
    runtime.block_on(async {
        let deadline = Instant::now() + Duration::from_secs(u64::from(timeout_secs));
        let Ok(Ok((bus, mut actions, mut closed))) = timeout_at(deadline, async {
            let bus = ashpd::zbus::Connection::session().await?;
            let proxy = notifications(&bus).await?;
            let capabilities: Vec<String> = proxy.call("GetCapabilities", &()).await?;
            if !capabilities.iter().any(|name| name == "actions") {
                return Err(ashpd::zbus::Error::Failure("Desktop actions are unavailable".into()));
            }
            let actions = proxy.receive_signal("ActionInvoked").await?;
            let closed = proxy.receive_signal("NotificationClosed").await?;
            Ok::<_, ashpd::zbus::Error>((bus, actions, closed))
        }).await else {
            return default_allow;
        };
        let Ok(Ok(proxy)) = timeout_at(deadline, notifications(&bus)).await else {
            return default_allow;
        };
        let mut id = 0;
        let result = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break default_allow;
            }
            let seconds = remaining.as_secs_f64().ceil() as u64;
            let message = super::connection_prompt_text(peer_ip, mode, seconds);
            let shown = timeout_at(deadline, notify(
                &proxy, id, "DeskBolt connection request", &message,
                &["allow", "Allow", "decline", "Decline"], 2, 0,
            )).await;
            let Ok(Ok(new_id)) = shown else { break default_allow };
            id = new_id;
            let tick = tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_secs(1)));
            tokio::pin!(tick);
            loop {
                tokio::select! {
                    biased;
                    action = poll_fn(|cx| std::pin::Pin::new(&mut actions).poll_next(cx)) => {
                        let Some(action) = action else { break };
                        if let Ok((notification, name)) = action.body().deserialize::<(u32, String)>()
                            && notification == id
                            && matches!(name.as_str(), "allow" | "decline")
                        {
                            let _ = tokio::time::timeout(Duration::from_millis(250),
                                proxy.call::<_, _, ()>("CloseNotification", &(id,))).await;
                            return name == "allow";
                        }
                    }
                    close = poll_fn(|cx| std::pin::Pin::new(&mut closed).poll_next(cx)) => {
                        let Some(close) = close else { break };
                        if let Ok((notification, reason)) = close.body().deserialize::<(u32, u32)>()
                            && notification == id
                        {
                            return reason == 1 && default_allow;
                        }
                    }
                    _ = &mut tick => break,
                }
            }
        };
        if id != 0 {
            let _ = tokio::time::timeout(Duration::from_millis(250),
                proxy.call::<_, _, ()>("CloseNotification", &(id,))).await;
        }
        result
    })
}

fn notification_runtime() -> Result<tokio::runtime::Runtime, String> {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
        && std::env::var_os("XDG_RUNTIME_DIR").is_none()
    {
        return Err("A desktop session is required.".into());
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|error| error.to_string())
}

async fn notifications(connection: &ashpd::zbus::Connection) -> ashpd::zbus::Result<ashpd::zbus::Proxy<'_>> {
    ashpd::zbus::Proxy::new(connection, "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications", "org.freedesktop.Notifications").await
}

// The desktop notification wire format requires all eight fields.
#[allow(clippy::too_many_arguments)]
async fn notify(
    proxy: &ashpd::zbus::Proxy<'_>, replaced: u32, title: &str, body: &str,
    actions: &[&str], urgency: u8, expire_ms: i32,
) -> ashpd::zbus::Result<u32> {
    use ashpd::zbus::Variant;
    let hints = std::collections::HashMap::from([("urgency", Variant::U8(urgency))]);
    proxy.call("Notify", &("DeskBolt", replaced, "", title, body, actions, hints, expire_ms)).await
}

pub fn execute_power_action(
    action: PowerAction,
    _force: bool,
    _delay_secs: u32,
) -> Result<String, String> {
    let method = match action {
        PowerAction::Shutdown | PowerAction::ColdPowerOff => "PowerOff",
        PowerAction::Restart => "Reboot",
        PowerAction::Sleep => "Suspend",
        PowerAction::Hibernate => "Hibernate",
        PowerAction::LogOff | PowerAction::Lock => {
            return Err("Desktop session lock/logoff requires an active display manager session on Linux.".into());
        }
    };
    crate::sysadmin::call_systemd_login_power(method)
}

pub fn set_curtain_mode(enabled: bool) -> Result<(), String> {
    curtain::set(enabled)
}

pub fn set_os_autostart(enabled: bool) -> Result<(), String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let dir = std::path::PathBuf::from(home).join(".config").join("autostart");
    let file = dir.join("deskbolt-server.desktop");
    if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let _ = std::fs::create_dir_all(&dir);
        let content = format!(
            "[Desktop Entry]\nType=Application\nName=DeskBolt Host\nExec=\"{}\" --console\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        );
        std::fs::write(&file, content).map_err(|e| format!("write {}: {e}", file.display()))?;
    } else if file.exists() {
        let _ = std::fs::remove_file(&file);
    }
    Ok(())
}

pub fn start_tray_icon(
    mut shutdown_rx: watch::Receiver<bool>,
    shutdown_tx: watch::Sender<bool>,
) -> Option<std::thread::JoinHandle<()>> {
    use ksni::{Tray, TrayMethods, menu::StandardItem};
    use super::TrayAction;
    struct HostTray(watch::Sender<bool>);
    impl Tray for HostTray {
        fn id(&self) -> String { "deskbolt-host".into() }
        fn title(&self) -> String { "DeskBolt Host".into() }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            let mut pixels = super::tray_icon_rgba();
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel.rotate_right(1);
            }
            vec![ksni::Icon { width: 16, height: 16, data: pixels }]
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            TrayAction::MENU.into_iter().map(|(label, action)| {
                StandardItem {
                    label: label.into(),
                    activate: Box::new(move |tray: &mut Self| {
                        let shutdown = tray.0.clone();
                        tokio::task::spawn_blocking(move || action.run(&shutdown));
                    }),
                    ..Default::default()
                }.into()
            }).collect()
        }
    }
    let runtime = notification_runtime().ok()?;
    Some(std::thread::spawn(move || {
        runtime.block_on(async {
            let started = tokio::time::timeout(std::time::Duration::from_secs(3), HostTray(shutdown_tx).spawn()).await;
            if let Ok(Ok(tray)) = started {
                let _ = shutdown_rx.wait_for(|stopping| *stopping).await;
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), tray.shutdown()).await;
            }
        });
    }))
}

fn vk_to_linux_evdev_code(vk: u16) -> Option<u16> {
    let code = match vk {
        0x1B => 1,   // KEY_ESC
        0x31 => 2,   // KEY_1
        0x32 => 3,
        0x33 => 4,
        0x34 => 5,
        0x35 => 6,
        0x36 => 7,
        0x37 => 8,
        0x38 => 9,
        0x39 => 10,
        0x30 => 11,  // KEY_0
        0xBD => 12,  // KEY_MINUS
        0xBB => 13,  // KEY_EQUAL
        0x08 => 14,  // KEY_BACKSPACE
        0x09 => 15,  // KEY_TAB
        0x51 => 16,  // KEY_Q
        0x57 => 17,  // KEY_W
        0x45 => 18,  // KEY_E
        0x52 => 19,  // KEY_R
        0x54 => 20,  // KEY_T
        0x59 => 21,  // KEY_Y
        0x55 => 22,  // KEY_U
        0x49 => 23,  // KEY_I
        0x4F => 24,  // KEY_O
        0x50 => 25,  // KEY_P
        0xDB => 26,  // KEY_LEFTBRACE
        0xDD => 27,  // KEY_RIGHTBRACE
        0x0D => 28,  // KEY_ENTER
        0x11 | 0xA2 => 29,  // KEY_LEFTCTRL
        0x41 => 30,  // KEY_A
        0x53 => 31,  // KEY_S
        0x44 => 32,  // KEY_D
        0x46 => 33,  // KEY_F
        0x47 => 34,  // KEY_G
        0x48 => 35,  // KEY_H
        0x4A => 36,  // KEY_J
        0x4B => 37,  // KEY_K
        0x4C => 38,  // KEY_L
        0xBA => 39,  // KEY_SEMICOLON
        0xDE => 40,  // KEY_APOSTROPHE
        0xC0 => 41,  // KEY_GRAVE
        0x10 | 0xA0 => 42,  // KEY_LEFTSHIFT
        0xDC => 43,  // KEY_BACKSLASH
        0x5A => 44,  // KEY_Z
        0x58 => 45,  // KEY_X
        0x43 => 46,  // KEY_C
        0x56 => 47,  // KEY_V
        0x42 => 48,  // KEY_B
        0x4E => 49,  // KEY_N
        0x4D => 50,  // KEY_M
        0xBC => 51,  // KEY_COMMA
        0xBE => 52,  // KEY_DOT
        0xBF => 53,  // KEY_SLASH
        0xA1 => 54,  // KEY_RIGHTSHIFT
        0x12 | 0xA4 => 56,  // KEY_LEFTALT
        0x20 => 57,  // KEY_SPACE
        0x14 => 58,  // KEY_CAPSLOCK
        0x70 => 59,  // KEY_F1
        0x71 => 60,
        0x72 => 61,
        0x73 => 62,
        0x74 => 63,
        0x75 => 64,
        0x76 => 65,
        0x77 => 66,
        0x78 => 67,
        0x79 => 68,  // KEY_F10
        0x7A => 87,  // KEY_F11
        0x7B => 88,  // KEY_F12
        0xA3 => 97,  // KEY_RIGHTCTRL
        0xA5 => 100, // KEY_RIGHTALT
        0x24 => 102, // KEY_HOME
        0x26 => 103, // KEY_UP
        0x21 => 104, // KEY_PAGEUP
        0x25 => 105, // KEY_LEFT
        0x27 => 106, // KEY_RIGHT
        0x23 => 107, // KEY_END
        0x28 => 108, // KEY_DOWN
        0x22 => 109, // KEY_PAGEDOWN
        0x2D => 110, // KEY_INSERT
        0x2E => 111, // KEY_DELETE
        0x5B => 125, // KEY_LEFTMETA
        0x5C => 126, // KEY_RIGHTMETA
        _ => return None,
    };
    Some(code)
}
