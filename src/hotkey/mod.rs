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
//! | intent                     | form                              |
//! |----------------------------|-----------------------------------|
//! | `Cmd+K`                    | `{CMD, Some(K)}`                   |
//! | bare `F5`                  | `{NONE, Some(F5)}`                 |
//! | single `Option` (either)   | `{OPT, None}`                      |
//! | only right `Option`        | `{NONE, Some(OptionRight)}`        |
//! | single `Fn`                | `{FN, None}`                       |
//! | `Fn+K`                     | `{FN, Some(K)}`                    |
//! | `Cmd+Shift` (no key)       | `{CMD\|SHIFT, None}`               |
//!
//! Registering an Fn-alone hotkey (`{FN, None}`) with [`Suppression::Swallow`]
//! also suppresses the system "Press 🌐 key to" action (emoji panel, input
//! source switch, …).

mod engine;
mod manager;
mod recorder;

pub use manager::HotkeyManager;
pub use recorder::{HotkeyRecorder, RecorderEvent};

use std::time::Duration;

use crate::{Key, Modifiers};

/// A global hotkey: `modifiers` must be exactly held, and
/// - `Some(trigger)`: the hotkey goes down when `trigger` is pressed and up
///   when it is released. The trigger may itself be a side-specific modifier
///   key (e.g. `Key::OptionRight`), which is how side-specific hotkeys are
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
                // `{OPT, OptionRight}` means the same as `{NONE, OptionRight}`.
                // (Expressing "hold left Option, press right Option" is not
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
    /// A modifier trigger (e.g. `OptionRight`) contributes its own group bit
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

/// The modifier group a key belongs to, for hotkey matching: the sided
/// groups plus sideless Fn. Caps Lock is deliberately excluded: its snapshot
/// bit means "locked", not "held", so it only makes sense as a trigger key.
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

/// Modifier bits relevant for matching. Caps Lock is excluded: it is a toggle
/// state, not a held modifier, and must not affect hotkey matching.
const MATCH_BITS: Modifiers = Modifiers::SHIFT
    .union(Modifiers::CONTROL)
    .union(Modifiers::OPTION)
    .union(Modifiers::COMMAND)
    .union(Modifiers::FN);

/// Exact match: the event's modifiers (restricted to match bits) equal the
/// hotkey's required set.
fn modifiers_exact(snapshot: Modifiers, required: Modifiers) -> bool {
    snapshot & MATCH_BITS == required
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fn_folds_into_modifier_set() {
        // Fn is sideless: both spellings of "Fn alone" are the same hotkey.
        assert_eq!(
            Hotkey::new(Modifiers::NONE, Some(Key::Function)),
            Hotkey::new(Modifiers::FN, None)
        );
        // Side-specific modifiers in the trigger slot keep their side.
        let right = Hotkey::new(Modifiers::NONE, Some(Key::OptionRight));
        assert_eq!(right.key(), Some(Key::OptionRight));
        assert_eq!(right.modifiers(), Modifiers::NONE);
        assert_ne!(right, Hotkey::new(Modifiers::OPTION, None));
        let left = Hotkey::new(Modifiers::NONE, Some(Key::OptionLeft));
        assert_ne!(left, right);
    }

    #[test]
    fn trigger_group_bit_is_stripped_from_modifiers() {
        // The trigger already satisfies its own group: `{OPT, OptionRight}`
        // normalizes to `{NONE, OptionRight}` instead of being unsatisfiable.
        assert_eq!(
            Hotkey::new(Modifiers::OPTION, Some(Key::OptionRight)),
            Hotkey::new(Modifiers::NONE, Some(Key::OptionRight))
        );
        // Unrelated modifier bits are kept.
        let h = Hotkey::new(Modifiers::SHIFT | Modifiers::OPTION, Some(Key::OptionRight));
        assert_eq!(h.modifiers(), Modifiers::SHIFT);
        assert_eq!(h.key(), Some(Key::OptionRight));
    }

    #[test]
    fn caps_lock_stays_a_trigger_key() {
        let h = Hotkey::new(Modifiers::NONE, Some(Key::CapsLock));
        assert_eq!(h.key(), Some(Key::CapsLock));
        assert_eq!(h.modifiers(), Modifiers::NONE);
    }

    #[test]
    fn exact_matching_ignores_caps_lock() {
        let h = Hotkey::new(Modifiers::COMMAND, Some(Key::KeyK));
        assert!(modifiers_exact(
            Modifiers::COMMAND | Modifiers::CAPS_LOCK,
            h.modifiers()
        ));
        assert!(!modifiers_exact(
            Modifiers::COMMAND | Modifiers::SHIFT,
            h.modifiers()
        ));
        assert!(modifiers_exact(Modifiers::COMMAND, h.modifiers()));
    }
}
