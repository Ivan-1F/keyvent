//! The hotkey matching engine: pure logic, no FFI, fully unit-testable.
//!
//! One `Matcher` lives on the tap thread inside the engine callback. It owns
//! the registration table (fed by commands from the manager), sees every
//! `KeyEvent`, maintains which registered hotkeys are currently active,
//! emits `HotkeyEvent`s for transitions, and decides whether the raw event
//! should be swallowed.
//!
//! Activation semantics (see `docs` on `Hotkey`):
//! - Trigger hotkey: Down when the trigger is pressed with an exact modifier
//!   match; Up when the trigger is released. Modifier drift mid-hold does not
//!   end it.
//! - Modifier-only hotkey: Down when the snapshot first equals the required set
//!   exactly; Up when the snapshot no longer contains the full set (extra
//!   modifiers pressed mid-hold do not end it).

use std::collections::HashMap;
use std::time::Instant;

use crate::{KeyEvent, KeyEventKind};

use super::{HotkeyEvent, HotkeyId, Suppression};

/// A registered hotkey and its suppression mode.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Registered {
    pub hotkey: super::Hotkey,
    pub suppression: Suppression,
}

#[derive(Default)]
pub(crate) struct Matcher {
    /// Registered hotkeys by id.
    registered: HashMap<HotkeyId, Registered>,
    /// Active hotkeys and when they went down.
    active: HashMap<HotkeyId, Instant>,
}

impl Matcher {
    pub(crate) fn register(&mut self, id: HotkeyId, reg: Registered) {
        self.registered.insert(id, reg);
    }

    /// Remove a registration, including any mid-hold activation: an
    /// unregistered hotkey simply never fires Up.
    pub(crate) fn unregister(&mut self, id: HotkeyId) {
        self.registered.remove(&id);
        self.active.remove(&id);
    }

    /// Process one event. Returns the hotkey events to emit and whether the
    /// raw event should be swallowed.
    pub(crate) fn handle(&mut self, event: &KeyEvent) -> (Vec<HotkeyEvent>, bool) {
        let mut out = Vec::new();
        let mut swallow = false;

        for (id, reg) in &self.registered {
            match reg.hotkey.key() {
                Some(trigger) => {
                    if event.key == trigger && !event.is_repeat {
                        match event.kind {
                            KeyEventKind::Press if !self.active.contains_key(id) => {
                                if reg.hotkey.modifiers_match(event.modifiers) {
                                    self.active.insert(*id, Instant::now());
                                    out.push(HotkeyEvent::Down(*id));
                                }
                            }
                            KeyEventKind::Release => deactivate(&mut self.active, *id, &mut out),
                            _ => {}
                        }
                    }
                }
                None => {
                    if reg.hotkey.modifiers_match(event.modifiers) {
                        if !self.active.contains_key(id) {
                            self.active.insert(*id, Instant::now());
                            out.push(HotkeyEvent::Down(*id));
                        }
                    } else if !event.modifiers.contains(reg.hotkey.modifiers()) {
                        deactivate(&mut self.active, *id, &mut out);
                    }
                }
            }

            // Swallow this raw event if it belongs to an active (or
            // just-activated) hotkey registered with Suppression::Swallow.
            // For releases, the raw engine follows the press decision
            // regardless of what we return here.
            if !swallow
                && reg.suppression == Suppression::Swallow
                && reg.hotkey.involves(event.key)
                && self.active.contains_key(id)
            {
                swallow = true;
            }
        }

        (out, swallow)
    }
}

