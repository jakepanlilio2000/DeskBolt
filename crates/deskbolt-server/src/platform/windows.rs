use std::collections::HashSet;
use std::mem;
use std::path::Path;
use std::time::Duration;

use deskbolt_proto::{
    DesktopState, KeyEvent, MonitorInfo, MouseBtn, MoveRect, PointerEvent,
};
use dxgi_capture_rs::{CaptureError, DXGIManager, FrameMetadata};
use tracing::{info, warn};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetSystemMetrics, PostMessageW, PostQuitMessage, RegisterClassW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, TranslateMessage,
    WM_CLOSE, WNDCLASSW,
};
use windows::core::w;

use crate::agent::map_norm_to_virtual_desktop;
use crate::service;
use crate::win32_util;

use super::{CaptureOutput, InputInjector, RawCapturedFrame, ScreenCapturer};

pub struct WinDxgiCapturer {
    manager: DXGIManager,
    monitor_idx: u8,
    monitors: Vec<MonitorInfo>,
    desktop_name: String,
    last_dims: (u32, u32),
}

impl ScreenCapturer for WinDxgiCapturer {
    fn monitors(&self) -> Vec<MonitorInfo> {
        self.monitors.clone()
    }

    fn set_monitor(&mut self, monitor_idx: u8) {
        self.monitor_idx = monitor_idx;
        self.manager.set_capture_source_index(monitor_idx as usize);
        self.last_dims = (0, 0);
    }

    fn current_desktop_name(&self) -> String {
        self.desktop_name.clone()
    }

    fn capture_next(&mut self, force_full: bool, _timeout_ms: u32) -> CaptureOutput {
        match self.manager.capture_frame_components_with_metadata() {
            Ok((bgra, (w, h), meta)) => {
                let w32 = w as u32;
                let h32 = h as u32;
                let dims_changed = (w32, h32) != self.last_dims && w32 > 0 && h32 > 0;
                if dims_changed {
                    self.last_dims = (w32, h32);
                    self.monitors = win32_util::enumerate_dxgi_monitors();
                }

                if force_full || dims_changed || meta.has_updates() {
                    let moves = convert_move_rects(&meta, w32, h32);
                    return CaptureOutput::Frame(RawCapturedFrame {
                        monitor_id: self.monitor_idx,
                        width: w32,
                        height: h32,
                        bgra,
                        move_rects: moves,
                        dirty_rects: meta.dirty_rects,
                        force_full: force_full || dims_changed,
                    });
                }
                CaptureOutput::Timeout
            }
            Err(CaptureError::Timeout) => {
                if force_full && let Ok((bgra, (w, h))) = self.manager.capture_frame_components() {
                    return CaptureOutput::Frame(RawCapturedFrame {
                        monitor_id: self.monitor_idx,
                        width: w as u32,
                        height: h as u32,
                        bgra,
                        move_rects: Vec::new(),
                        dirty_rects: Vec::new(),
                        force_full: true,
                    });
                }
                CaptureOutput::Timeout
            }
            Err(CaptureError::AccessLost) | Err(CaptureError::AccessDenied) => {
                info!("DXGI access lost (UAC/Winlogon/mode change); rebinding active desktop");
                if let Ok(desk) = win32_util::bind_thread_to_active_input_desktop() {
                    self.desktop_name = desk.clone();
                    return CaptureOutput::DesktopSwitched {
                        state: desktop_state_for(&desk),
                        name: desk,
                    };
                }
                CaptureOutput::Reacquire("DXGI access lost".into())
            }
            Err(CaptureError::RefreshFailure) => CaptureOutput::Reacquire("DXGI refresh failure".into()),
            Err(CaptureError::Fail(e)) => {
                warn!("DXGI capture error: {e:?}");
                CaptureOutput::Reacquire("DXGI capture failure".into())
            }
        }
    }
}

