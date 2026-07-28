//! Raw hook event -> [`KeyEvent`] conversion.
//!
//! The hook reports side-specific press/release per key directly, so no side
//! inference is needed (unlike macOS `FlagsChanged`). But it carries no
//! modifier snapshot and no autorepeat flag, so both are self-tracked here:
//! the snapshot folds the currently-held modifier keys, and a press arriving
//! for an already-held key is a repeat. A duplicate modifier press is dropped
//! instead of reported: macOS never emits one, and reporting it would let
//! suppression re-decide an already-booked press cycle. The hook can miss
//! releases (secure desktop, skipped events), so tracked state is reconciled
//! against physical key state on every event — the role the OS-provided
//! snapshot plays in the macOS tracker.

use std::collections::HashSet;

use super::keycode::{self, Keycode};
use crate::{KeyEvent, KeyEventKind, Modifiers};

/// Tracks held keys across events to synthesize the snapshot and the repeat
/// flag the hook does not provide. Hook-thread local.
#[derive(Default)]
pub(crate) struct Tracker {
    pressed: HashSet<Keycode>,
}

impl Tracker {
    /// Convert one raw event into a uniform [`KeyEvent`]; `None` for a
    /// duplicate modifier press, which the hook layer swallows silently.
    /// The snapshot is post-change: a modifier's own press already includes
    /// its group bit.
    pub(crate) fn convert(&mut self, code: Keycode, down: bool) -> Option<KeyEvent> {
        let key = keycode::key_from_code(code);
        let mut is_repeat = false;
        let kind = if down {
            if self.pressed.insert(code) {
                KeyEventKind::Press
            } else if key.modifier_group().is_some() {
                return None;
            } else {
                is_repeat = true;
                KeyEventKind::Press
            }
        } else {
            self.pressed.remove(&code);
            KeyEventKind::Release
        };
        Some(KeyEvent {
            key,
            kind,
            modifiers: self.snapshot(),
            is_repeat,
        })
    }

    /// Seeds tracked state at install time from physical key state, so keys
    /// held while the stream starts are tracked truthfully: their in-flight
    /// autorepeats read as repeats, not fresh presses.
    pub(crate) fn init_from_physical(&mut self, is_held: impl Fn(Keycode) -> bool) {
        self.pressed.clear();
        for code in keycode::all_codes() {
            if is_held(code) {
                self.pressed.insert(code);
            }
        }
    }

    /// Drop tracked keys that are physically released — their releases were
    /// missed — and return them for synthetic-release closing. `skip` keeps
    /// the current release's own key: tracked-but-released is the state that
    /// release is about to resolve, not drift.
    pub(crate) fn reconcile(
        &mut self,
        is_held: impl Fn(Keycode) -> bool,
        skip: Option<Keycode>,
    ) -> Vec<Keycode> {
        let stale: Vec<Keycode> = self
            .pressed
            .iter()
            .filter(|code| Some(**code) != skip && !is_held(**code))
            .copied()
            .collect();
        for code in &stale {
            self.pressed.remove(code);
        }
        stale
    }

