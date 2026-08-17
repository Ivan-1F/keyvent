//! macOS keycode <-> [`Key`] mapping.
//!
//! Keycode values are OS facts (see `IOHIDSystem/ev_keymap.h`); the mapping is
//! total in both directions: every `u16` maps to a `Key` (`Unknown` as
//! fallback), and every `Key` maps back to its keycode.

use crate::Key;

pub(crate) use crate::platform::Keycode;

/// Keycode of the Caps Lock key.
pub(crate) const CAPS_LOCK: Keycode = 57;
/// Keycode of the action echo macOS emits around the caps-lock action
/// (input-source switch / lock engage). Not a physical key.
pub(crate) const CAPS_ACTION_ECHO: Keycode = 255;

pub fn key_from_code(code: Keycode) -> Key {
    match code {
        0 => Key::KeyA,
        1 => Key::KeyS,
        2 => Key::KeyD,
        3 => Key::KeyF,
        4 => Key::KeyH,
        5 => Key::KeyG,
        6 => Key::KeyZ,
        7 => Key::KeyX,
        8 => Key::KeyC,
        9 => Key::KeyV,
        11 => Key::KeyB,
        12 => Key::KeyQ,
        13 => Key::KeyW,
        14 => Key::KeyE,
        15 => Key::KeyR,
        16 => Key::KeyY,
        17 => Key::KeyT,
        18 => Key::Num1,
        19 => Key::Num2,
        20 => Key::Num3,
        21 => Key::Num4,
        22 => Key::Num6,
        23 => Key::Num5,
        24 => Key::Equal,
        25 => Key::Num9,
        26 => Key::Num7,
        27 => Key::Minus,
        28 => Key::Num8,
        29 => Key::Num0,
        30 => Key::RightBracket,
        31 => Key::KeyO,
        32 => Key::KeyU,
        33 => Key::LeftBracket,
        34 => Key::KeyI,
        35 => Key::KeyP,
        36 => Key::Return,
        37 => Key::KeyL,
        38 => Key::KeyJ,
        39 => Key::Quote,
        40 => Key::KeyK,
        41 => Key::Semicolon,
        42 => Key::Backslash,
        43 => Key::Comma,
        44 => Key::Slash,
        45 => Key::KeyN,
        46 => Key::KeyM,
        47 => Key::Period,
        48 => Key::Tab,
        49 => Key::Space,
        50 => Key::BackQuote,
        51 => Key::Delete,
        53 => Key::Escape,
        54 => Key::SuperRight,
        55 => Key::SuperLeft,
        56 => Key::ShiftLeft,
        57 => Key::CapsLock,
        58 => Key::AltLeft,
        59 => Key::ControlLeft,
        60 => Key::ShiftRight,
        61 => Key::AltRight,
        62 => Key::ControlRight,
        63 => Key::Function,
        64 => Key::F17,
        79 => Key::F18,
        80 => Key::F19,
        90 => Key::F20,
        96 => Key::F5,
        97 => Key::F6,
        98 => Key::F7,
        99 => Key::F3,
        100 => Key::F8,
        101 => Key::F9,
        103 => Key::F11,
        105 => Key::F13,
        106 => Key::F16,
        107 => Key::F14,
        109 => Key::F10,
        111 => Key::F12,
        113 => Key::F15,
        118 => Key::F4,
        120 => Key::F2,
        122 => Key::F1,
        123 => Key::ArrowLeft,
        124 => Key::ArrowRight,
        125 => Key::ArrowDown,
        126 => Key::ArrowUp,
        other => Key::Unknown(other),
    }
}

/// Reverse mapping. Kept public for completeness even though the tap layer
/// only needs `key_from_code`.
#[allow(dead_code)]
pub fn code_from_key(key: Key) -> Option<Keycode> {
    let code = match key {
        Key::KeyA => 0,
        Key::KeyS => 1,
        Key::KeyD => 2,
        Key::KeyF => 3,
        Key::KeyH => 4,
        Key::KeyG => 5,
        Key::KeyZ => 6,
        Key::KeyX => 7,
        Key::KeyC => 8,
        Key::KeyV => 9,
        Key::KeyB => 11,
        Key::KeyQ => 12,
        Key::KeyW => 13,
        Key::KeyE => 14,
        Key::KeyR => 15,
        Key::KeyY => 16,
        Key::KeyT => 17,
        Key::Num1 => 18,
        Key::Num2 => 19,
        Key::Num3 => 20,
        Key::Num4 => 21,
        Key::Num6 => 22,
        Key::Num5 => 23,
        Key::Equal => 24,
        Key::Num9 => 25,
        Key::Num7 => 26,
        Key::Minus => 27,
        Key::Num8 => 28,
        Key::Num0 => 29,
        Key::RightBracket => 30,
        Key::KeyO => 31,
        Key::KeyU => 32,
        Key::LeftBracket => 33,
        Key::KeyI => 34,
        Key::KeyP => 35,
        Key::Return => 36,
        Key::KeyL => 37,
        Key::KeyJ => 38,
        Key::Quote => 39,
        Key::KeyK => 40,
        Key::Semicolon => 41,
        Key::Backslash => 42,
        Key::Comma => 43,
        Key::Slash => 44,
        Key::KeyN => 45,
        Key::KeyM => 46,
        Key::Period => 47,
        Key::Tab => 48,
        Key::Space => 49,
        Key::BackQuote => 50,
        Key::Delete => 51,
        Key::Escape => 53,
        Key::SuperRight => 54,
        Key::SuperLeft => 55,
        Key::ShiftLeft => 56,
        Key::CapsLock => 57,
        Key::AltLeft => 58,
        Key::ControlLeft => 59,
        Key::ShiftRight => 60,
        Key::AltRight => 61,
        Key::ControlRight => 62,
        Key::Function => 63,
        Key::F17 => 64,
        Key::F18 => 79,
        Key::F19 => 80,
        Key::F20 => 90,
        Key::F5 => 96,
        Key::F6 => 97,
        Key::F7 => 98,
        Key::F3 => 99,
        Key::F8 => 100,
        Key::F9 => 101,
        Key::F11 => 103,
        Key::F13 => 105,
        Key::F16 => 106,
        Key::F14 => 107,
        Key::F10 => 109,
        Key::F12 => 111,
        Key::F15 => 113,
        Key::F4 => 118,
        Key::F2 => 120,
        Key::F1 => 122,
        Key::ArrowLeft => 123,
        Key::ArrowRight => 124,
        Key::ArrowDown => 125,
        Key::ArrowUp => 126,
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
                "roundtrip failed for {code}"
            );
        }
    }

    #[test]
    fn spot_checks() {
        assert_eq!(key_from_code(63), Key::Function);
        assert_eq!(key_from_code(55), Key::SuperLeft);
        assert_eq!(key_from_code(54), Key::SuperRight);
        assert_eq!(key_from_code(56), Key::ShiftLeft);
        assert_eq!(key_from_code(60), Key::ShiftRight);
        assert_eq!(key_from_code(90), Key::F20);
        assert_eq!(key_from_code(10_000), Key::Unknown(10_000));
    }
}
