//! CGEvent -> [`KeyEvent`] conversion, including side-specific modifier
//! press/release labeling.
//!
//! The OS reports modifier changes as `FlagsChanged` events whose flags are
//! side-agnostic aggregates (e.g. `MaskShift` covers both Shift keys). To emit
//! uniform Press/Release events with side-specific keys we track per-side held
//! state, and reconcile it against the aggregate snapshot on every event:
//!
//! - aggregate bit clear -> both sides are up (clears drift from missed
//!   events);
//! - aggregate bit set but no side tracked -> fall back to the left side.
//!
//! Caps Lock and Fn need no tracking: their flags (`MaskAlphaShift`,
//! `MaskSecondaryFn`) are not shared between keys, so press/release is read
//! directly from the snapshot.

use crate::keycode::{self, Keycode};
use crate::{Key, KeyEvent, KeyEventKind, Modifiers};

/// Side-specific held state for the eight modifier keys.
#[derive(Default, Debug, Clone, Copy)]
struct SideState(u8);

impl SideState {
    const SHIFT_L: Self = Self(1 << 0);
    const SHIFT_R: Self = Self(1 << 1);
    const CONTROL_L: Self = Self(1 << 2);
    const CONTROL_R: Self = Self(1 << 3);
    const OPTION_L: Self = Self(1 << 4);
    const OPTION_R: Self = Self(1 << 5);
    const COMMAND_L: Self = Self(1 << 6);
    const COMMAND_R: Self = Self(1 << 7);

    const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
    fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
    fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

/// Maps a modifier `Key` to (its side bit, its group's aggregate snapshot bit).
fn side_of(key: Key) -> Option<(SideState, Modifiers)> {
    let group = key.modifier_group()?;
    let side = match key {
        Key::ShiftLeft => SideState::SHIFT_L,
        Key::ShiftRight => SideState::SHIFT_R,
        Key::ControlLeft => SideState::CONTROL_L,
        Key::ControlRight => SideState::CONTROL_R,
        Key::OptionLeft => SideState::OPTION_L,
        Key::OptionRight => SideState::OPTION_R,
        Key::CommandLeft => SideState::COMMAND_L,
        Key::CommandRight => SideState::COMMAND_R,
        _ => return None,
    };
    Some((side, group))
}

/// Tracks side-specific modifier state across events. Tap-thread local.
#[derive(Default)]
pub(crate) struct ModifierTracker {
    held: SideState,
}

impl ModifierTracker {
    /// Handle a `FlagsChanged` event. Consumes the keycode + snapshot and
    /// produces the uniform Press/Release event.
    pub(crate) fn flags_changed(&mut self, key: Key, snapshot: Modifiers) -> KeyEvent {
        let kind = match key {
            // Stateless keys: their flag bit is not shared with any other key.
            Key::CapsLock => press_if(snapshot.contains(Modifiers::CAPS_LOCK)),
            Key::Function => press_if(snapshot.contains(Modifiers::FN)),
            _ => match side_of(key) {
                Some((side, group)) => {
                    // FlagsChanged carries POST-change flags, so a clear aggregate
                    // bit is deterministic: no key of this group is held anymore,
                    // therefore this change was a release — even if our tracked
                    // state disagrees (missed press). Only a *set* aggregate is
                    // ambiguous (press, or release with the other side still held),
                    // and only there do we need toggle inference.
                    if !snapshot.contains(group) {
                        self.held.remove(side);
                        KeyEventKind::Release
                    } else {
                        let was_held = self.held.contains(side);
                        if was_held {
                            self.held.remove(side);
                        } else {
                            self.held.insert(side);
                        }
                        press_if(!was_held)
                    }
                }
                None => {
                    // Unknown keycode arriving as FlagsChanged: nothing sensible to
                    // infer; treat as press of the unknown key.
                    KeyEventKind::Press
                }
            },
        };
        // Reconcile all groups against the snapshot. Post-change flags make this
        // safe for the changed group too, and it clears phantom tracked state
        // (e.g. from a missed press) in the same event instead of the next one.
        self.reconcile(snapshot, None);
        KeyEvent {
            key,
            kind,
            modifiers: snapshot,
            is_repeat: false,
        }
    }