pub fn create_capturer(monitor_idx: u8) -> Result<Box<dyn ScreenCapturer>, String> {
    let desktop_name = win32_util::bind_thread_to_active_input_desktop().unwrap_or_else(|_| "Default".into());
    let mut manager = DXGIManager::new(33).map_err(|e| format!("DXGIManager::new deferred ({e})"))?;
    manager.set_capture_source_index(monitor_idx as usize);
    Ok(Box::new(WinDxgiCapturer {
        manager,
        monitor_idx,
        monitors: win32_util::enumerate_dxgi_monitors(),
        desktop_name,
        last_dims: (0, 0),
    }))
}

fn desktop_state_for(name: &str) -> DesktopState {
    if name.eq_ignore_ascii_case("Winlogon") {
        DesktopState::SecureWinlogon
    } else if name.eq_ignore_ascii_case("Disconnect") {
        DesktopState::Disconnected
    } else {
        DesktopState::Interactive
    }
}

fn convert_move_rects(meta: &FrameMetadata, w: u32, h: u32) -> Vec<MoveRect> {
    meta.move_rects
        .iter()
        .filter_map(|mv| {
            let (sx, sy) = mv.source_point;
            let (dl, dt, dr, db) = mv.destination_rect;
            let rw = dr - dl;
            let rh = db - dt;
            if sx < 0
                || sy < 0
                || dl < 0
                || dt < 0
                || rw <= 0
                || rh <= 0
                || (sx + rw) as u32 > w
                || (sy + rh) as u32 > h
                || dr as u32 > w
                || db as u32 > h
            {
                return None;
            }
            Some(MoveRect {
                src_x: sx as u16,
                src_y: sy as u16,
                dst_x: dl as u16,
                dst_y: dt as u16,
                width: rw as u16,
                height: rh as u16,
            })
        })
        .collect()
}

pub struct WinInputInjector {
    depressed: HashSet<(u16, bool)>,
}

impl InputInjector for WinInputInjector {
    fn sync_desktop(&mut self) {
        let _ = win32_util::bind_thread_to_active_input_desktop();
    }

