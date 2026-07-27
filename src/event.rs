//! Public event types delivered to the user's callback.
//!
//! Design contract:
//! - Every key on the keyboard — including modifiers and Fn/Globe — produces
//!   uniform [`KeyEventKind::Press`] / [`KeyEventKind::Release`] events.
//! - Every event carries a complete [`Modifiers`] snapshot taken from the OS at
//!   event time. Consumers never need to mirror key state locally.
//! - The snapshot is side-agnostic (the OS does not report sides in event
//!   flags); side information is carried by [`KeyEvent::key`] itself, e.g. a
//!   `Press` of [`Key::ShiftLeft`].
//! - Unmapped keycodes are preserved as [`Key::Unknown`] instead of dropped.

use std::error::Error;
use std::fmt;
use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not, Sub, SubAssign};
use std::str::FromStr;

/// A physical key on the keyboard.
///
/// Modifier keys are side-specific (`ShiftLeft` vs `ShiftRight`). `Function`
/// is the Fn/Globe key (keycode 0x3F); on macOS it only generates flags-changed
/// events, which the tap layer normalizes into regular press/release events.
/// `Function` is not reported on Windows, where Fn is handled inside keyboard
/// firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Key {
    ShiftLeft,
    ShiftRight,
    ControlLeft,
    ControlRight,
    /// The Alt key (⌥ Option on macOS).
    AltLeft,
    AltRight,
    /// The Super key (⌘ Command on macOS, ⊞ Win on Windows).
    SuperLeft,
    SuperRight,
    CapsLock,
    /// The Fn/Globe key (🌐).
    Function,

    Escape,
    Tab,
    Space,
    Return,
    /// The Delete key (backspace).
    Delete,

    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,

    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,

    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,

    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,

    BackQuote,
    Minus,
    Equal,
    LeftBracket,
    RightBracket,
    Semicolon,
    Quote,
    Backslash,
    Comma,
    Period,
    Slash,

    /// A keycode with no known mapping. The raw macOS keycode is preserved.
    Unknown(u16),
}

impl Key {
    /// The side-agnostic modifier group a side-specific modifier key belongs
    /// to, if any. Caps Lock and Fn (sideless) have no group.
    pub(crate) fn modifier_group(self) -> Option<Modifiers> {
        Some(match self {
            Key::ShiftLeft | Key::ShiftRight => Modifiers::SHIFT,
            Key::ControlLeft | Key::ControlRight => Modifiers::CONTROL,
            Key::AltLeft | Key::AltRight => Modifiers::ALT,
            Key::SuperLeft | Key::SuperRight => Modifiers::SUPER,
            _ => return None,
        })
    }
}

/// Letter keys in alphabetical order, for the single-character short form.
pub(crate) const LETTER_KEYS: [Key; 26] = [
    Key::KeyA,
    Key::KeyB,
    Key::KeyC,
    Key::KeyD,
    Key::KeyE,
    Key::KeyF,
    Key::KeyG,
    Key::KeyH,
    Key::KeyI,
    Key::KeyJ,
    Key::KeyK,
    Key::KeyL,
    Key::KeyM,
    Key::KeyN,
    Key::KeyO,
    Key::KeyP,
    Key::KeyQ,
    Key::KeyR,
    Key::KeyS,
    Key::KeyT,
    Key::KeyU,
    Key::KeyV,
    Key::KeyW,
    Key::KeyX,
    Key::KeyY,
    Key::KeyZ,
];

/// Digit keys in numerical order, for the single-character short form.
pub(crate) const DIGIT_KEYS: [Key; 10] = [
    Key::Num0,
    Key::Num1,
    Key::Num2,
    Key::Num3,
    Key::Num4,
    Key::Num5,
    Key::Num6,
    Key::Num7,
    Key::Num8,
    Key::Num9,
];

