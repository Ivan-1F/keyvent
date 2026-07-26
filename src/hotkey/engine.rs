//! The hotkey matching engine: pure logic, no FFI, fully unit-testable.
//!
//! One `Matcher` lives on the tap thread inside the engine callback. It sees
//! every `KeyEvent`, maintains which registered hotkeys are currently active,
//! emits `HotkeyEvent`s for transitions, and decides whether the raw event
//! should be swallowed.
//!
//! Activation semantics (see `docs` on `Hotkey`):
//! - Trigger hotkey: Down when the trigger is pressed with an exact modifier
//!   match; Up when the trigger is released. Modifier drift mid-hold does not
//!   end it.
//! - Modifier-only hotkey: Down when the snapshot first equals the required
//!   set exactly; Up when the snapshot no longer contains the full set
//!   (extra modifiers pressed mid-hold do not end it).

use std::collections::HashMap;
use std::time::Instant;

use crate::{KeyEvent, KeyEventKind};

use super::{
    MATCH_BITS, HotkeyEvent, HotkeyId, Suppression, modifier_bit_of, modifiers_exact,
    modifiers_still_held,
};

/// A registered hotkey and its suppression mode.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Registered {
    pub hotkey: super::Hotkey,
    pub suppression: Suppression,
}

#[derive(Default)]
pub(crate) struct Matcher {
    /// Active hotkeys and when they went down.
    active: HashMap<HotkeyId, Instant>,
}

