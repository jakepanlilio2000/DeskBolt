use std::collections::HashSet;
use std::ffi::CString;
use std::io::{self, BufRead};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use arboard::Clipboard;
use deskbolt_proto::{
    ClipboardSync, IpcMsg, KeyEvent, MonitorInfo, MouseBtn, PointerEvent,
};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use tokio::sync::mpsc;
use tracing::{info, warn};
use xcap::Monitor;

use super::{
    CaptureOutput, InputInjector, RawCapturedFrame, ScreenCapturer, diff_bgra_dirty_rects,
    rgba_to_bgra,
};

const MAX_CLIPBOARD_BYTES: usize = 2 * 1024 * 1024;
const DIFF_BLOCK_PX: u32 = 64;

pub fn enumerate_xcap_monitors() -> Vec<MonitorInfo> {
    let Ok(list) = Monitor::all() else {
        return Vec::new();
    };
    list.into_iter()
        .enumerate()
        .map(|(idx, m)| {
            let name = m
                .friendly_name()
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| m.name().ok())
                .unwrap_or_else(|| format!("Display {}", idx + 1));
            MonitorInfo {
                id: idx.min(u8::MAX as usize) as u8,
                name,
                is_primary: m.is_primary().unwrap_or(idx == 0),
                left: m.x().unwrap_or(0),
                top: m.y().unwrap_or(0),
                width: m.width().unwrap_or(1920),
                height: m.height().unwrap_or(1080),
                dpi_scale: m.scale_factor().unwrap_or(1.0).max(0.5),
            }
        })
        .collect()
}

pub struct XcapCapturer {
    monitor_idx: u8,
    monitors: Vec<MonitorInfo>,
    prev_bgra: Vec<u8>,
    last_dims: (u32, u32),
}

impl XcapCapturer {
    pub fn new(monitor_idx: u8) -> Result<Self, String> {
        let monitors = enumerate_xcap_monitors();
        if monitors.is_empty() {
            return Err("No active display found via desktop capture".into());
        }
        Ok(Self {
            monitor_idx,
            monitors,
            prev_bgra: Vec::new(),
            last_dims: (0, 0),
        })
    }
}

impl ScreenCapturer for XcapCapturer {
    fn monitors(&self) -> Vec<MonitorInfo> {
        self.monitors.clone()
    }

    fn set_monitor(&mut self, monitor_idx: u8) {
        self.monitor_idx = monitor_idx;
        self.prev_bgra.clear();
        self.last_dims = (0, 0);
    }

    fn capture_next(&mut self, force_full: bool, _timeout_ms: u32) -> CaptureOutput {
        let Ok(all) = Monitor::all() else {
            return CaptureOutput::Reacquire("Monitor enumeration failed".into());
        };
        if all.is_empty() {
            return CaptureOutput::Reacquire("No displays available".into());
        }
        let target = all
            .get(self.monitor_idx as usize)
            .unwrap_or(&all[0]);
        let img = match target.capture_image() {
            Ok(i) => i,
            Err(e) => return CaptureOutput::Reacquire(format!("capture_image failed: {e}")),
        };
        let w = img.width();
        let h = img.height();
        let dims_changed = (w, h) != self.last_dims && w > 0 && h > 0;
        if dims_changed {
            self.last_dims = (w, h);
            self.monitors = enumerate_xcap_monitors();
            self.prev_bgra.clear();
        }

        let rgba = img.into_raw();
        let bgra = rgba_to_bgra(&rgba);
        let full = force_full || dims_changed || self.prev_bgra.len() != bgra.len();
        let dirty_rects = if full {
            Vec::new()
        } else {
            let rects = diff_bgra_dirty_rects(&self.prev_bgra, &bgra, w, h, DIFF_BLOCK_PX);
            if rects.is_empty() {
                return CaptureOutput::Timeout;
            }
            rects
        };
        self.prev_bgra = bgra.clone();

        CaptureOutput::Frame(RawCapturedFrame {
            monitor_id: self.monitor_idx,
            width: w,
            height: h,
            bgra,
            move_rects: Vec::new(),
            dirty_rects,
            force_full: full,
        })
    }
}

pub struct EnigoInjector {
    enigo: Enigo,
    depressed: HashSet<(u16, bool)>,
}

impl EnigoInjector {
    pub fn try_new() -> Result<Self, String> {
        let enigo = Enigo::new(&Settings::default())
            .map_err(|e| format!("Enigo init failed: {e}"))?;
        Ok(Self {
            enigo,
            depressed: HashSet::new(),
        })
    }
}

