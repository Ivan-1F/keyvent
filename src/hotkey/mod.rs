//! High-level global hotkeys on top of the raw event stream.
//!
//! A [`Hotkey`] is a modifier set plus an optional trigger key. The trigger
//! is usually a non-modifier, but may be a side-specific modifier (that is
//! how side-specific hotkeys are expressed) or Caps Lock. The constructor
//! normalizes: the sideless Fn key folds into the modifier set, and a
//! trigger's own group bit in `modifiers` is stripped as redundant.
//!
//! Covered shapes:
//!
//! | intent                    | form                              |
//! |---------------------------|-----------------------------------|
//! | `Super+K`                 | `{SUPER, Some(K)}`                |
//! | bare `F5`                 | `{NONE, Some(F5)}`                |
//! | single `Alt` (either)     | `{ALT, None}`                     |
//! | only right `Alt`          | `{NONE, Some(AltRight)}`          |
//! | single `Fn`               | `{FN, None}`                      |
//! | `Fn+K`                    | `{FN, Some(K)}`                   |
//! | `Super+Shift` (no key)    | `{SUPER\|SHIFT, None}`            |
//!
//! Registering an Fn-alone hotkey (`{FN, None}`) with [`Suppression::Swallow`]
//! also suppresses the system "Press 🌐 key to" action (emoji panel, input
//! source switch, …).

mod engine;
mod manager;
mod recorder;

pub use manager::HotkeyManager;
pub use recorder::{HotkeyRecorder, RecorderEvent};

use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::event::parse_modifier_token;
use crate::{Key, Modifiers};

/// A global hotkey: `modifiers` must be exactly held, and
/// - `Some(trigger)`: the hotkey goes down when `trigger` is pressed and up
///   when it is released. The trigger may itself be a side-specific modifier
///   key (e.g. `Key::AltRight`), which is how side-specific hotkeys are
///   expressed;
/// - `None` (modifier-only): the hotkey goes down the moment the modifier set
///   becomes complete, and up the moment any member leaves the set.
///
/// Use [`Hotkey::new`] to construct. Only the sideless Fn key is folded into
/// the modifier set (`{NONE, Function}` becomes `{FN, None}`); left/right
/// modifiers in the trigger slot keep their side, since that is the point.
/// A trigger's own group bit in `modifiers` is redundant and stripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hotkey {
    modifiers: Modifiers,
    key: Option<Key>,
}

impl Hotkey {
    pub fn new(modifiers: Modifiers, key: Option<Key>) -> Self {
        match key {
            // Fn has no sides, so folding loses nothing and keeps one
            // canonical form for "Fn alone".
            Some(Key::Function) => Self {
                modifiers: modifiers | Modifiers::FN,
                key: None,
            },
            Some(k) => {
                // A modifier trigger already satisfies its own group, so a
                // matching bit in `modifiers` is redundant and stripped:
                // `{ALT, AltRight}` means the same as `{NONE, AltRight}`.
                // (Expressing "hold left Alt, press right Alt" is not
                // currently possible.)
                let modifiers = match modifier_bit_of(k) {
                    Some(bit) => modifiers - bit,
                    None => modifiers,
                };
                Self { modifiers, key }
            }
            None => Self { modifiers, key },
        }
    }

    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    pub fn key(&self) -> Option<Key> {
        self.key
    }

    /// True if this hotkey involves `key` — as trigger or as a modifier member.
    fn involves(&self, key: Key) -> bool {
        if self.key == Some(key) {
            return true;
        }
        matches!(modifier_bit_of(key), Some(bit) if self.modifiers.contains(bit))
    }

    /// True if a press carrying `snapshot` satisfies the modifier requirement.
    /// A modifier trigger (e.g. `AltRight`) contributes its own group bit
    /// to the snapshot by definition, so the REST is matched exactly — the
    /// constructor has already stripped that bit from `modifiers`.
    fn modifiers_match(&self, snapshot: Modifiers) -> bool {
        let own = self
            .key
            .and_then(modifier_bit_of)
            .unwrap_or(Modifiers::NONE);
        modifiers_exact(snapshot - own, self.modifiers)
    }
}

impl fmt::Display for Hotkey {
    /// `+`-joined modifiers and trigger key (e.g. `Shift+Super+K`). Modifier
    /// bits outside the match set are written as a `0x..` hex token so the
    /// form parses back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        let named = self.modifiers & MATCH_BITS;
        if !named.is_empty() {
            parts.push(named.to_string());
        }
        let raw = self.modifiers - MATCH_BITS;
        if !raw.is_empty() {
            parts.push(format!("0x{:X}", raw.bits()));
        }
        if let Some(key) = self.key {
            parts.push(key.to_string());
        }
        write!(f, "{}", parts.join("+"))
    }
}

