use deskbolt_proto::{InputMsg, KeyEvent};
use egui::Key;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypingHotkey {
    pub key: Key,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl TypingHotkey {
    pub fn parse(spec: &str) -> Option<Self> {
        let mut parts = spec.split('+').map(str::trim);
        let name = parts.next_back()?;
        let key = Key::ALL.iter().copied()
            .find(|key| key.name().eq_ignore_ascii_case(name))
            .or_else(|| Key::from_name(name))?;
        crate::hook::egui_key_to_key_event(key, true)?;
        let mut hotkey = Self { key, ctrl: false, alt: false, shift: false };
        for part in parts {
            let modifier = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut hotkey.ctrl,
                "alt" => &mut hotkey.alt,
                "shift" => &mut hotkey.shift,
                _ => return None,
            };
            if *modifier {
                return None;
            }
            *modifier = true;
        }
        if !(hotkey.ctrl || hotkey.alt || hotkey.shift) || (hotkey.ctrl && hotkey.alt && key == Key::Home) {
            return None;
        }
        Some(hotkey)
    }
}

pub fn character_events(ch: char) -> Vec<InputMsg> {
    let mut shifted = false;
    let (scan, vk) = match ch {
        '\n' | '\r' => (0x1c, 0x0d),
        '\t' => (0x0f, 0x09),
        ' ' => (0x39, 0x20),
        '\x60' | '~' => {
            shifted = ch == '~';
            (0x29, 0xc0)
        }
        ch if ch.is_ascii_graphic() => {
            const ROWS: &[(u16, &[u8], &[u8])] = &[
                (0x02, b"1234567890-=", b"!@#$%^&*()_+"),
                (0x10, b"qwertyuiop[]", b"QWERTYUIOP{}"),
                (0x1e, b"asdfghjkl;'", b"ASDFGHJKL:\""),
                (0x2b, b"\\zxcvbnm,./", b"|ZXCVBNM<>?"),
            ];
            let Some((scan, base, add_shift)) = ROWS.iter().find_map(|&(start, plain, shift)| {
                if let Some(index) = plain.iter().position(|&key| key == ch as u8) {
                    Some((start + index as u16, plain[index], false))
                } else {
                    shift.iter().position(|&key| key == ch as u8)
                        .map(|index| (start + index as u16, plain[index], true))
                }
            }) else {
                return Vec::new();
            };
            shifted = add_shift;
            let vk = match base {
                b';' => 0xba,
                b'=' => 0xbb,
                b',' => 0xbc,
                b'-' => 0xbd,
                b'.' => 0xbe,
                b'/' => 0xbf,
                b'[' => 0xdb,
                b'\\' => 0xdc,
                b']' => 0xdd,
                b'\'' => 0xde,
                base => u16::from(base.to_ascii_uppercase()),
            };
            (scan, vk)
        }
        ch if !ch.is_control() => return vec![InputMsg::Unicode(ch)],
        _ => return Vec::new(),
    };
    let key = |scan_code, vk_code, down| InputMsg::Key(KeyEvent { scan_code, vk_code, down, extended: false });
    if shifted {
        vec![key(0x2a, 0x10, true), key(scan, vk, true), key(scan, vk, false), key(0x2a, 0x10, false)]
    } else {
        vec![key(scan, vk, true), key(scan, vk, false)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_balanced_scan_codes_for_ascii_and_native_unicode() {
        assert_eq!(character_events('a'), [
            InputMsg::Key(KeyEvent { scan_code: 0x1e, vk_code: 0x41, down: true, extended: false }),
            InputMsg::Key(KeyEvent { scan_code: 0x1e, vk_code: 0x41, down: false, extended: false }),
        ]);
        assert_eq!(character_events('@'), [
            InputMsg::Key(KeyEvent { scan_code: 0x2a, vk_code: 0x10, down: true, extended: false }),
            InputMsg::Key(KeyEvent { scan_code: 0x03, vk_code: 0x32, down: true, extended: false }),
            InputMsg::Key(KeyEvent { scan_code: 0x03, vk_code: 0x32, down: false, extended: false }),
            InputMsg::Key(KeyEvent { scan_code: 0x2a, vk_code: 0x10, down: false, extended: false }),
        ]);
        assert!((b' '..=b'~').all(|ch| !character_events(char::from(ch)).is_empty()));
        assert_eq!(character_events('é'), [InputMsg::Unicode('é')]);
        assert_eq!(character_events('🧪'), [InputMsg::Unicode('🧪')]);
        assert!(character_events('\0').is_empty());
    }

    #[test]
    fn accepts_custom_combinations_and_preserves_capture_escape() {
        assert_eq!(TypingHotkey::parse("Ctrl+Shift+V"), Some(TypingHotkey {
            key: Key::V, ctrl: true, alt: false, shift: true,
        }));
        assert!(TypingHotkey::parse("Alt+F9").is_some());
        assert!(TypingHotkey::parse("V").is_none());
        assert!(TypingHotkey::parse("Ctrl+Alt+Home").is_none());
    }
}