impl InputInjector for EnigoInjector {
    fn inject_pointer(&mut self, ev: PointerEvent, mon: &MonitorInfo) {
        match ev {
            PointerEvent::MoveAbs { norm_x, norm_y } => {
                let (x, y) = monitor_pixel_coords(norm_x, norm_y, mon);
                if let Err(e) = self.enigo.move_mouse(x, y, Coordinate::Abs) {
                    warn!("enigo move_mouse error: {e}");
                }
            }
            PointerEvent::Button {
                button,
                down,
                norm_x,
                norm_y,
            } => {
                let (x, y) = monitor_pixel_coords(norm_x, norm_y, mon);
                let _ = self.enigo.move_mouse(x, y, Coordinate::Abs);
                let btn = match button {
                    MouseBtn::Left => Button::Left,
                    MouseBtn::Right => Button::Right,
                    MouseBtn::Middle => Button::Middle,
                    MouseBtn::X1 => Button::Back,
                    MouseBtn::X2 => Button::Forward,
                };
                let dir = if down { Direction::Press } else { Direction::Release };
                if let Err(e) = self.enigo.button(btn, dir) {
                    warn!("enigo button error: {e}");
                }
            }
            PointerEvent::Wheel { delta_x, delta_y } => {
                if delta_y != 0 {
                    let steps = (-(delta_y as i32) / 30).clamp(-20, 20);
                    let len = if steps == 0 {
                        if delta_y > 0 { -1 } else { 1 }
                    } else {
                        steps
                    };
                    let _ = self.enigo.scroll(len, Axis::Vertical);
                }
                if delta_x != 0 {
                    let steps = (delta_x as i32 / 30).clamp(-20, 20);
                    let len = if steps == 0 {
                        if delta_x > 0 { 1 } else { -1 }
                    } else {
                        steps
                    };
                    let _ = self.enigo.scroll(len, Axis::Horizontal);
                }
            }
        }
    }

    fn inject_key(&mut self, ev: KeyEvent) {
        if ev.down {
            self.depressed.insert((ev.vk_code, ev.extended));
        } else {
            self.depressed.remove(&(ev.vk_code, ev.extended));
        }
        let dir = if ev.down { Direction::Press } else { Direction::Release };
        if let Some(key) = vk_to_enigo_key(ev.vk_code) {
            if let Err(e) = self.enigo.key(key, dir) {
                warn!("enigo key error: {e}");
            }
        }
    }

    fn release_all_keys(&mut self) {
        let held: Vec<(u16, bool)> = self.depressed.drain().collect();
        for (vk, _) in held {
            if let Some(key) = vk_to_enigo_key(vk) {
                let _ = self.enigo.key(key, Direction::Release);
            }
        }
    }
}

pub fn monitor_pixel_coords(norm_x: f32, norm_y: f32, mon: &MonitorInfo) -> (i32, i32) {
    let x = mon.left + ((norm_x.clamp(0.0, 1.0) * (mon.width.max(1) - 1) as f32).round() as i32);
    let y = mon.top + ((norm_y.clamp(0.0, 1.0) * (mon.height.max(1) - 1) as f32).round() as i32);
    (x, y)
}

/// Maps Win32 virtual-key codes received from the wire protocol into cross-platform `enigo::Key`.
pub fn vk_to_enigo_key(vk: u16) -> Option<Key> {
    let k = match vk {
        0x08 => Key::Backspace,
        0x09 => Key::Tab,
        0x0D => Key::Return,
        0x10 => Key::Shift,
        0x11 => Key::Control,
        0x12 => Key::Alt,
        0x14 => Key::CapsLock,
        0x1B => Key::Escape,
        0x20 => Key::Space,
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        0x23 => Key::End,
        0x24 => Key::Home,
        0x25 => Key::LeftArrow,
        0x26 => Key::UpArrow,
        0x27 => Key::RightArrow,
        0x28 => Key::DownArrow,
        0x2E => Key::Delete,
        0x5B | 0x5C => Key::Meta,
        0xA0 => Key::LShift,
        0xA1 => Key::RShift,
        0xA2 => Key::LControl,
        0xA3 => Key::RControl,
        0xA4 | 0xA5 => Key::Alt,
        0x30..=0x39 => Key::Unicode((vk as u8) as char),
        0x41..=0x5A => Key::Unicode(((vk as u8) + 32) as char),
        0x60 => Key::Numpad0,
        0x61 => Key::Numpad1,
        0x62 => Key::Numpad2,
        0x63 => Key::Numpad3,
        0x64 => Key::Numpad4,
        0x65 => Key::Numpad5,
        0x66 => Key::Numpad6,
        0x67 => Key::Numpad7,
        0x68 => Key::Numpad8,
        0x69 => Key::Numpad9,
        0x6A => Key::Multiply,
        0x6B => Key::Add,
        0x6D => Key::Subtract,
        0x6E => Key::Decimal,
        0x6F => Key::Divide,
        0x70 => Key::F1,
        0x71 => Key::F2,
        0x72 => Key::F3,
        0x73 => Key::F4,
        0x74 => Key::F5,
        0x75 => Key::F6,
        0x76 => Key::F7,
        0x77 => Key::F8,
        0x78 => Key::F9,
        0x79 => Key::F10,
        0x7A => Key::F11,
        0x7B => Key::F12,
        0xBA => Key::Unicode(';'),
        0xBB => Key::Unicode('='),
        0xBC => Key::Unicode(','),
        0xBD => Key::Unicode('-'),
        0xBE => Key::Unicode('.'),
        0xBF => Key::Unicode('/'),
        0xC0 => Key::Unicode('`'),
        0xDB => Key::Unicode('['),
        0xDC => Key::Unicode('\\'),
        0xDD => Key::Unicode(']'),
        0xDE => Key::Unicode('\''),
        _ => return None,
    };
    Some(k)
}