    fn inject_pointer(&mut self, ev: PointerEvent, mon: &MonitorInfo) {
        // SAFETY: GetSystemMetrics queries virtual desktop extents; SendInput sends single valid INPUT struct.
        unsafe {
            let v_left = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let v_top = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let v_w = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let v_h = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);

            let mi = match ev {
                PointerEvent::MoveAbs { norm_x, norm_y } => {
                    let (dx, dy) = map_norm_to_virtual_desktop(norm_x, norm_y, mon, v_left, v_top, v_w, v_h);
                    MOUSEINPUT {
                        dx,
                        dy,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        time: 0,
                        dwExtraInfo: 0,
                    }
                }
                PointerEvent::Button {
                    button,
                    down,
                    norm_x,
                    norm_y,
                } => {
                    let (dx, dy) = map_norm_to_virtual_desktop(norm_x, norm_y, mon, v_left, v_top, v_w, v_h);
                    let (btn_flag, mouse_data) = match (button, down) {
                        (MouseBtn::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                        (MouseBtn::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                        (MouseBtn::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                        (MouseBtn::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                        (MouseBtn::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                        (MouseBtn::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                        (MouseBtn::X1, true) => (MOUSEEVENTF_XDOWN, 1),
                        (MouseBtn::X1, false) => (MOUSEEVENTF_XUP, 1),
                        (MouseBtn::X2, true) => (MOUSEEVENTF_XDOWN, 2),
                        (MouseBtn::X2, false) => (MOUSEEVENTF_XUP, 2),
                    };
                    MOUSEINPUT {
                        dx,
                        dy,
                        mouseData: mouse_data,
                        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | btn_flag,
                        time: 0,
                        dwExtraInfo: 0,
                    }
                }
                PointerEvent::Wheel { delta_x, delta_y } => {
                    if delta_y != 0 {
                        let inp_v = INPUT {
                            r#type: INPUT_MOUSE,
                            Anonymous: INPUT_0 {
                                mi: MOUSEINPUT {
                                    dx: 0,
                                    dy: 0,
                                    mouseData: delta_y as i32 as u32,
                                    dwFlags: MOUSEEVENTF_WHEEL,
                                    time: 0,
                                    dwExtraInfo: 0,
                                },
                            },
                        };
                        let _ = SendInput(&[inp_v], mem::size_of::<INPUT>() as i32);
                    }
                    if delta_x != 0 {
                        MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: delta_x as i32 as u32,
                            dwFlags: MOUSEEVENTF_HWHEEL,
                            time: 0,
                            dwExtraInfo: 0,
                        }
                    } else {
                        return;
                    }
                }
            };

            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 { mi },
            };
            let _ = SendInput(&[input], mem::size_of::<INPUT>() as i32);
        }
    }

    fn inject_key(&mut self, ev: KeyEvent) {
        if ev.down {
            self.depressed.insert((ev.scan_code, ev.extended));
        } else {
            self.depressed.remove(&(ev.scan_code, ev.extended));
        }

        let mut flags = KEYEVENTF_SCANCODE;
        if !ev.down {
            flags |= KEYEVENTF_KEYUP;
        }
        if ev.extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }

        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                    wScan: ev.scan_code,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        // SAFETY: valid single INPUT_KEYBOARD slice.
        unsafe {
            let _ = SendInput(&[input], mem::size_of::<INPUT>() as i32);
        }
    }

    fn release_all_keys(&mut self) {
        let keys: Vec<(u16, bool)> = self.depressed.drain().collect();
        for (scan_code, extended) in keys {
            let mut flags = KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                        wScan: scan_code,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            // SAFETY: valid single INPUT_KEYBOARD slice.
            unsafe {
                let _ = SendInput(&[input], mem::size_of::<INPUT>() as i32);
            }
        }
    }
}

pub fn create_injector() -> Box<dyn InputInjector> {
    Box::new(WinInputInjector {
        depressed: HashSet::new(),
    })
}

pub fn enumerate_monitors() -> Vec<MonitorInfo> {
    win32_util::enumerate_dxgi_monitors()
}

pub fn bind_active_input_desktop() -> Result<String, String> {
    win32_util::bind_thread_to_active_input_desktop()
}

pub fn install_service() -> Result<(), String> {
    service::install_service_with_code().map_err(|(_, msg)| msg)
}

pub fn uninstall_service() -> Result<(), String> {
    service::uninstall_service_with_code().map_err(|(_, msg)| msg)
}

pub fn install_service_with_code() -> Result<(), (i32, String)> {
    service::install_service_with_code()
}

pub fn uninstall_service_with_code() -> Result<(), (i32, String)> {
    service::uninstall_service_with_code()
}

pub fn run_service_dispatcher() -> Result<(), String> {
    service::run_service_dispatcher()
}

pub fn trigger_send_sas() -> Result<(), String> {
    win32_util::trigger_send_sas()
}

pub fn set_restrictive_acl(path: &Path) -> Result<(), String> {
    win32_util::set_restrictive_acl(path)
}

pub fn read_password_masked() -> Result<String, String> {
    win32_util::read_password_masked()
}

pub fn dpapi_protect_machine(plain: &[u8]) -> Result<Vec<u8>, String> {
    win32_util::dpapi_protect_machine(plain)
}

pub fn dpapi_unprotect_machine(cipher: &[u8]) -> Result<Vec<u8>, String> {
    win32_util::dpapi_unprotect_machine(cipher)
}

use std::net::IpAddr;

use deskbolt_proto::{PowerAction, SessionMode};
use tokio::sync::watch;

pub fn startup_permission_check() {}

pub fn show_desktop_popup(title: &str, body: &str, urgent: bool) -> Result<(), String> {
    use windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTSGetActiveConsoleSessionId, WTSSendMessageW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_TOPMOST, MESSAGEBOX_RESULT,
    };
    use windows::core::PCWSTR;

    let sid = unsafe { WTSGetActiveConsoleSessionId() };
    let title_w = win32_util::to_wide_null(title);
    let body_w = win32_util::to_wide_null(body);
    let style = MB_OK | MB_TOPMOST | if urgent { MB_ICONWARNING } else { MB_ICONINFORMATION };
    let mut resp = MESSAGEBOX_RESULT::default();
    let title_bytes = (title_w.len().saturating_sub(1) * 2) as u32;
    let body_bytes = (body_w.len().saturating_sub(1) * 2) as u32;
    // SAFETY: valid UTF-16 buffers passed synchronously/non-blocking via WTS api.
    unsafe {
        WTSSendMessageW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            sid,
            PCWSTR(title_w.as_ptr()),
            title_bytes,
            PCWSTR(body_w.as_ptr()),
            body_bytes,
            style,
            0,
            &mut resp,
            false,
        )
        .map_err(|e| format!("WTSSendMessageW failed: {e}"))?;
    }
    Ok(())
}