    /// The current modifier snapshot: the groups of the held modifier keys.
    pub(crate) fn snapshot(&self) -> Modifiers {
        let mut snapshot = Modifiers::NONE;
        for &code in &self.pressed {
            if let Some(group) = keycode::key_from_code(code).modifier_group() {
                snapshot |= group;
            }
        }
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Key;

    fn ev(key: Key, kind: KeyEventKind, mods: Modifiers, is_repeat: bool) -> KeyEvent {
        KeyEvent {
            key,
            kind,
            modifiers: mods,
            is_repeat,
        }
    }

    #[test]
    fn plain_press_release_and_repeat() {
        let mut t = Tracker::default();
        let press = t.convert(0x1E, true).unwrap();
        assert_eq!(
            press,
            ev(Key::KeyA, KeyEventKind::Press, Modifiers::NONE, false)
        );
        // A second press while held is a repeat; state is unchanged.
        let repeat = t.convert(0x1E, true).unwrap();
        assert_eq!(
            repeat,
            ev(Key::KeyA, KeyEventKind::Press, Modifiers::NONE, true)
        );
        let release = t.convert(0x1E, false).unwrap();
        assert_eq!(
            release,
            ev(Key::KeyA, KeyEventKind::Release, Modifiers::NONE, false)
        );
        // After the release, a press is fresh again.
        let press = t.convert(0x1E, true).unwrap();
        assert!(!press.is_repeat);
    }

    #[test]
    fn modifier_snapshot_tracks_held_keys() {
        let mut t = Tracker::default();
        // A modifier's own press already carries its group bit.
        let e = t.convert(0x2A, true).unwrap();
        assert_eq!(
            e,
            ev(Key::ShiftLeft, KeyEventKind::Press, Modifiers::SHIFT, false)
        );
        let e = t.convert(0x1E, true).unwrap();
        assert_eq!(
            e,
            ev(Key::KeyA, KeyEventKind::Press, Modifiers::SHIFT, false)
        );
        // The release snapshot is post-change: SHIFT is already gone.
        let e = t.convert(0x2A, false).unwrap();
        assert_eq!(
            e,
            ev(
                Key::ShiftLeft,
                KeyEventKind::Release,
                Modifiers::NONE,
                false
            )
        );
    }

    #[test]
    fn both_sides_of_a_group() {
        let mut t = Tracker::default();
        let e = t.convert(0x2A, true).unwrap();
        assert_eq!(e.modifiers, Modifiers::SHIFT);
        let e = t.convert(0x36, true).unwrap();
        assert_eq!(e.modifiers, Modifiers::SHIFT);
        // Releasing one side keeps the group bit.
        let e = t.convert(0x2A, false).unwrap();
        assert_eq!(e.modifiers, Modifiers::SHIFT);
        let e = t.convert(0x36, false).unwrap();
        assert_eq!(e.modifiers, Modifiers::NONE);
    }

    #[test]
    fn duplicate_modifier_press_is_dropped() {
        let mut t = Tracker::default();
        assert!(t.convert(0x2A, true).is_some());
        // A duplicate down (second keyboard, synthetic input) is not a
        // repeat and not a fresh press: it leaves the stream entirely.
        assert_eq!(t.convert(0x2A, true), None);
        // The pair closes normally.
        let e = t.convert(0x2A, false).unwrap();
        assert_eq!(e.kind, KeyEventKind::Release);
    }

    #[test]
    fn caps_lock_is_a_plain_key() {
        let mut t = Tracker::default();
        // Caps Lock produces plain press/release events and never leaks into
        // the snapshot.
        let e = t.convert(0x3A, true).unwrap();
        assert_eq!(
            e,
            ev(Key::CapsLock, KeyEventKind::Press, Modifiers::NONE, false)
        );
        let e = t.convert(0x3A, false).unwrap();
        assert_eq!(
            e,
            ev(Key::CapsLock, KeyEventKind::Release, Modifiers::NONE, false)
        );
    }

    #[test]
    fn release_without_press_passes_through() {
        let mut t = Tracker::default();
        let e = t.convert(0x1E, false).unwrap();
        assert_eq!(
            e,
            ev(Key::KeyA, KeyEventKind::Release, Modifiers::NONE, false)
        );
    }

    #[test]
    fn reconcile_is_a_no_op_when_consistent() {
        let mut t = Tracker::default();
        t.convert(0x1E, true).unwrap();
        assert!(t.reconcile(|_| true, None).is_empty());
        // Still tracked: a second press reads as a repeat.
        assert!(t.convert(0x1E, true).unwrap().is_repeat);
    }

    #[test]
    fn reconcile_drops_physically_released() {
        let mut t = Tracker::default();
        t.convert(0x2A, true).unwrap(); // ShiftLeft down
        t.convert(0x1E, true).unwrap(); // KeyA down
        // ShiftLeft's release was missed; KeyA is still held.
        let stale = t.reconcile(|code| code == 0x1E, None);
        assert_eq!(stale, vec![0x2A]);
        // The phantom group bit is gone from subsequent snapshots.
        let e = t.convert(0x1F, true).unwrap();
        assert_eq!(e.modifiers, Modifiers::NONE);
    }

    #[test]
    fn reconcile_makes_the_next_press_fresh() {
        let mut t = Tracker::default();
        t.convert(0x2A, true).unwrap();
        // The release was missed and the key is pressed again after the gap:
        // without reconcile this down would vanish as a duplicate.
        assert_eq!(t.reconcile(|_| false, None), vec![0x2A]);
        let e = t.convert(0x2A, true).unwrap();
        assert_eq!(e.kind, KeyEventKind::Press);
        assert!(!e.is_repeat);
    }

    #[test]
    fn reconcile_skips_the_current_release() {
        let mut t = Tracker::default();
        t.convert(0x1E, true).unwrap();
        // The real release is arriving now: tracked-but-released is expected.
        assert!(t.reconcile(|_| false, Some(0x1E)).is_empty());
        let e = t.convert(0x1E, false).unwrap();
        assert_eq!(e.kind, KeyEventKind::Release);
    }

    #[test]
    fn init_from_physical_seeds_held_keys() {
        let mut t = Tracker::default();
        // ControlLeft, AltRight and KeyA are physically held at install time.
        t.init_from_physical(|code| matches!(code, 0x1D | 0xE038 | 0x1E));
        // The snapshot reflects the seeded modifiers.
        let e = t.convert(0x1F, true).unwrap();
        assert_eq!(
            e,
            ev(
                Key::KeyS,
                KeyEventKind::Press,
                Modifiers::CONTROL | Modifiers::ALT,
                false
            )
        );
        // KeyA was held since before the hook: its autorepeat reads as a
        // repeat, not a fresh press.
        let e = t.convert(0x1E, true).unwrap();
        assert!(e.is_repeat);
        // Seeded modifiers behave like observed presses: releasing one
        // clears its group bit.
        let e = t.convert(0x1D, false).unwrap();
        assert_eq!(
            e,
            ev(
                Key::ControlLeft,
                KeyEventKind::Release,
                Modifiers::ALT,
                false
            )
        );
    }
}
