#[cfg(target_os = "windows")]
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
#[cfg(target_os = "windows")]
use std::thread::{self, JoinHandle};

use deskbolt_proto::KeyEvent;
#[cfg(target_os = "windows")]
use deskbolt_proto::InputMsg;
use tokio::sync::mpsc;
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
#[cfg(target_os = "windows")]
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::GetCurrentThreadId;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC_EX, MapVirtualKeyW};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId,
    HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, PostThreadMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
};

#[derive(Debug, Default)]
pub struct ModifierState {
    pub ctrl: bool,
    pub alt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDecision {
    Forward(KeyEvent),
    EscapeCapture,
    PassThrough,
}

/// Pure key translation + `Ctrl+Alt+Home` safety escape logic for Windows `WH_KEYBOARD_LL`.
#[cfg(target_os = "windows")]
pub fn translate_ll_key(
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    msg: u32,
    mods: &mut ModifierState,
) -> HookDecision {
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
    if !down && !up {
        return HookDecision::PassThrough;
    }

    match vk_code {
        0x11 | 0xA2 | 0xA3 => mods.ctrl = down,
        0x12 | 0xA4 | 0xA5 => mods.alt = down,
        0x24 if down && mods.ctrl && mods.alt => {
            mods.ctrl = false;
            mods.alt = false;
            return HookDecision::EscapeCapture;
        }
        _ => {}
    }

    let extended = (flags & LLKHF_EXTENDED.0) != 0;
    let resolved_scan = if scan_code != 0 {
        scan_code as u16
    } else {
        // SAFETY: pure lookup in active keyboard layout.
        unsafe { MapVirtualKeyW(vk_code, MAPVK_VK_TO_VSC_EX) as u16 }
    };

    HookDecision::Forward(KeyEvent {
        scan_code: resolved_scan,
        vk_code: vk_code as u16,
        down,
        extended,
    })
}

/// Maps `egui::Key` values into protocol `KeyEvent` virtual-key/scan-code pairs.
pub fn egui_key_to_key_event(key: egui::Key, down: bool) -> Option<KeyEvent> {
    use egui::Key;
    let (vk, ext): (u16, bool) = match key {
        Key::ArrowDown => (0x28, true),
        Key::ArrowLeft => (0x25, true),
        Key::ArrowRight => (0x27, true),
        Key::ArrowUp => (0x26, true),
        Key::Escape => (0x1B, false),
        Key::Tab => (0x09, false),
        Key::Backspace => (0x08, false),
        Key::Enter => (0x0D, false),
        Key::Space => (0x20, false),
        Key::Insert => (0x2D, true),
        Key::Delete => (0x2E, true),
        Key::Home => (0x24, true),
        Key::End => (0x23, true),
        Key::PageUp => (0x21, true),
        Key::PageDown => (0x22, true),
        Key::Minus => (0xBD, false),
        Key::Plus | Key::Equals => (0xBB, false),
        Key::OpenBracket => (0xDB, false),
        Key::CloseBracket => (0xDD, false),
        Key::Backslash => (0xDC, false),
        Key::Semicolon => (0xBA, false),
        Key::Quote => (0xDE, false),
        Key::Comma => (0xBC, false),
        Key::Period => (0xBE, false),
        Key::Slash => (0xBF, false),
        Key::Backtick => (0xC0, false),
        Key::A => (b'A' as u16, false),
        Key::B => (b'B' as u16, false),
        Key::C => (b'C' as u16, false),
        Key::D => (b'D' as u16, false),
        Key::E => (b'E' as u16, false),
        Key::F => (b'F' as u16, false),
        Key::G => (b'G' as u16, false),
        Key::H => (b'H' as u16, false),
        Key::I => (b'I' as u16, false),
        Key::J => (b'J' as u16, false),
        Key::K => (b'K' as u16, false),
        Key::L => (b'L' as u16, false),
        Key::M => (b'M' as u16, false),
        Key::N => (b'N' as u16, false),
        Key::O => (b'O' as u16, false),
        Key::P => (b'P' as u16, false),
        Key::Q => (b'Q' as u16, false),
        Key::R => (b'R' as u16, false),
        Key::S => (b'S' as u16, false),
        Key::T => (b'T' as u16, false),
        Key::U => (b'U' as u16, false),
        Key::V => (b'V' as u16, false),
        Key::W => (b'W' as u16, false),
        Key::X => (b'X' as u16, false),
        Key::Y => (b'Y' as u16, false),
        Key::Z => (b'Z' as u16, false),
        Key::Num0 => (b'0' as u16, false),
        Key::Num1 => (b'1' as u16, false),
        Key::Num2 => (b'2' as u16, false),
        Key::Num3 => (b'3' as u16, false),
        Key::Num4 => (b'4' as u16, false),
        Key::Num5 => (b'5' as u16, false),
        Key::Num6 => (b'6' as u16, false),
        Key::Num7 => (b'7' as u16, false),
        Key::Num8 => (b'8' as u16, false),
        Key::Num9 => (b'9' as u16, false),
        Key::F1 => (0x70, false),
        Key::F2 => (0x71, false),
        Key::F3 => (0x72, false),
        Key::F4 => (0x73, false),
        Key::F5 => (0x74, false),
        Key::F6 => (0x75, false),
        Key::F7 => (0x76, false),
        Key::F8 => (0x77, false),
        Key::F9 => (0x78, false),
        Key::F10 => (0x79, false),
        Key::F11 => (0x7A, false),
        Key::F12 => (0x7B, false),
        _ => return None,
    };

    #[cfg(target_os = "windows")]
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) as u16 };
    #[cfg(not(target_os = "windows"))]
    let scan = vk;

    Some(KeyEvent {
        scan_code: scan,
        vk_code: vk,
        down,
        extended: ext,
    })
}