/// Keys without a short form, paired with their canonical names.
pub(crate) const KEY_NAMES: &[(Key, &str)] = &[
    (Key::ShiftLeft, "ShiftLeft"),
    (Key::ShiftRight, "ShiftRight"),
    (Key::ControlLeft, "ControlLeft"),
    (Key::ControlRight, "ControlRight"),
    (Key::AltLeft, "AltLeft"),
    (Key::AltRight, "AltRight"),
    (Key::SuperLeft, "SuperLeft"),
    (Key::SuperRight, "SuperRight"),
    (Key::CapsLock, "CapsLock"),
    (Key::Function, "Fn"),
    (Key::Escape, "Escape"),
    (Key::Tab, "Tab"),
    (Key::Space, "Space"),
    (Key::Return, "Return"),
    (Key::Delete, "Delete"),
    (Key::ArrowUp, "ArrowUp"),
    (Key::ArrowDown, "ArrowDown"),
    (Key::ArrowLeft, "ArrowLeft"),
    (Key::ArrowRight, "ArrowRight"),
    (Key::F1, "F1"),
    (Key::F2, "F2"),
    (Key::F3, "F3"),
    (Key::F4, "F4"),
    (Key::F5, "F5"),
    (Key::F6, "F6"),
    (Key::F7, "F7"),
    (Key::F8, "F8"),
    (Key::F9, "F9"),
    (Key::F10, "F10"),
    (Key::F11, "F11"),
    (Key::F12, "F12"),
    (Key::F13, "F13"),
    (Key::F14, "F14"),
    (Key::F15, "F15"),
    (Key::F16, "F16"),
    (Key::F17, "F17"),
    (Key::F18, "F18"),
    (Key::F19, "F19"),
    (Key::F20, "F20"),
    (Key::BackQuote, "BackQuote"),
    (Key::Minus, "Minus"),
    (Key::Equal, "Equal"),
    (Key::LeftBracket, "LeftBracket"),
    (Key::RightBracket, "RightBracket"),
    (Key::Semicolon, "Semicolon"),
    (Key::Quote, "Quote"),
    (Key::Backslash, "Backslash"),
    (Key::Comma, "Comma"),
    (Key::Period, "Period"),
    (Key::Slash, "Slash"),
];

impl fmt::Display for Key {
    /// The single character for letter/digit keys, the canonical name
    /// otherwise; `Unknown` keeps the keycode as `Unknown(0x..)` hex.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(i) = LETTER_KEYS.iter().position(|k| k == self) {
            return write!(f, "{}", char::from(b'A' + i as u8));
        }
        if let Some(i) = DIGIT_KEYS.iter().position(|k| k == self) {
            return write!(f, "{}", char::from(b'0' + i as u8));
        }
        if let Key::Unknown(code) = self {
            return write!(f, "Unknown(0x{code:X})");
        }
        match KEY_NAMES.iter().find(|(k, _)| k == self) {
            Some((_, name)) => f.write_str(name),
            None => unreachable!("KEY_NAMES covers every named key"),
        }
    }
}

impl FromStr for Key {
    type Err = ParseKeyError;

    /// Parses the `Display` form case-insensitively; also accepts the `KeyX` /
    /// `NumX` variant names and decimal `Unknown(..)`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let short = single_byte(s)
            .and_then(letter_key)
            .or_else(|| single_byte(s).and_then(digit_key))
            .or_else(|| {
                strip_prefix_ci(s, "Key")
                    .and_then(single_byte)
                    .and_then(letter_key)
            })
            .or_else(|| {
                strip_prefix_ci(s, "Num")
                    .and_then(single_byte)
                    .and_then(digit_key)
            });
        if let Some(key) = short {
            return Ok(key);
        }
        if let Some(inner) = strip_prefix_ci(s, "Unknown")
            .and_then(|rest| rest.strip_prefix('('))
            .and_then(|rest| rest.strip_suffix(')'))
        {
            let code = match strip_prefix_ci(inner, "0x") {
                Some(hex) => u16::from_str_radix(hex, 16),
                None => inner.parse(),
            };
            return code.map(Key::Unknown).map_err(|_| ParseKeyError(()));
        }
        KEY_NAMES
            .iter()
            .find(|(_, name)| name.eq_ignore_ascii_case(s))
            .map(|(key, _)| *key)
            .ok_or(ParseKeyError(()))
    }
}

/// Failed to parse a [`Key`] from a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseKeyError(());

impl fmt::Display for ParseKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unrecognized key")
    }
}

impl Error for ParseKeyError {}

/// The only byte of a one-byte string.
fn single_byte(s: &str) -> Option<u8> {
    (s.len() == 1).then(|| s.as_bytes()[0])
}

/// The letter key for an ASCII letter byte, case-insensitively.
fn letter_key(c: u8) -> Option<Key> {
    c.is_ascii_alphabetic()
        .then(|| LETTER_KEYS[(c.to_ascii_uppercase() - b'A') as usize])
}

/// The digit key for an ASCII digit byte.
fn digit_key(c: u8) -> Option<Key> {
    c.is_ascii_digit().then(|| DIGIT_KEYS[(c - b'0') as usize])
}

/// `strip_prefix`, but ASCII case-insensitive.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| s.get(prefix.len()..).unwrap())
}