pub fn run_clipboard_loop(
    rx: std::sync::mpsc::Receiver<ClipboardSync>,
    ev_tx: mpsc::Sender<IpcMsg>,
    stop: Arc<AtomicBool>,
) {
    let last_written = Arc::new(Mutex::new(None::<String>));
    let mut seq = 1u64;
    let mut last_observed = String::new();

    let writer_last = last_written.clone();
    let writer_stop = stop.clone();
    let writer = thread::spawn(move || {
        while !writer_stop.load(Ordering::SeqCst) {
            let Ok(msg) = rx.recv_timeout(Duration::from_millis(200)) else {
                continue;
            };
            let Ok(mut cb) = Clipboard::new() else {
                continue;
            };
            match msg {
                ClipboardSync::SetText { utf8_text, .. } if utf8_text.len() <= MAX_CLIPBOARD_BYTES => {
                    *writer_last.lock().unwrap() = Some(utf8_text.clone());
                    let _ = cb.set_text(utf8_text);
                }
                ClipboardSync::Clear { .. } => {
                    *writer_last.lock().unwrap() = Some(String::new());
                    let _ = cb.clear();
                }
                _ => {}
            }
        }
    });

    while !stop.load(Ordering::SeqCst) {
        if let Ok(mut cb) = Clipboard::new()
            && let Ok(text) = cb.get_text()
            && text.len() <= MAX_CLIPBOARD_BYTES
            && text != last_observed
        {
            last_observed = text.clone();
            let mut guard = last_written.lock().unwrap();
            if guard.as_deref() == Some(text.as_str()) {
                *guard = None;
            } else {
                *guard = None;
                seq = seq.saturating_add(1);
                let _ = ev_tx.blocking_send(IpcMsg::Clipboard(ClipboardSync::SetText {
                    seq,
                    utf8_text: text,
                }));
            }
        }
        thread::sleep(Duration::from_millis(250));
    }

    let _ = writer.join();
    info!("Desktop clipboard sync stopped");
}

pub fn unix_set_restrictive_acl(path: &Path) -> Result<(), String> {
    let mode: libc::mode_t = if path.is_dir() { 0o700 } else { 0o600 };
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "path contains interior NUL byte".to_string())?;
    // SAFETY: valid NUL-terminated path passed to POSIX chmod.
    let rc = unsafe { libc::chmod(c_path.as_ptr(), mode) };
    if rc != 0 {
        return Err(format!("chmod({}): {}", path.display(), io::Error::last_os_error()));
    }
    Ok(())
}

pub fn unix_read_password_masked() -> Result<String, String> {
    struct TermGuard {
        fd: libc::c_int,
        prev: libc::termios,
    }
    impl Drop for TermGuard {
        fn drop(&mut self) {
            // SAFETY: restores saved termios attributes on stdin.
            unsafe {
                let _ = libc::tcsetattr(self.fd, libc::TCSANOW, &self.prev);
            }
        }
    }

    let _guard = unsafe {
        if libc::isatty(libc::STDIN_FILENO) == 1 {
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut term) == 0 {
                let prev = term;
                term.c_lflag &= !libc::ECHO;
                let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term);
                Some(TermGuard {
                    fd: libc::STDIN_FILENO,
                    prev,
                })
            } else {
                None
            }
        } else {
            None
        }
    };

    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("failed to read password: {e}"))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}