    /// Initialize tracked state from physical key state (at tap install time,
    /// so keys held while the stream starts are tracked truthfully instead of
    /// being guessed later by reconcile's assume-left fallback).
    pub(crate) fn init_from_physical(&mut self, is_held: impl Fn(Keycode) -> bool) {
        self.held = SideState::default();
        const SIDE_KEYS: [(Key, SideState); 8] = [
            (Key::ShiftLeft, SideState::SHIFT_L),
            (Key::ShiftRight, SideState::SHIFT_R),
            (Key::ControlLeft, SideState::CONTROL_L),
            (Key::ControlRight, SideState::CONTROL_R),
            (Key::OptionLeft, SideState::OPTION_L),
            (Key::OptionRight, SideState::OPTION_R),
            (Key::CommandLeft, SideState::COMMAND_L),
            (Key::CommandRight, SideState::COMMAND_R),
        ];
        for (key, side) in SIDE_KEYS {
            if let Some(code) = keycode::code_from_key(key)
                && is_held(code)
            {
                self.held.insert(side);
            }
        }
    }

    /// Reconcile tracked side state against an aggregate snapshot. Called on
    /// every non-FlagsChanged event (skip = None) and for the untouched groups
    /// on FlagsChanged events.
    pub(crate) fn reconcile(&mut self, snapshot: Modifiers, skip: Option<Modifiers>) {
        const GROUPS: [(Modifiers, SideState, SideState); 4] = [
            (Modifiers::SHIFT, SideState::SHIFT_L, SideState::SHIFT_R),
            (
                Modifiers::CONTROL,
                SideState::CONTROL_L,
                SideState::CONTROL_R,
            ),
            (Modifiers::OPTION, SideState::OPTION_L, SideState::OPTION_R),
            (
                Modifiers::COMMAND,
                SideState::COMMAND_L,
                SideState::COMMAND_R,
            ),
        ];
        for (group, left, right) in GROUPS {
            if skip == Some(group) {
                continue;
            }
            if !snapshot.contains(group) {
                // OS says nothing in this group is held: clear drift.
                self.held.remove(left);
                self.held.remove(right);
            } else if !self.held.contains(left) && !self.held.contains(right) {
                // OS says something is held but we never saw the press: assume left.
                self.held.insert(left);
            }
        }
    }
}

const fn press_if(held: bool) -> KeyEventKind {
    if held {
        KeyEventKind::Press
    } else {
        KeyEventKind::Release
    }
}

/// Convert one raw event into a uniform [`KeyEvent`].
///
/// Pure with respect to the OS: all OS facts (keycode, flags, autorepeat) are
/// passed in, so the logic is unit-testable without FFI.
pub(crate) fn convert(
    keycode: Keycode,
    is_repeat: bool,
    snapshot: Modifiers,
    tracker: &mut ModifierTracker,
    kind_of_event: RawKind,
) -> KeyEvent {
    let key = keycode::key_from_code(keycode);
    match kind_of_event {
        RawKind::Down => {
            tracker.reconcile(snapshot, None);
            KeyEvent {
                key,
                kind: KeyEventKind::Press,
                modifiers: snapshot,
                is_repeat,
            }
        }
        RawKind::Up => {
            tracker.reconcile(snapshot, None);
            KeyEvent {
                key,
                kind: KeyEventKind::Release,
                modifiers: snapshot,
                is_repeat: false,
            }
        }
        RawKind::FlagsChanged => tracker.flags_changed(key, snapshot),
    }
}

/// The three raw event categories the tap subscribes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawKind {
    Down,
    Up,
    FlagsChanged,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(key: Key, kind: KeyEventKind, mods: Modifiers) -> KeyEvent {
        KeyEvent {
            key,
            kind,
            modifiers: mods,
            is_repeat: false,
        }
    }

    #[test]
    fn plain_key_events() {
        let mut t = ModifierTracker::default();
        let down = convert(0, false, Modifiers::NONE, &mut t, RawKind::Down);
        assert_eq!(down, ev(Key::KeyA, KeyEventKind::Press, Modifiers::NONE));
        let repeat = convert(0, true, Modifiers::NONE, &mut t, RawKind::Down);
        assert!(repeat.is_repeat);
        let up = convert(0, false, Modifiers::NONE, &mut t, RawKind::Up);
        assert_eq!(up, ev(Key::KeyA, KeyEventKind::Release, Modifiers::NONE));
    }

    #[test]
    fn fn_press_release_from_flags() {
        let mut t = ModifierTracker::default();
        let down = convert(63, false, Modifiers::FN, &mut t, RawKind::FlagsChanged);
        assert_eq!(down, ev(Key::Function, KeyEventKind::Press, Modifiers::FN));
        let up = convert(63, false, Modifiers::NONE, &mut t, RawKind::FlagsChanged);
        assert_eq!(
            up,
            ev(Key::Function, KeyEventKind::Release, Modifiers::NONE)
        );
    }