impl FromStr for Hotkey {
    type Err = ParseHotkeyError;

    /// Parses the `Display` form case-insensitively: modifier tokens, then an
    /// optional trigger key as the last token. Caps Lock is always the
    /// trigger key here, never a modifier token. The result is normalized
    /// like [`Hotkey::new`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut modifiers = Modifiers::NONE;
        let mut tokens = s.split('+').peekable();
        while let Some(token) = tokens.next() {
            let modifier = if token.eq_ignore_ascii_case("capslock") {
                None
            } else {
                parse_modifier_token(token)
            };
            match modifier {
                Some(bit) => modifiers |= bit,
                None => {
                    if tokens.peek().is_some() {
                        return Err(ParseHotkeyError("trigger key must be the last token"));
                    }
                    let key = token
                        .parse()
                        .map_err(|_| ParseHotkeyError("unrecognized key"))?;
                    return Ok(Hotkey::new(modifiers, Some(key)));
                }
            }
        }
        if modifiers.is_empty() {
            return Err(ParseHotkeyError("empty hotkey"));
        }
        Ok(Hotkey::new(modifiers, None))
    }
}

/// Failed to parse a [`Hotkey`] from a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseHotkeyError(&'static str);

impl fmt::Display for ParseHotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Error for ParseHotkeyError {}

/// The modifier group a key belongs to, for hotkey matching: the sided
/// groups plus sideless Fn.
fn modifier_bit_of(key: Key) -> Option<Modifiers> {
    match key {
        Key::Function => Some(Modifiers::FN),
        _ => key.modifier_group(),
    }
}

/// Whether a swallowed hotkey's events are hidden from other applications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Suppression {
    /// Events forming the hotkey never reach other apps.
    Swallow,
    /// Events pass through; the hotkey is observed only.
    Pass,
}

/// Opaque identifier of a registered hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HotkeyId(u32);

/// A hotkey activation or deactivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// The hotkey became active.
    Down(HotkeyId),
    /// The hotkey became inactive; `held_for` is how long it was active.
    Up { id: HotkeyId, held_for: Duration },
}

impl HotkeyEvent {
    pub fn id(&self) -> HotkeyId {
        match self {
            HotkeyEvent::Down(id) | HotkeyEvent::Up { id, .. } => *id,
        }
    }
}

/// Modifier bits relevant for matching.
const MATCH_BITS: Modifiers = Modifiers::SHIFT
    .union(Modifiers::CONTROL)
    .union(Modifiers::ALT)
    .union(Modifiers::SUPER)
    .union(Modifiers::FN);

