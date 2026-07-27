//! Windows keycode <-> [`Key`] mapping.
//!
//! A keycode is the scan code the hook reports (`0x00sc`, or `0xE0sc` for
//! extended keys), else the virtual-key code (`0xF0vk`). Scan codes are
//! physical positions, layout-independent; the VK fallback only covers keys
//! whose VK is layout-independent too (F13-F20). The mapping is total in both
//! directions: every `u16` maps to a `Key` (`Unknown` as fallback), and every
//! `Key` except `Function` maps back to its keycode.

use crate::Key;

pub(crate) use crate::platform::Keycode;

/// Encodes one hook event into a keycode; the scan code is preferred, the
/// virtual-key code is the fallback for events reporting no scan code
/// (injected input, F13-F20 on common keyboards).
pub fn encode(vk: u32, scan: u32, extended: bool) -> Keycode {
    const EXTENDED: u16 = 0xE000;
    const VK_FALLBACK: u16 = 0xF000;
    match (scan, extended) {
        (0, _) => VK_FALLBACK | (vk as u16 & 0xFF),
        (_, true) => EXTENDED | (scan as u16 & 0xFF),
        (_, false) => scan as u16 & 0xFF,
    }
}

pub fn key_from_code(code: Keycode) -> Key {
    match code {
        0x01 => Key::Escape,
        0x02 => Key::Num1,
        0x03 => Key::Num2,
        0x04 => Key::Num3,
        0x05 => Key::Num4,
        0x06 => Key::Num5,
        0x07 => Key::Num6,
        0x08 => Key::Num7,
        0x09 => Key::Num8,
        0x0A => Key::Num9,
        0x0B => Key::Num0,
        0x0C => Key::Minus,
        0x0D => Key::Equal,
        0x0E => Key::Delete,
        0x0F => Key::Tab,
        0x10 => Key::KeyQ,
        0x11 => Key::KeyW,
        0x12 => Key::KeyE,
        0x13 => Key::KeyR,
        0x14 => Key::KeyT,
        0x15 => Key::KeyY,
        0x16 => Key::KeyU,
        0x17 => Key::KeyI,
        0x18 => Key::KeyO,
        0x19 => Key::KeyP,
        0x1A => Key::LeftBracket,
        0x1B => Key::RightBracket,
        0x1C => Key::Return,
        0x1D => Key::ControlLeft,
        0x1E => Key::KeyA,
        0x1F => Key::KeyS,
        0x20 => Key::KeyD,
        0x21 => Key::KeyF,
        0x22 => Key::KeyG,
        0x23 => Key::KeyH,
        0x24 => Key::KeyJ,
        0x25 => Key::KeyK,
        0x26 => Key::KeyL,
        0x27 => Key::Semicolon,
        0x28 => Key::Quote,
        0x29 => Key::BackQuote,
        0x2A => Key::ShiftLeft,
        0x2B => Key::Backslash,
        0x2C => Key::KeyZ,
        0x2D => Key::KeyX,
        0x2E => Key::KeyC,
        0x2F => Key::KeyV,
        0x30 => Key::KeyB,
        0x31 => Key::KeyN,
        0x32 => Key::KeyM,
        0x33 => Key::Comma,
        0x34 => Key::Period,
        0x35 => Key::Slash,
        0x36 => Key::ShiftRight,
        0x38 => Key::AltLeft,
        0x39 => Key::Space,
        0x3A => Key::CapsLock,
        0x3B => Key::F1,
        0x3C => Key::F2,
        0x3D => Key::F3,
        0x3E => Key::F4,
        0x3F => Key::F5,
        0x40 => Key::F6,
        0x41 => Key::F7,
        0x42 => Key::F8,
        0x43 => Key::F9,
        0x44 => Key::F10,
        0x57 => Key::F11,
        0x58 => Key::F12,
        0xE01D => Key::ControlRight,
        0xE038 => Key::AltRight,
        0xE048 => Key::ArrowUp,
        0xE04B => Key::ArrowLeft,
        0xE04D => Key::ArrowRight,
        0xE050 => Key::ArrowDown,
        0xE05B => Key::SuperLeft,
        0xE05C => Key::SuperRight,
        0xF07C => Key::F13,
        0xF07D => Key::F14,
        0xF07E => Key::F15,
        0xF07F => Key::F16,
        0xF080 => Key::F17,
        0xF081 => Key::F18,
        0xF082 => Key::F19,
        0xF083 => Key::F20,
        other => Key::Unknown(other),
    }
}