impl Matcher {
    /// Process one event. Returns the hotkey events to emit and whether the
    /// raw event should be swallowed.
    pub(crate) fn handle(
        &mut self,
        event: &KeyEvent,
        registered: &HashMap<HotkeyId, Registered>,
    ) -> (Vec<HotkeyEvent>, bool) {
        // Prune activations whose registration was removed mid-hold.
        self.active.retain(|id, _| registered.contains_key(id));

        let now = Instant::now();
        let mut out = Vec::new();

        for (id, reg) in registered {
            let is_active = self.active.contains_key(id);
            match reg.hotkey.key() {
                Some(trigger) => {
                    if event.key != trigger || event.is_repeat {
                        continue;
                    }
                    match event.kind {
                        KeyEventKind::Press if !is_active => {
                            // For a modifier trigger (e.g. OptionRight), the
                            // trigger's own group bit is part of the snapshot
                            // by definition; match the REST exactly. Regular
                            // triggers match the whole snapshot.
                            let matched = match modifier_bit_of(trigger) {
                                Some(group) => {
                                    (event.modifiers & MATCH_BITS) - group
                                        == reg.hotkey.modifiers()
                                }
                                None => {
                                    modifiers_exact(event.modifiers, reg.hotkey.modifiers())
                                }
                            };
                            if matched {
                                self.active.insert(*id, now);
                                out.push(HotkeyEvent::Down(*id));
                            }
                        }
                        KeyEventKind::Release if is_active => {
                            let started = self.active.remove(id).unwrap();
                            out.push(HotkeyEvent::Up {
                                id: *id,
                                held_for: now - started,
                            });
                        }
                        _ => {}
                    }
                }
                None => {
                    if !is_active && modifiers_exact(event.modifiers, reg.hotkey.modifiers()) {
                        self.active.insert(*id, now);
                        out.push(HotkeyEvent::Down(*id));
                    } else if is_active
                        && !modifiers_still_held(event.modifiers, reg.hotkey.modifiers())
                    {
                        let started = self.active.remove(id).unwrap();
                        out.push(HotkeyEvent::Up {
                            id: *id,
                            held_for: now - started,
                        });
                    }
                }
            }
        }

        // Swallow this raw event if it belongs to an active (or just-activated)
        // hotkey registered with Suppression::Swallow. For releases, the raw
        // engine follows the press decision regardless of what we return here.
        let swallow = out.iter().any(|e| {
            matches!(registered.get(&e.id()), Some(reg) if reg.suppression == Suppression::Swallow)
        }) || registered.iter().any(|(id, reg)| {
            reg.suppression == Suppression::Swallow
                && self.active.contains_key(id)
                && reg.hotkey.involves(event.key)
        });

        (out, swallow)
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

    fn reg(cmd_k: Suppression) -> (HotkeyId, HashMap<HotkeyId, Registered>) {
        let id = HotkeyId(0);
        let mut m = HashMap::new();
        m.insert(
            id,
            Registered {
                hotkey: Hotkey::new(Modifiers::COMMAND, Some(Key::KeyK)),
                suppression: cmd_k,
            },
        );
        (id, m)
    }

    #[test]
    fn trigger_hotkey_down_up() {
        let (id, set) = reg(Suppression::Pass);
        let mut m = Matcher::default();
        // K without Cmd: nothing.
        let (events, swallow) = m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::NONE), &set);
        assert!(events.is_empty() && !swallow);
        // Cmd+K: Down.
        let (events, swallow) = m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::COMMAND), &set);
        assert_eq!(events, vec![HotkeyEvent::Down(id)]);
        assert!(!swallow);
        // Repeat while held: no new event.
        let (events, _) = m.handle(&repeat(Key::KeyK, Modifiers::COMMAND), &set);
        assert!(events.is_empty());
        // Modifier drifts mid-hold (Cmd released first): still active, no Up.
        let (events, _) = m.handle(&ev(Key::CommandLeft, KeyEventKind::Release, Modifiers::NONE), &set);
        assert!(events.is_empty());
        // K released: Up.
        let (events, _) = m.handle(&ev(Key::KeyK, KeyEventKind::Release, Modifiers::NONE), &set);
        assert!(matches!(events.as_slice(), [HotkeyEvent::Up { id: i, .. }] if *i == id));
    }

    #[test]
    fn exact_match_rejects_extra_modifiers() {
        let (_id, set) = reg(Suppression::Pass);
        let mut m = Matcher::default();
        let (events, _) = m.handle(
            &ev(Key::KeyK, KeyEventKind::Press, Modifiers::COMMAND | Modifiers::SHIFT),
            &set,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn swallow_flag_only_for_swallow_registration() {
        let (id, set) = reg(Suppression::Swallow);
        let mut m = Matcher::default();
        let (events, swallow) = m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::COMMAND), &set);
        assert_eq!(events, vec![HotkeyEvent::Down(id)]);
        assert!(swallow);
    }

    #[test]
    fn modifier_only_single_option() {
        let id = HotkeyId(1);
        let mut set = HashMap::new();
        set.insert(
            id,
            Registered {
                hotkey: Hotkey::new(Modifiers::OPTION, None),
                suppression: Suppression::Swallow,
            },
        );
        let mut m = Matcher::default();
        // Option down (snapshot OPTION): Down + swallow.
        let (events, swallow) = m.handle(&ev(Key::OptionLeft, KeyEventKind::Press, Modifiers::OPTION), &set);
        assert_eq!(events, vec![HotkeyEvent::Down(id)]);
        assert!(swallow);
        // Typing while held: no transition, no swallow (unrelated key).
        let (events, swallow) = m.handle(&ev(Key::KeyA, KeyEventKind::Press, Modifiers::OPTION), &set);
        assert!(events.is_empty() && !swallow);
        // Option up: Up.
        let (events, _) = m.handle(&ev(Key::OptionLeft, KeyEventKind::Release, Modifiers::NONE), &set);
        assert!(matches!(events.as_slice(), [HotkeyEvent::Up { id: i, .. }] if *i == id));
    }

    #[test]
    fn modifier_only_survives_extra_modifier_mid_hold() {
        let id = HotkeyId(2);
        let mut set = HashMap::new();
        set.insert(
            id,
            Registered {
                hotkey: Hotkey::new(Modifiers::FN, None),
                suppression: Suppression::Pass,
            },
        );
        let mut m = Matcher::default();
        m.handle(&ev(Key::Function, KeyEventKind::Press, Modifiers::FN), &set);
        // Shift pressed mid-hold: set still contains FN, no Up.
        let (events, _) = m.handle(&ev(Key::ShiftLeft, KeyEventKind::Press, Modifiers::FN | Modifiers::SHIFT), &set);
        assert!(events.is_empty());
        // Fn released (snapshot SHIFT): Up.
        let (events, _) = m.handle(&ev(Key::Function, KeyEventKind::Release, Modifiers::SHIFT), &set);
        assert!(matches!(events.as_slice(), [HotkeyEvent::Up { id: i, .. }] if *i == id));
    }

    #[test]
    fn held_for_is_reported() {
        let (id, set) = reg(Suppression::Pass);
        let mut m = Matcher::default();
        m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::COMMAND), &set);
        std::thread::sleep(std::time::Duration::from_millis(10));
        let (events, _) = m.handle(&ev(Key::KeyK, KeyEventKind::Release, Modifiers::COMMAND), &set);
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
        let mut set = HashMap::new();
        set.insert(
            id,
            Registered {
                hotkey: Hotkey::new(Modifiers::NONE, Some(Key::OptionRight)),
                suppression: Suppression::Swallow,
            },
        );
        let mut m = Matcher::default();
        // Left option: not this hotkey.
        let (events, swallow) =
            m.handle(&ev(Key::OptionLeft, KeyEventKind::Press, Modifiers::OPTION), &set);
        assert!(events.is_empty() && !swallow);
        // Right option: Down + swallow.
        let (events, swallow) =
            m.handle(&ev(Key::OptionRight, KeyEventKind::Press, Modifiers::OPTION), &set);
        assert_eq!(events, vec![HotkeyEvent::Down(id)]);
        assert!(swallow);
        // Right released while left still held: Up.
        let (events, _) =
            m.handle(&ev(Key::OptionRight, KeyEventKind::Release, Modifiers::OPTION), &set);
        assert!(matches!(events.as_slice(), [HotkeyEvent::Up { id: i, .. }] if *i == id));
    }

    #[test]
    fn modifier_trigger_matches_other_modifiers_exactly() {
        let id = HotkeyId(4);
        let id2 = HotkeyId(5);
        let mut set = HashMap::new();
        set.insert(
            id,
            Registered {
                hotkey: Hotkey::new(Modifiers::NONE, Some(Key::OptionRight)),
                suppression: Suppression::Pass,
            },
        );
        set.insert(
            id2,
            Registered {
                hotkey: Hotkey::new(Modifiers::SHIFT, Some(Key::OptionRight)),
                suppression: Suppression::Pass,
            },
        );
        let mut m = Matcher::default();
        // Shift+RightOption matches only the {SHIFT, OptionRight} hotkey.
        let (events, _) = m.handle(
            &ev(
                Key::OptionRight,
                KeyEventKind::Press,
                Modifiers::OPTION | Modifiers::SHIFT,
            ),
            &set,
        );
        assert_eq!(events, vec![HotkeyEvent::Down(id2)]);
        // Plain RightOption now matches only the bare one.
        let (events, _) = m.handle(
            &ev(Key::OptionRight, KeyEventKind::Press, Modifiers::OPTION),
            &set,
        );
        assert_eq!(events, vec![HotkeyEvent::Down(id)]);
    }
}