/// Whether a key was pressed or released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyEventKind {
    Press,
    Release,
}

/// A single keyboard event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    /// The key that changed state. Side-specific for modifiers.
    pub key: Key,
    /// Press or release.
    pub kind: KeyEventKind,
    /// Full modifier snapshot at event time (side-agnostic OS ground truth).
    pub modifiers: Modifiers,
    /// True if this press was generated by key repeat (holding the key down).
    /// Always false for releases and for modifier keys.
    pub is_repeat: bool,
}

/// A snapshot of modifier state, as reported by the OS event flags.
///
/// Bits are side-agnostic: `SHIFT` means either Shift key is held. This is a
/// plain bitset; combine with `|`, test with [`contains`](Modifiers::contains).
#[derive(Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Modifiers(u32);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1 << 0);
    pub const CONTROL: Self = Self(1 << 1);
    pub const ALT: Self = Self(1 << 2);
    pub const SUPER: Self = Self(1 << 3);
    /// The Fn/Globe key (macOS only).
    pub const FN: Self = Self(1 << 4);

    pub const fn empty() -> Self {
        Self::NONE
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// True if every bit in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// True if any bit in `other` is set in `self`.
    pub const fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub(crate) const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Bitflags-style union, usable in const contexts.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl BitOr for Modifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for Modifiers {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for Modifiers {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl Sub for Modifiers {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 & !rhs.0)
    }
}

impl SubAssign for Modifiers {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 &= !rhs.0;
    }
}

impl Not for Modifiers {
    type Output = Self;
    fn not(self) -> Self {
        Self(!self.0)
    }
}

impl fmt::Debug for Modifiers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMES: [(Modifiers, &str); 5] = [
            (Modifiers::SHIFT, "SHIFT"),
            (Modifiers::CONTROL, "CONTROL"),
            (Modifiers::ALT, "ALT"),
            (Modifiers::SUPER, "SUPER"),
            (Modifiers::FN, "FN"),
        ];
        if self.is_empty() {
            return write!(f, "Modifiers(NONE)");
        }
        let mut parts = Vec::new();
        let mut rest = *self;
        for (bit, name) in NAMES {
            if self.contains(bit) {
                parts.push(name);
                rest -= bit;
            }
        }
        if !rest.is_empty() {
            parts.push("?");
        }
        write!(f, "Modifiers({})", parts.join("|"))
    }
}

/// Modifier bit names in display order, shared with the hotkey string form.
pub(crate) const MODIFIER_TOKENS: &[(Modifiers, &str)] = &[
    (Modifiers::FN, "Fn"),
    (Modifiers::CONTROL, "Ctrl"),
    (Modifiers::ALT, "Alt"),
    (Modifiers::SHIFT, "Shift"),
    (Modifiers::SUPER, "Super"),
];

/// Parses one modifier token: a name from [`MODIFIER_TOKENS`] or raw bits as
/// `0x..` hex, case-insensitively.
pub(crate) fn parse_modifier_token(token: &str) -> Option<Modifiers> {
    if let Some(hex) = strip_prefix_ci(token, "0x") {
        return u32::from_str_radix(hex, 16).ok().map(Modifiers::from_bits);
    }
    MODIFIER_TOKENS
        .iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(token))
        .map(|(bit, _)| *bit)
}

impl fmt::Display for Modifiers {
    /// Named bits joined by `+` in [`MODIFIER_TOKENS`] order, any unnamed bits
    /// as a trailing `0x..` hex token; the empty set is the empty string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = MODIFIER_TOKENS
            .iter()
            .filter(|(bit, _)| self.contains(*bit))
            .map(|(_, name)| name.to_string())
            .collect();
        let named = MODIFIER_TOKENS
            .iter()
            .fold(Modifiers::NONE, |acc, (bit, _)| acc | *bit);
        let rest = *self - named;
        if !rest.is_empty() {
            parts.push(format!("0x{:X}", rest.bits()));
        }
        write!(f, "{}", parts.join("+"))
    }
}

impl FromStr for Modifiers {
    type Err = ParseModifiersError;

    /// Parses the `Display` form: `+`-joined tokens, each a bit name or `0x..`
    /// hex bits, case-insensitively. The empty string is the empty set.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Ok(Modifiers::NONE);
        }
        let mut modifiers = Modifiers::NONE;
        for token in s.split('+') {
            match parse_modifier_token(token) {
                Some(bit) => modifiers |= bit,
                None => return Err(ParseModifiersError(())),
            }
        }
        Ok(modifiers)
    }
}

/// Failed to parse a [`Modifiers`] from a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseModifiersError(());

