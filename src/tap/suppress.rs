//! Suppression bookkeeping: keeps the OS-side event stream consistent.
//!
//! The invariant, per key-press cycle: **press and release must travel
//! together.** A swallowed press must be matched by a swallowed release
//! (otherwise apps see a release for a press they never witnessed), and a
//! passed press must be matched by a passed release (otherwise the OS believes
//! the key is held forever — a system-wide stuck key).
//!
//! Decision ownership:
//! - The callback owns the *press* decision, once per cycle.
//! - The *release* decision is derived, never asked: it follows whatever the
//!   press did.
//! - Key repeats are free: swallowing them is harmless (the OS just sees fewer
//!   repeated KeyDowns), so the callback decides per repeat — but repeats
//!   never create bookkeeping entries, so they cannot corrupt the release
//!   decision.
//!
//! After an event-stream interruption (tap timeout, teardown), presses may
//! have lost their releases. [`Suppression::reconcile`] compares the book
//! against physical key state and reports which tracked presses are actually
//! up, so a later rebuild doesn't force-swallow releases for presses the OS
//! actually saw.

use std::collections::HashSet;

use crate::keycode::Keycode;

/// The Fn/Globe key's keycode.
pub(crate) const FN_KEYCODE: Keycode = 63;
/// Keycode of the synthesized event macOS emits on Fn release to trigger the
/// system "Press 🌐 key to" action (emoji panel, input-source switch, …).
pub(crate) const GLOBE_ACTION_KEYCODE: Keycode = 179;

/// What to do with the raw OS event after bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Return NULL from the tap callback; the event never reaches other apps.
    Swallow,
    /// Pass the event through unchanged.
    Pass,
}

#[derive(Default)]
pub(crate) struct Suppression {
    /// Keycodes whose initial press was swallowed and whose release is still
    /// pending. Only initial presses create entries — never repeats.
    swallowed_presses: HashSet<Keycode>,
}

impl Suppression {
    /// Book a press. `user_swallow` is the callback's decision.
    pub(crate) fn press(&mut self, code: Keycode, is_repeat: bool, user_swallow: bool) -> Action {
        if self.swallowed_presses.contains(&code) {
            // Mid-cycle of a swallowed press: everything (repeats, re-press
            // after a lost release) stays swallowed.
            return Action::Swallow;
        }
        if is_repeat {
            // Safe to swallow (fewer repeated KeyDowns for the OS), but never
            // recorded: the release must follow the INITIAL press's decision.
            return if user_swallow {
                Action::Swallow
            } else {
                Action::Pass
            };
        }
        if user_swallow {
            self.swallowed_presses.insert(code);
            Action::Swallow
        } else {
            Action::Pass
        }
    }

    /// Book a release. The decision is derived from the press, never asked:
    /// swallowed press -> forced swallow; anything else -> forced pass, so a
    /// passed press can never leave the OS with a stuck key.
    pub(crate) fn release(&mut self, code: Keycode) -> Action {
        if self.swallowed_presses.remove(&code) {
            Action::Swallow
        } else {
            Action::Pass
        }
    }

    /// Drop entries whose key is no longer physically held; returns their
    /// keycodes so the tap layer can keep the books clean across rebuilds.
    /// `is_held` reports physical state (CGEventSourceKeyState).
    pub(crate) fn reconcile(&mut self, is_held: impl Fn(Keycode) -> bool) -> Vec<Keycode> {
        let released: Vec<Keycode> = self
            .swallowed_presses
            .iter()
            .filter(|code| !is_held(**code))
            .copied()
            .collect();
        for code in &released {
            self.swallowed_presses.remove(code);
        }
        released
    }

    /// True if `code` is the globe-action event and the Fn cycle that
    /// produced it is being swallowed.
    pub(crate) fn swallows_globe_action(&self, code: Keycode) -> bool {
        code == GLOBE_ACTION_KEYCODE && self.swallowed_presses.contains(&FN_KEYCODE)
    }

    #[cfg(test)]
    fn is_tracking(&self, code: Keycode) -> bool {
        self.swallowed_presses.contains(&code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swallowed_press_forces_swallowed_release() {
        let mut s = Suppression::default();
        assert_eq!(s.press(36, false, true), Action::Swallow);
        assert_eq!(s.release(36), Action::Swallow);
        assert!(!s.is_tracking(36));
    }

    #[test]
    fn release_of_passed_press_always_passes() {
        let mut s = Suppression::default();
        assert_eq!(s.press(36, false, false), Action::Pass);
        // The callback may WANT to swallow this release; it doesn't get a say —
        // the OS saw the press, so it must see the release.
        assert_eq!(s.release(36), Action::Pass);
    }

    #[test]
    fn repeat_during_swallowed_press_is_swallowed() {
        let mut s = Suppression::default();
        assert_eq!(s.press(36, false, true), Action::Swallow);
        assert_eq!(s.press(36, true, false), Action::Swallow); // repeat, cb passes
        assert_eq!(s.press(36, true, false), Action::Swallow);
        assert_eq!(s.release(36), Action::Swallow);
        assert_eq!(s.press(36, false, false), Action::Pass); // back to normal
    }

    #[test]
    fn swallowing_a_repeat_does_not_stick_the_key() {
        let mut s = Suppression::default();
        // Hold-to-activate: initial press passes, a later repeat is swallowed.
        assert_eq!(s.press(36, false, false), Action::Pass);
        assert_eq!(s.press(36, true, true), Action::Swallow); // repeat swallowed
        // ...but NOT booked, so the release must still reach the OS.
        assert_eq!(s.release(36), Action::Pass);
        assert!(!s.is_tracking(36));
    }

    #[test]
    fn globe_action_follows_fn_swallow() {
        let mut s = Suppression::default();
        // Fn cycle not claimed: the globe-action event passes.
        assert!(!s.swallows_globe_action(GLOBE_ACTION_KEYCODE));
        // Fn press swallowed: the whole cycle — Fn release AND the
        // globe-action event — is suppressed.
        assert_eq!(s.press(FN_KEYCODE, false, true), Action::Swallow);
        assert!(s.swallows_globe_action(GLOBE_ACTION_KEYCODE));
        // After the Fn release is booked, the cycle is over.
        assert_eq!(s.release(FN_KEYCODE), Action::Swallow);
        assert!(!s.swallows_globe_action(GLOBE_ACTION_KEYCODE));
        // Unrelated keycodes are never globe actions.
        assert!(!s.swallows_globe_action(36));
    }

    #[test]
    fn reconcile_drops_physically_released() {
        let mut s = Suppression::default();
        s.press(36, false, true);
        s.press(56, false, true);
        // 36 physically released, 56 still held.
        let released = s.reconcile(|code| code == 56);
        assert_eq!(released, vec![36]);
        assert!(s.is_tracking(56));
        assert!(!s.is_tracking(36));
    }
}