pub fn prompt_connection_confirmation(
    peer_ip: IpAddr,
    mode: SessionMode,
    timeout_secs: u16,
    default_allow: bool,
) -> bool {
    use windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTSGetActiveConsoleSessionId, WTSSendMessageW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONQUESTION, MB_TOPMOST, MB_YESNO, MESSAGEBOX_RESULT,
    };
    use windows::core::PCWSTR;

    let sid = unsafe { WTSGetActiveConsoleSessionId() };
    let mode_txt = match mode {
        SessionMode::FullControl => "Full Control",
        SessionMode::ViewOnly => "View Only",
        SessionMode::FileTransfer => "File Transfer",
    };
    let title_w = win32_util::to_wide_null("DeskBolt Host — Connection Request");
    let body_w = win32_util::to_wide_null(&format!(
        "Allow incoming {mode_txt} connection from {peer_ip}?\n\nAuto-resolves in {timeout_secs}s."
    ));
    let mut resp = MESSAGEBOX_RESULT::default();
    let res = unsafe {
        WTSSendMessageW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            sid,
            PCWSTR(title_w.as_ptr()),
            (title_w.len().saturating_sub(1) * 2) as u32,
            PCWSTR(body_w.as_ptr()),
            (body_w.len().saturating_sub(1) * 2) as u32,
            MB_YESNO | MB_ICONQUESTION | MB_TOPMOST,
            timeout_secs.max(3) as u32,
            &mut resp,
            true,
        )
    };
    match res {
        Ok(()) if resp == IDYES => true,
        Ok(()) if resp.0 == 32000 => default_allow, // IDTIMEOUT
        Ok(()) => false,
        Err(_) => default_allow,
    }
}

pub fn execute_power_action(
    action: PowerAction,
    force: bool,
    delay_secs: u32,
) -> Result<String, String> {
    use windows::Win32::System::Power::SetSuspendState;
    use windows::Win32::System::Shutdown::{
        EWX_FORCEIFHUNG, EWX_LOGOFF, EXIT_WINDOWS_FLAGS, ExitWindowsEx,
        InitiateSystemShutdownExW, LockWorkStation, SHTDN_REASON_MAJOR_OTHER,
    };
    use windows::core::PCWSTR;

    if matches!(
        action,
        PowerAction::Shutdown
            | PowerAction::Restart
            | PowerAction::ColdPowerOff
            | PowerAction::Sleep
            | PowerAction::Hibernate
    ) {
        let _ = enable_shutdown_privilege();
    }

    match action {
        PowerAction::Lock => {
            unsafe { LockWorkStation() }.map_err(|e| format!("LockWorkStation: {e}"))?;
            Ok("Workstation locked.".into())
        }
        PowerAction::LogOff => {
            let flags = EXIT_WINDOWS_FLAGS(EWX_LOGOFF.0 | if force { EWX_FORCEIFHUNG.0 } else { 0 });
            unsafe { ExitWindowsEx(flags, SHTDN_REASON_MAJOR_OTHER) }
                .map_err(|e| format!("ExitWindowsEx: {e}"))?;
            Ok("Log off initiated.".into())
        }
        PowerAction::Sleep => {
            let ok = unsafe { SetSuspendState(false, force, false) };
            if ok {
                Ok("System entering sleep.".into())
            } else {
                Err("SetSuspendState(sleep) denied or unsupported.".into())
            }
        }
        PowerAction::Hibernate => {
            let ok = unsafe { SetSuspendState(true, force, false) };
            if ok {
                Ok("System entering hibernate.".into())
            } else {
                Err("SetSuspendState(hibernate) denied or unsupported.".into())
            }
        }
        PowerAction::Shutdown | PowerAction::ColdPowerOff => {
            let msg = win32_util::to_wide_null("DeskBolt Administrator initiated system shutdown.");
            unsafe {
                InitiateSystemShutdownExW(
                    PCWSTR::null(),
                    PCWSTR(msg.as_ptr()),
                    delay_secs,
                    force,
                    false,
                    SHTDN_REASON_MAJOR_OTHER,
                )
            }
            .map_err(|e| format!("InitiateSystemShutdownExW: {e}"))?;
            Ok(format!("System shutdown scheduled in {delay_secs}s."))
        }
        PowerAction::Restart => {
            let msg = win32_util::to_wide_null("DeskBolt Administrator initiated system restart.");
            unsafe {
                InitiateSystemShutdownExW(
                    PCWSTR::null(),
                    PCWSTR(msg.as_ptr()),
                    delay_secs,
                    force,
                    true,
                    SHTDN_REASON_MAJOR_OTHER,
                )
            }
            .map_err(|e| format!("InitiateSystemShutdownExW(reboot): {e}"))?;
            Ok(format!("System restart scheduled in {delay_secs}s."))
        }
    }
}