impl fmt::Display for ParseModifiersError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unrecognized modifier")
    }
}

impl Error for ParseModifiersError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_bitops() {
        let m = Modifiers::SHIFT | Modifiers::SUPER;
        assert!(m.contains(Modifiers::SHIFT));
        assert!(m.contains(Modifiers::SUPER));
        assert!(!m.contains(Modifiers::ALT));
        assert!(m.intersects(Modifiers::SUPER | Modifiers::ALT));
        assert!(!m.intersects(Modifiers::ALT));
        assert_eq!(m - Modifiers::SHIFT, Modifiers::SUPER);
        assert!((m - Modifiers::SHIFT - Modifiers::SUPER).is_empty());
    }

    #[test]
    fn modifiers_debug() {
        assert_eq!(format!("{:?}", Modifiers::NONE), "Modifiers(NONE)");
        assert_eq!(
            format!("{:?}", Modifiers::SUPER | Modifiers::FN),
            "Modifiers(SUPER|FN)"
        );
    }

    #[test]
    fn key_string_round_trip() {
        let keys = LETTER_KEYS
            .iter()
            .chain(DIGIT_KEYS.iter())
            .copied()
            .chain(KEY_NAMES.iter().map(|(key, _)| *key))
            .chain([Key::Unknown(0), Key::Unknown(0x3F), Key::Unknown(0xFFFF)]);
        for key in keys {
            assert_eq!(key.to_string().parse::<Key>(), Ok(key));
        }
    }

    #[test]
    fn key_display() {
        assert_eq!(Key::KeyK.to_string(), "K");
        assert_eq!(Key::Num1.to_string(), "1");
        assert_eq!(Key::F5.to_string(), "F5");
        assert_eq!(Key::Function.to_string(), "Fn");
        assert_eq!(Key::AltRight.to_string(), "AltRight");
        assert_eq!(Key::Unknown(0x3F).to_string(), "Unknown(0x3F)");
    }

    #[test]
    fn key_parse_accepts_variant_names_and_decimal_unknown() {
        assert_eq!("keyk".parse(), Ok(Key::KeyK));
        assert_eq!("Num1".parse(), Ok(Key::Num1));
        assert_eq!("fn".parse(), Ok(Key::Function));
        assert_eq!("space".parse(), Ok(Key::Space));
        assert_eq!("Unknown(94)".parse(), Ok(Key::Unknown(94)));
        assert_eq!("unknown(0x5e)".parse(), Ok(Key::Unknown(0x5E)));
    }

    #[test]
    fn key_parse_errors() {
        for s in [
            "",
            "Key",
            "Num",
            "Unknown()",
            "Unknown(0x10000)",
            "Nope",
            "++",
        ] {
            assert!(s.parse::<Key>().is_err(), "{s}");
        }
    }

    #[test]
    fn modifiers_string_round_trip() {
        // Every combination of the named bits, plus unnamed-bit shapes.
        let combos = (0..1 << MODIFIER_TOKENS.len()).map(|mask| {
            MODIFIER_TOKENS.iter().enumerate().fold(
                Modifiers::NONE,
                |acc, (i, (bit, _))| match mask & (1 << i) {
                    0 => acc,
                    _ => acc | *bit,
                },
            )
        });
        let sets = combos.chain([
            Modifiers::from_bits(0x40),
            Modifiers::SUPER | Modifiers::from_bits(0x40),
            Modifiers::from_bits(u32::MAX),
        ]);
        for m in sets {
            assert_eq!(m.to_string().parse::<Modifiers>(), Ok(m));
        }
    }

    #[test]
    fn modifiers_display() {
        assert_eq!(Modifiers::NONE.to_string(), "");
        assert_eq!(
            (Modifiers::SUPER | Modifiers::SHIFT).to_string(),
            "Shift+Super"
        );
        assert_eq!(
            (Modifiers::SUPER | Modifiers::from_bits(0x40)).to_string(),
            "Super+0x40"
        );
    }

    #[test]
    fn modifiers_parse() {
        assert_eq!("".parse(), Ok(Modifiers::NONE));
        assert_eq!(
            "super+shift".parse(),
            Ok(Modifiers::SUPER | Modifiers::SHIFT)
        );
        assert_eq!("0x3".parse(), Ok(Modifiers::SHIFT | Modifiers::CONTROL));
    }

    #[test]
    fn modifiers_parse_errors() {
        for s in ["Super+", "+", "0x", "0x100000000", "Nope"] {
            assert!(s.parse::<Modifiers>().is_err(), "{s}");
        }
    }
}