#[cfg(target_os = "windows")]
struct HookShared {
    active: AtomicBool,
    escaped: AtomicBool,
    sender: Mutex<Option<mpsc::Sender<InputMsg>>>,
    mods: Mutex<ModifierState>,
    self_pid: u32,
}

#[cfg(target_os = "windows")]
static HOOK_SHARED: Mutex<Option<Arc<HookShared>>> = Mutex::new(None);

/// Focus-gated OS keyboard hook on Windows; benign no-op on other platforms.
pub struct KeyboardHookController {
    #[cfg(target_os = "windows")]
    shared: Arc<HookShared>,
    #[cfg(target_os = "windows")]
    thread_id: Arc<AtomicU32>,
    #[cfg(target_os = "windows")]
    join: Option<JoinHandle<()>>,
}

impl Default for KeyboardHookController {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyboardHookController {
    #[cfg(target_os = "windows")]
    pub fn new() -> Self {
        let shared = Arc::new(HookShared {
            active: AtomicBool::new(false),
            escaped: AtomicBool::new(false),
            sender: Mutex::new(None),
            mods: Mutex::new(ModifierState::default()),
            self_pid: std::process::id(),
        });
        *HOOK_SHARED.lock().unwrap() = Some(shared.clone());

        let tid = Arc::new(AtomicU32::new(0));
        let tid_clone = tid.clone();
        let join = thread::spawn(move || {
            // SAFETY: installs WH_KEYBOARD_LL on dedicated thread with GetMessageW pump.
            unsafe {
                tid_clone.store(GetCurrentThreadId(), Ordering::SeqCst);
                let hmod = GetModuleHandleW(None).unwrap_or_default();
                let hook: HHOOK = SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(ll_keyboard_proc),
                    Some(HINSTANCE(hmod.0)),
                    0,
                )
                .unwrap_or_default();

                let mut msg = Default::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if !hook.is_invalid() {
                    let _ = UnhookWindowsHookEx(hook);
                }
            }
        });