fn enable_shutdown_privilege() -> Result<(), String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        SE_SHUTDOWN_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::PCWSTR;

    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(|e| format!("OpenProcessToken: {e}"))?;
        let mut luid = LUID::default();
        let res = LookupPrivilegeValueW(PCWSTR::null(), SE_SHUTDOWN_NAME, &mut luid)
            .map_err(|e| format!("LookupPrivilegeValueW: {e}"));
        if res.is_ok() {
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let _ = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
        }
        let _ = CloseHandle(token);
        res
    }
}

pub fn set_curtain_mode(enabled: bool) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SC_MONITORPOWER, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SYSCOMMAND,
    };
    let power_state: isize = if enabled { 2 } else { -1 };
    let mut unused = 0usize;
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SYSCOMMAND,
            WPARAM(SC_MONITORPOWER as usize),
            LPARAM(power_state),
            SMTO_ABORTIFHUNG,
            250,
            Some(&mut unused),
        );
    }
    Ok(())
}

pub fn set_os_autostart(enabled: bool) -> Result<(), String> {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteValueW, RegSetValueExW,
    };
    use windows::core::PCWSTR;

    let subkey = win32_util::to_wide_null(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let val_name = win32_util::to_wide_null("DeskBoltHost");
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            Some(0),
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()
        .map_err(|e| format!("RegCreateKeyExW: {e}"))?;

        let res = if enabled {
            let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
            let cmd = win32_util::to_wide_null(&format!("\"{}\" --console", exe.display()));
            let bytes = std::slice::from_raw_parts(cmd.as_ptr().cast::<u8>(), cmd.len() * 2);
            RegSetValueExW(key, PCWSTR(val_name.as_ptr()), Some(0), REG_SZ, Some(bytes))
                .ok()
                .map_err(|e| format!("RegSetValueExW: {e}"))
        } else {
            let _ = RegDeleteValueW(key, PCWSTR(val_name.as_ptr()));
            Ok(())
        };
        let _ = RegCloseKey(key);
        res
    }
}