/// Exact match: the event's modifiers (restricted to match bits) equal the
/// hotkey's required set.
fn modifiers_exact(snapshot: Modifiers, required: Modifiers) -> bool {
    snapshot & MATCH_BITS == required
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{DIGIT_KEYS, KEY_NAMES, LETTER_KEYS};

    #[test]
    fn only_fn_folds_into_modifier_set() {
        // Fn is sideless: both spellings of "Fn alone" are the same hotkey.
        assert_eq!(
            Hotkey::new(Modifiers::NONE, Some(Key::Function)),
            Hotkey::new(Modifiers::FN, None)
        );
        // Side-specific modifiers in the trigger slot keep their side.
        let right = Hotkey::new(Modifiers::NONE, Some(Key::AltRight));
        assert_eq!(right.key(), Some(Key::AltRight));
        assert_eq!(right.modifiers(), Modifiers::NONE);
        assert_ne!(right, Hotkey::new(Modifiers::ALT, None));
        let left = Hotkey::new(Modifiers::NONE, Some(Key::AltLeft));
        assert_ne!(left, right);
    }

    #[test]
    fn trigger_group_bit_is_stripped_from_modifiers() {
        // The trigger already satisfies its own group: `{ALT, AltRight}`
        // normalizes to `{NONE, AltRight}` instead of being unsatisfiable.
        assert_eq!(
            Hotkey::new(Modifiers::ALT, Some(Key::AltRight)),
            Hotkey::new(Modifiers::NONE, Some(Key::AltRight))
        );
        // Unrelated modifier bits are kept.
        let h = Hotkey::new(Modifiers::SHIFT | Modifiers::ALT, Some(Key::AltRight));
        assert_eq!(h.modifiers(), Modifiers::SHIFT);
        assert_eq!(h.key(), Some(Key::AltRight));
    }

    #[test]
    fn caps_lock_stays_a_trigger_key() {
        let h = Hotkey::new(Modifiers::NONE, Some(Key::CapsLock));
        assert_eq!(h.key(), Some(Key::CapsLock));
        assert_eq!(h.modifiers(), Modifiers::NONE);
    }

    #[test]
    fn exact_matching_ignores_unmatched_bits() {
        let h = Hotkey::new(Modifiers::SUPER, Some(Key::KeyK));
        assert!(modifiers_exact(
            Modifiers::SUPER | Modifiers::from_bits(0x20),
            h.modifiers()
        ));
        assert!(!modifiers_exact(
            Modifiers::SUPER | Modifiers::SHIFT,
            h.modifiers()
        ));
        assert!(modifiers_exact(Modifiers::SUPER, h.modifiers()));
    }

    #[test]
    fn hotkey_string_round_trip() {
        let hotkeys = [
            Hotkey::new(Modifiers::SUPER | Modifiers::SHIFT, Some(Key::KeyK)),
            Hotkey::new(Modifiers::NONE, Some(Key::F5)),
            Hotkey::new(Modifiers::ALT, None),
            Hotkey::new(Modifiers::FN, None),
            Hotkey::new(Modifiers::NONE, Some(Key::AltRight)),
            Hotkey::new(Modifiers::NONE, Some(Key::CapsLock)),
            Hotkey::new(Modifiers::SUPER, Some(Key::CapsLock)),
            Hotkey::new(Modifiers::from_bits(0x20), None),
            Hotkey::new(
                Modifiers::from_bits(0x20) | Modifiers::SUPER,
                Some(Key::KeyK),
            ),
            Hotkey::new(Modifiers::NONE, Some(Key::Unknown(0x5E))),
        ];
        for hotkey in hotkeys {
            assert_eq!(hotkey.to_string().parse::<Hotkey>(), Ok(hotkey));
        }
    }

    #[test]
    fn every_key_round_trips_as_trigger() {
        // Every named key, bare and behind a modifier set — including the
        // side-specific modifiers, whose names share prefixes with modifier
        // tokens (`ShiftLeft` vs `Shift`).
        let keys = LETTER_KEYS
            .iter()
            .chain(DIGIT_KEYS.iter())
            .copied()
            .chain(KEY_NAMES.iter().map(|(key, _)| *key))
            .chain([Key::Unknown(0x5E)]);
        for key in keys {
            for modifiers in [Modifiers::NONE, Modifiers::SUPER | Modifiers::SHIFT] {
                let hotkey = Hotkey::new(modifiers, Some(key));
                assert_eq!(hotkey.to_string().parse::<Hotkey>(), Ok(hotkey));
            }
        }
    }

    #[test]
    fn hotkey_display() {
        assert_eq!(
            Hotkey::new(Modifiers::SUPER | Modifiers::SHIFT, Some(Key::KeyK)).to_string(),
            "Shift+Super+K"
        );
        assert_eq!(
            Hotkey::new(Modifiers::NONE, Some(Key::F5)).to_string(),
            "F5"
        );
        assert_eq!(Hotkey::new(Modifiers::ALT, None).to_string(), "Alt");
        assert_eq!(
            Hotkey::new(Modifiers::NONE, Some(Key::Function)).to_string(),
            "Fn"
        );
        assert_eq!(
            Hotkey::new(Modifiers::from_bits(0x20), None).to_string(),
            "0x20"
        );
        assert_eq!(Hotkey::new(Modifiers::NONE, None).to_string(), "");
    }

    #[test]
    fn hotkey_parse_normalizes() {
        assert_eq!(
            "shift+super+k".parse(),
            Ok(Hotkey::new(
                Modifiers::SUPER | Modifiers::SHIFT,
                Some(Key::KeyK)
            ))
        );
        // The trigger's own group bit is stripped, like in `Hotkey::new`.
        assert_eq!(
            "alt+altright".parse(),
            Ok(Hotkey::new(Modifiers::NONE, Some(Key::AltRight)))
        );
        // Fn folds into the modifier set.
        assert_eq!("fn+fn".parse(), Ok(Hotkey::new(Modifiers::FN, None)));
        // Caps Lock is a trigger key, never a modifier.
        assert_eq!(
            "CapsLock".parse(),
            Ok(Hotkey::new(Modifiers::NONE, Some(Key::CapsLock)))
        );
        assert_eq!(
            "super+capslock".parse(),
            Ok(Hotkey::new(Modifiers::SUPER, Some(Key::CapsLock)))
        );
    }

    #[test]
    fn hotkey_parse_errors() {
        for s in [
            "",
            "Super+",
            "+K",
            "Super+K+J",
            "K+Super",
            "CapsLock+K",
            "Nope",
            "0x0",
        ] {
            assert!(s.parse::<Hotkey>().is_err(), "{s}");
        }
    }
}