/// Reverse mapping. Kept public for completeness even though the hook layer
/// only needs `key_from_code`.
#[allow(dead_code)]
pub fn code_from_key(key: Key) -> Option<Keycode> {
    let code = match key {
        Key::Escape => 0x01,
        Key::Num1 => 0x02,
        Key::Num2 => 0x03,
        Key::Num3 => 0x04,
        Key::Num4 => 0x05,
        Key::Num5 => 0x06,
        Key::Num6 => 0x07,
        Key::Num7 => 0x08,
        Key::Num8 => 0x09,
        Key::Num9 => 0x0A,
        Key::Num0 => 0x0B,
        Key::Minus => 0x0C,
        Key::Equal => 0x0D,
        Key::Delete => 0x0E,
        Key::Tab => 0x0F,
        Key::KeyQ => 0x10,
        Key::KeyW => 0x11,
        Key::KeyE => 0x12,
        Key::KeyR => 0x13,
        Key::KeyT => 0x14,
        Key::KeyY => 0x15,
        Key::KeyU => 0x16,
        Key::KeyI => 0x17,
        Key::KeyO => 0x18,
        Key::KeyP => 0x19,
        Key::LeftBracket => 0x1A,
        Key::RightBracket => 0x1B,
        Key::Return => 0x1C,
        Key::ControlLeft => 0x1D,
        Key::KeyA => 0x1E,
        Key::KeyS => 0x1F,
        Key::KeyD => 0x20,
        Key::KeyF => 0x21,
        Key::KeyG => 0x22,
        Key::KeyH => 0x23,
        Key::KeyJ => 0x24,
        Key::KeyK => 0x25,
        Key::KeyL => 0x26,
        Key::Semicolon => 0x27,
        Key::Quote => 0x28,
        Key::BackQuote => 0x29,
        Key::ShiftLeft => 0x2A,
        Key::Backslash => 0x2B,
        Key::KeyZ => 0x2C,
        Key::KeyX => 0x2D,
        Key::KeyC => 0x2E,
        Key::KeyV => 0x2F,
        Key::KeyB => 0x30,
        Key::KeyN => 0x31,
        Key::KeyM => 0x32,
        Key::Comma => 0x33,
        Key::Period => 0x34,
        Key::Slash => 0x35,
        Key::ShiftRight => 0x36,
        Key::AltLeft => 0x38,
        Key::Space => 0x39,
        Key::CapsLock => 0x3A,
        Key::F1 => 0x3B,
        Key::F2 => 0x3C,
        Key::F3 => 0x3D,
        Key::F4 => 0x3E,
        Key::F5 => 0x3F,
        Key::F6 => 0x40,
        Key::F7 => 0x41,
        Key::F8 => 0x42,
        Key::F9 => 0x43,
        Key::F10 => 0x44,
        Key::F11 => 0x57,
        Key::F12 => 0x58,
        Key::ControlRight => 0xE01D,
        Key::AltRight => 0xE038,
        Key::ArrowUp => 0xE048,
        Key::ArrowLeft => 0xE04B,
        Key::ArrowRight => 0xE04D,
        Key::ArrowDown => 0xE050,
        Key::SuperLeft => 0xE05B,
        Key::SuperRight => 0xE05C,
        Key::F13 => 0xF07C,
        Key::F14 => 0xF07D,
        Key::F15 => 0xF07E,
        Key::F16 => 0xF07F,
        Key::F17 => 0xF080,
        Key::F18 => 0xF081,
        Key::F19 => 0xF082,
        Key::F20 => 0xF083,
        // Fn lives inside keyboard firmware and never reaches the hook.
        Key::Function => return None,
        Key::Unknown(code) => code,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_is_total() {
        for code in 0..=u16::MAX {
            let key = key_from_code(code);
            assert_eq!(
                code_from_key(key),
                Some(code),
                "roundtrip failed for {code:#06X}"
            );
        }
    }

    #[test]
    fn spot_checks() {
        assert_eq!(key_from_code(0x1D), Key::ControlLeft);
        assert_eq!(key_from_code(0xE01D), Key::ControlRight);
        assert_eq!(key_from_code(0x38), Key::AltLeft);
        assert_eq!(key_from_code(0xE038), Key::AltRight);
        assert_eq!(key_from_code(0xE05B), Key::SuperLeft);
        assert_eq!(key_from_code(0xE05C), Key::SuperRight);
        assert_eq!(key_from_code(0xF083), Key::F20);
        assert_eq!(key_from_code(0xE052), Key::Unknown(0xE052));
        assert_eq!(code_from_key(Key::Function), None);
    }

    #[test]
    fn encode_prefers_scan_code() {
        assert_eq!(encode(0x41, 0x1E, false), 0x1E);
        assert_eq!(encode(0xA3, 0x1D, true), 0xE01D);
        assert_eq!(encode(0x7C, 0, false), 0xF07C);
    }
}