pub fn start_tray_icon(
    shutdown_rx: watch::Receiver<bool>,
    shutdown_tx: watch::Sender<bool>,
) -> Option<std::thread::JoinHandle<()>> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Shell::{
        NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, IDI_APPLICATION, LoadIconW,
        MF_STRING, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RETURNCMD, TrackPopupMenu, WM_USER,
    };

    const WM_TRAY: u32 = WM_USER + 42;
    const WM_RBUTTONUP: usize = 0x0205;
    const WM_LBUTTONDBLCLK: usize = 0x0203;
    const CMD_SETTINGS: isize = 1001;
    const CMD_EXIT: isize = 1002;

    Some(std::thread::spawn(move || unsafe {
        struct TrayCtx {
            shutdown_tx: watch::Sender<bool>,
        }
        unsafe extern "system" fn tray_wnd_proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            use windows::Win32::UI::WindowsAndMessaging::{
                CREATESTRUCTW, GWLP_USERDATA, GetWindowLongPtrW, SetWindowLongPtrW, WM_CREATE,
            };
            unsafe {
                if msg == WM_CREATE {
                    let cs = &*(lparam.0 as *const CREATESTRUCTW);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                    return LRESULT(0);
                }
                if msg == WM_TRAY {
                    let ev = lparam.0 as usize;
                    if ev == WM_LBUTTONDBLCLK {
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(exe).arg("--settings").spawn();
                        }
                        return LRESULT(0);
                    }
                    if ev == WM_RBUTTONUP {
                        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayCtx;
                        if let Ok(menu) = CreatePopupMenu() {
                            let settings_w = win32_util::to_wide_null("Server Settings...");
                            let exit_w = win32_util::to_wide_null("Stop Host");
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING,
                                CMD_SETTINGS as usize,
                                windows::core::PCWSTR(settings_w.as_ptr()),
                            );
                            let _ = AppendMenuW(
                                menu,
                                MF_STRING,
                                CMD_EXIT as usize,
                                windows::core::PCWSTR(exit_w.as_ptr()),
                            );
                            let mut pt = POINT::default();
                            let _ = GetCursorPos(&mut pt);
                            let _ = SetForegroundWindow(hwnd);
                            let chosen = TrackPopupMenu(
                                menu,
                                TPM_RETURNCMD | TPM_BOTTOMALIGN,
                                pt.x,
                                pt.y,
                                Some(0),
                                hwnd,
                                None,
                            );
                            let _ = DestroyMenu(menu);
                            if chosen.0 as isize == CMD_SETTINGS
                                && let Ok(exe) = std::env::current_exe()
                            {
                                let _ = std::process::Command::new(exe).arg("--settings").spawn();
                            } else if chosen.0 as isize == CMD_EXIT && !ptr.is_null() {
                                let _ = (*ptr).shutdown_tx.send(true);
                                PostQuitMessage(0);
                            }
                        }
                        return LRESULT(0);
                    }
                }
                if msg == WM_CLOSE {
                    PostQuitMessage(0);
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class_name = w!("DeskBoltTrayClass");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(tray_wnd_proc),
            hInstance: hinst.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let ctx_ptr = Box::into_raw(Box::new(TrayCtx { shutdown_tx }));
        if let Ok(hwnd) = CreateWindowExW(
            Default::default(),
            class_name,
            w!("DeskBolt Host"),
            Default::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst.into()),
            Some(ctx_ptr.cast()),
        ) {
            let mut nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_MESSAGE | NIF_TIP,
                uCallbackMessage: WM_TRAY,
                ..Default::default()
            };
            if let Ok(icon) = LoadIconW(None, IDI_APPLICATION) {
                nid.hIcon = icon;
                nid.uFlags |= windows::Win32::UI::Shell::NIF_ICON;
            }
            let tip: Vec<u16> = "DeskBolt Host".encode_utf16().collect();
            let n = tip.len().min(nid.szTip.len().saturating_sub(1));
            nid.szTip[..n].copy_from_slice(&tip[..n]);
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);

            let stopper_hwnd = hwnd.0 as usize;
            let stop_watcher = std::thread::spawn(move || {
                let mut rx = shutdown_rx;
                while !*rx.borrow() {
                    std::thread::sleep(Duration::from_millis(200));
                    if rx.has_changed().unwrap_or(false) && *rx.borrow_and_update() {
                        break;
                    }
                }
                let _ = PostMessageW(Some(HWND(stopper_hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
            });

            let mut msg = Default::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            let _ = DestroyWindow(hwnd);
            let _ = stop_watcher.join();
        }
        drop(Box::from_raw(ctx_ptr));
    }))
}