        Self {
            shared,
            thread_id: tid,
            join: Some(join),
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub fn new() -> Self {
        Self {}
    }

    #[cfg(target_os = "windows")]
    pub fn update_state(
        &self,
        should_capture: bool,
        tx: Option<mpsc::Sender<deskbolt_proto::InputMsg>>,
    ) -> bool {
        if self.shared.escaped.swap(false, Ordering::SeqCst) {
            self.shared.active.store(false, Ordering::SeqCst);
            *self.shared.sender.lock().unwrap() = None;
            return false;
        }
        *self.shared.sender.lock().unwrap() = tx;
        self.shared.active.store(should_capture, Ordering::SeqCst);
        should_capture
    }

    #[cfg(not(target_os = "windows"))]
    pub fn update_state(
        &self,
        _should_capture: bool,
        _tx: Option<mpsc::Sender<deskbolt_proto::InputMsg>>,
    ) -> bool {
        false
    }
}

#[cfg(target_os = "windows")]
impl Drop for KeyboardHookController {
    fn drop(&mut self) {
        self.shared.active.store(false, Ordering::SeqCst);
        let tid = self.thread_id.load(Ordering::SeqCst);
        if tid != 0 {
            // SAFETY: posts WM_QUIT to exit dedicated GetMessageW loop cleanly.
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn ll_keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: Standard WH_KEYBOARD_LL callback executing strictly non-blocking try_send.
    unsafe {
        if code >= 0
            && let Some(shared) = HOOK_SHARED.lock().ok().and_then(|g| g.clone())
            && shared.active.load(Ordering::SeqCst)
        {
            let fg = GetForegroundWindow();
            let mut fg_pid = 0u32;
            GetWindowThreadProcessId(fg, Some(&mut fg_pid));
            if fg_pid == shared.self_pid {
                let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
                let mut mods = shared.mods.lock().unwrap();
                match translate_ll_key(kb.vkCode, kb.scanCode, kb.flags.0, wparam.0 as u32, &mut mods)
                {
                    HookDecision::Forward(ev) => {
                        if let Some(tx) = shared.sender.lock().unwrap().as_ref() {
                            let _ = tx.try_send(InputMsg::Key(ev));
                        }
                        return LRESULT(1);
                    }
                    HookDecision::EscapeCapture => {
                        shared.active.store(false, Ordering::SeqCst);
                        shared.escaped.store(true, Ordering::SeqCst);
                        if let Some(tx) = shared.sender.lock().unwrap().as_ref() {
                            let _ = tx.try_send(InputMsg::ReleaseAllKeys);
                        }
                        return CallNextHookEx(None, code, wparam, lparam);
                    }
                    HookDecision::PassThrough => {}
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ll_hook_translates_extended_keys_and_ctrl_alt_home_escape() {
        #[cfg(target_os = "windows")]
        {
            let mut mods = ModifierState::default();
            let d1 = translate_ll_key(0x5B, 0x5B, LLKHF_EXTENDED.0, WM_KEYDOWN, &mut mods);
            assert!(matches!(
                d1,
                HookDecision::Forward(KeyEvent {
                    vk_code: 0x5B,
                    down: true,
                    extended: true,
                    ..
                })
            ));

            let _ = translate_ll_key(0xA2, 0x1D, 0, WM_KEYDOWN, &mut mods);
            let _ = translate_ll_key(0xA4, 0x38, 0, WM_KEYDOWN, &mut mods);
            let esc = translate_ll_key(0x24, 0x47, LLKHF_EXTENDED.0, WM_KEYDOWN, &mut mods);
            assert_eq!(esc, HookDecision::EscapeCapture);
            assert!(!mods.ctrl && !mods.alt);
        }

        let enter = egui_key_to_key_event(egui::Key::Enter, true).unwrap();
        assert_eq!(enter.vk_code, 0x0D);
        assert!(enter.down);
        assert!(!enter.extended);
    }
}