    #[test]
    fn both_shifts_held_release_one() {
        let mut t = ModifierTracker::default();
        // Left shift down (aggregate SHIFT set).
        let e = convert(56, false, Modifiers::SHIFT, &mut t, RawKind::FlagsChanged);
        assert_eq!(e.kind, KeyEventKind::Press);
        assert_eq!(e.key, Key::ShiftLeft);
        // Right shift down while left held (aggregate still SHIFT).
        let e = convert(60, false, Modifiers::SHIFT, &mut t, RawKind::FlagsChanged);
        assert_eq!(e.kind, KeyEventKind::Press);
        assert_eq!(e.key, Key::ShiftRight);
        // Left released while right still held (aggregate STILL SHIFT).
        let e = convert(56, false, Modifiers::SHIFT, &mut t, RawKind::FlagsChanged);
        assert_eq!(e.kind, KeyEventKind::Release);
        assert_eq!(e.key, Key::ShiftLeft);
        // Right released (aggregate clear).
        let e = convert(60, false, Modifiers::NONE, &mut t, RawKind::FlagsChanged);
        assert_eq!(e.kind, KeyEventKind::Release);
        assert_eq!(e.key, Key::ShiftRight);
    }

    #[test]
    fn reconcile_clears_drift_from_missed_release() {
        let mut t = ModifierTracker::default();
        // ShiftLeft pressed.
        let _ = convert(56, false, Modifiers::SHIFT, &mut t, RawKind::FlagsChanged);
        // Its release is LOST (e.g. tap timeout). Next plain event has SHIFT clear.
        let _ = convert(0, false, Modifiers::NONE, &mut t, RawKind::Down);
        // Tracker must have cleared: a subsequent ShiftLeft FlagsChanged is a Press
        // again.
        let e = convert(56, false, Modifiers::SHIFT, &mut t, RawKind::FlagsChanged);
        assert_eq!(e.kind, KeyEventKind::Press);
    }

    #[test]
    fn reconcile_assumes_left_for_missed_press() {
        let mut t = ModifierTracker::default();
        // First event ever seen already has COMMAND held: press was missed.
        let e = convert(0, false, Modifiers::COMMAND, &mut t, RawKind::Down);
        assert_eq!(e.modifiers, Modifiers::COMMAND);
        // CommandRight release arrives with the aggregate CLEAR: the aggregate
        // rule determines this is a release (never a toggle-mislabeled press),
        // and the same event's reconcile clears the phantom CommandLeft that
        // assume-left had inserted. Tracker must not go negative or leak state.
        let e = convert(54, false, Modifiers::NONE, &mut t, RawKind::FlagsChanged);
        assert_eq!(e.key, Key::CommandRight);
        assert_eq!(e.kind, KeyEventKind::Release);
        // Phantom state is gone: a subsequent CommandLeft FlagsChanged with the
        // aggregate set is inferred as a fresh press.
        let e = convert(55, false, Modifiers::COMMAND, &mut t, RawKind::FlagsChanged);
        assert_eq!(e.kind, KeyEventKind::Press);
    }

    #[test]
    fn aggregate_clear_release_rule_heals_missed_press() {
        let mut t = ModifierTracker::default();
        // Tap installed while CommandLeft was held; its press is in the blind
        // zone. No assume-left has fired yet (no plain event seen).
        // The user releases it: aggregate clears -> deterministic Release.
        let e = convert(55, false, Modifiers::NONE, &mut t, RawKind::FlagsChanged);
        assert_eq!(
            e,
            ev(Key::CommandLeft, KeyEventKind::Release, Modifiers::NONE)
        );
    }

    #[test]
    fn caps_lock_from_flags() {
        let mut t = ModifierTracker::default();
        let on = convert(
            57,
            false,
            Modifiers::CAPS_LOCK,
            &mut t,
            RawKind::FlagsChanged,
        );
        assert_eq!(
            on,
            ev(Key::CapsLock, KeyEventKind::Press, Modifiers::CAPS_LOCK)
        );
        let off = convert(57, false, Modifiers::NONE, &mut t, RawKind::FlagsChanged);
        assert_eq!(
            off,
            ev(Key::CapsLock, KeyEventKind::Release, Modifiers::NONE)
        );
    }
}