/// End an activation, emitting Up with the held duration if it was active.
fn deactivate(active: &mut HashMap<HotkeyId, Instant>, id: HotkeyId, out: &mut Vec<HotkeyEvent>) {
    if let Some(started) = active.remove(&id) {
        out.push(HotkeyEvent::Up {
            id,
            held_for: started.elapsed(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::Hotkey;
    use crate::{Key, Modifiers};

    fn ev(key: Key, kind: KeyEventKind, mods: Modifiers) -> KeyEvent {
        KeyEvent {
            key,
            kind,
            modifiers: mods,
            is_repeat: false,
        }
    }

    fn repeat(key: Key, mods: Modifiers) -> KeyEvent {
        KeyEvent {
            key,
            kind: KeyEventKind::Press,
            modifiers: mods,
            is_repeat: true,
        }
    }

    fn matcher_with(id: HotkeyId, hotkey: Hotkey, suppression: Suppression) -> Matcher {
        let mut m = Matcher::default();
        m.register(
            id,
            Registered {
                hotkey,
                suppression,
            },
        );
        m
    }

    /// A matcher with one Cmd+K hotkey registered as id 0.
    fn cmd_k(suppression: Suppression) -> (HotkeyId, Matcher) {
        let id = HotkeyId(0);
        let m = matcher_with(
            id,
            Hotkey::new(Modifiers::COMMAND, Some(Key::KeyK)),
            suppression,
        );
        (id, m)
    }

    #[test]
    fn trigger_hotkey_down_up() {
        let (id, mut m) = cmd_k(Suppression::Pass);
        // K without Cmd: nothing.
        let (events, swallow) = m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::NONE));
        assert!(events.is_empty() && !swallow);
        // Cmd+K: Down.
        let (events, swallow) = m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::COMMAND));
        assert_eq!(events, vec![HotkeyEvent::Down(id)]);
        assert!(!swallow);
        // Repeat while held: no new event.
        let (events, _) = m.handle(&repeat(Key::KeyK, Modifiers::COMMAND));
        assert!(events.is_empty());
        // Modifier drifts mid-hold (Cmd released first): still active, no Up.
        let (events, _) = m.handle(&ev(
            Key::CommandLeft,
            KeyEventKind::Release,
            Modifiers::NONE,
        ));
        assert!(events.is_empty());
        // K released: Up.
        let (events, _) = m.handle(&ev(Key::KeyK, KeyEventKind::Release, Modifiers::NONE));
        assert!(matches!(events.as_slice(), [HotkeyEvent::Up { id: i, .. }] if *i == id));
    }

    #[test]
    fn exact_match_rejects_extra_modifiers() {
        let (_id, mut m) = cmd_k(Suppression::Pass);
        let (events, _) = m.handle(&ev(
            Key::KeyK,
            KeyEventKind::Press,
            Modifiers::COMMAND | Modifiers::SHIFT,
        ));
        assert!(events.is_empty());
    }

    #[test]
    fn swallow_flag_only_for_swallow_registration() {
        let (id, mut m) = cmd_k(Suppression::Swallow);
        let (events, swallow) = m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::COMMAND));
        assert_eq!(events, vec![HotkeyEvent::Down(id)]);
        assert!(swallow);
    }

    #[test]
    fn unregister_mid_hold_drops_activation() {
        let (id, mut m) = cmd_k(Suppression::Pass);
        m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::COMMAND));
        m.unregister(id);
        // Release after unregister: no Up.
        let (events, _) = m.handle(&ev(Key::KeyK, KeyEventKind::Release, Modifiers::NONE));
        assert!(events.is_empty());
    }

    #[test]
    fn modifier_only_single_option() {
        let id = HotkeyId(1);
        let mut m = matcher_with(
            id,
            Hotkey::new(Modifiers::OPTION, None),
            Suppression::Swallow,
        );
        // Option down (snapshot OPTION): Down + swallow.
        let (events, swallow) =
            m.handle(&ev(Key::OptionLeft, KeyEventKind::Press, Modifiers::OPTION));
        assert_eq!(events, vec![HotkeyEvent::Down(id)]);
        assert!(swallow);
        // Typing while held: no transition, no swallow (unrelated key).
        let (events, swallow) = m.handle(&ev(Key::KeyA, KeyEventKind::Press, Modifiers::OPTION));
        assert!(events.is_empty() && !swallow);
        // Option up: Up.
        let (events, _) = m.handle(&ev(Key::OptionLeft, KeyEventKind::Release, Modifiers::NONE));
        assert!(matches!(events.as_slice(), [HotkeyEvent::Up { id: i, .. }] if *i == id));
    }

    #[test]
    fn modifier_only_survives_extra_modifier_mid_hold() {
        let id = HotkeyId(2);
        let mut m = matcher_with(id, Hotkey::new(Modifiers::FN, None), Suppression::Pass);
        m.handle(&ev(Key::Function, KeyEventKind::Press, Modifiers::FN));
        // Shift pressed mid-hold: set still contains FN, no Up.
        let (events, _) = m.handle(&ev(
            Key::ShiftLeft,
            KeyEventKind::Press,
            Modifiers::FN | Modifiers::SHIFT,
        ));
        assert!(events.is_empty());
        // Fn released (snapshot SHIFT): Up.
        let (events, _) = m.handle(&ev(Key::Function, KeyEventKind::Release, Modifiers::SHIFT));
        assert!(matches!(events.as_slice(), [HotkeyEvent::Up { id: i, .. }] if *i == id));
    }

    #[test]
    fn held_for_is_reported() {
        let (id, mut m) = cmd_k(Suppression::Pass);
        m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::COMMAND));
        std::thread::sleep(std::time::Duration::from_millis(10));
        let (events, _) = m.handle(&ev(Key::KeyK, KeyEventKind::Release, Modifiers::COMMAND));
        match events.as_slice() {
            [HotkeyEvent::Up { id: i, held_for }] => {
                assert_eq!(*i, id);
                assert!(*held_for >= std::time::Duration::from_millis(10));
            }
            other => panic!("expected Up, got {other:?}"),
        }
    }

    #[test]
    fn side_specific_modifier_trigger() {
        let id = HotkeyId(3);
        let mut m = matcher_with(
            id,
            Hotkey::new(Modifiers::NONE, Some(Key::OptionRight)),
            Suppression::Swallow,
        );
        // Left option: not this hotkey.
        let (events, swallow) =
            m.handle(&ev(Key::OptionLeft, KeyEventKind::Press, Modifiers::OPTION));
        assert!(events.is_empty() && !swallow);
        // Right option: Down + swallow.
        let (events, swallow) = m.handle(&ev(
            Key::OptionRight,
            KeyEventKind::Press,
            Modifiers::OPTION,
        ));
        assert_eq!(events, vec![HotkeyEvent::Down(id)]);
        assert!(swallow);
        // Right released while left still held: Up.
        let (events, _) = m.handle(&ev(
            Key::OptionRight,
            KeyEventKind::Release,
            Modifiers::OPTION,
        ));
        assert!(matches!(events.as_slice(), [HotkeyEvent::Up { id: i, .. }] if *i == id));
    }

    #[test]
    fn modifier_trigger_matches_other_modifiers_exactly() {
        let id = HotkeyId(4);
        let id2 = HotkeyId(5);
        let mut m = matcher_with(
            id,
            Hotkey::new(Modifiers::NONE, Some(Key::OptionRight)),
            Suppression::Pass,
        );
        m.register(
            id2,
            Registered {
                hotkey: Hotkey::new(Modifiers::SHIFT, Some(Key::OptionRight)),
                suppression: Suppression::Pass,
            },
        );
        // Shift+RightOption matches only the {SHIFT, OptionRight} hotkey.
        let (events, _) = m.handle(&ev(
            Key::OptionRight,
            KeyEventKind::Press,
            Modifiers::OPTION | Modifiers::SHIFT,
        ));
        assert_eq!(events, vec![HotkeyEvent::Down(id2)]);
        // Plain RightOption now matches only the bare one.
        let (events, _) = m.handle(&ev(
            Key::OptionRight,
            KeyEventKind::Press,
            Modifiers::OPTION,
        ));
        assert_eq!(events, vec![HotkeyEvent::Down(id)]);
    }
}
