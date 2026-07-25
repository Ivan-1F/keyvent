//! Suppression bookkeeping: keeps the OS-side event stream consistent.
//!
//! The invariant: **a swallowed press must be matched by a swallowed release.**
//! If the press was eaten but the release passes through, the focused app sees
//! a release for a press it never witnessed (phantom release). If the press
//! passed but the release is eaten, the OS believes the key is still held.
//!
//! The user callback decides press suppression freely. Release suppression is
//! *forced* for keys whose press was swallowed; otherwise the callback decides.
//!
//! After an event-stream interruption (tap timeout, teardown), presses may
//! have lost their releases. [`Suppression::reconcile`] compares the book
//! against physical key state and reports which tracked presses are actually
//! up, so the tap layer can emit synthetic release events to the consumer.

use std::collections::HashSet;

use crate::keycode::Keycode;

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
    /// Keycodes whose press was swallowed and whose release is still pending.
    swallowed_presses: HashSet<Keycode>,
}

impl Suppression {
    /// Book a press. `user_swallow` is the callback's decision.
    ///
    /// While a press of this key is being swallowed, ALL its subsequent down
    /// events (key repeats, or a re-press after a lost release) are swallowed
    /// too — otherwise the OS would see repeated KeyDowns for a press it never
    /// witnessed.
    pub(crate) fn press(&mut self, code: Keycode, user_swallow: bool) -> Action {
        if user_swallow || self.swallowed_presses.contains(&code) {
            self.swallowed_presses.insert(code);
            Action::Swallow
        } else {
            Action::Pass
        }
    }

    /// Book a release. Swallow is forced if the matching press was swallowed.
    pub(crate) fn release(&mut self, code: Keycode, user_swallow: bool) -> Action {
        if self.swallowed_presses.remove(&code) {
            return Action::Swallow;
        }
        if user_swallow {
            Action::Swallow
        } else {
            Action::Pass
        }
    }

    /// Drop entries whose key is no longer physically held; returns their
    /// keycodes so the tap layer can emit synthetic releases to the consumer.
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
        assert_eq!(s.press(36, true), Action::Swallow);
        // Callback would pass the release; bookkeeping forces swallow.
        assert_eq!(s.release(36, false), Action::Swallow);
        assert!(!s.is_tracking(36));
    }

    #[test]
    fn passed_press_release_follows_callback() {
        let mut s = Suppression::default();
        assert_eq!(s.press(36, false), Action::Pass);
        assert_eq!(s.release(36, false), Action::Pass);
        // Callback may still swallow a release of a passed press (its choice).
        assert_eq!(s.press(36, false), Action::Pass);
        assert_eq!(s.release(36, true), Action::Swallow);
    }

    #[test]
    fn repeat_during_swallowed_press_is_swallowed() {
        let mut s = Suppression::default();
        assert_eq!(s.press(36, true), Action::Swallow);
        // Callback passes the key-repeat downs; bookkeeping still swallows them.
        assert_eq!(s.press(36, false), Action::Swallow);
        assert_eq!(s.press(36, false), Action::Swallow);
        assert_eq!(s.release(36, false), Action::Swallow);
        // After the release, the key is back to normal.
        assert_eq!(s.press(36, false), Action::Pass);
    }

    #[test]
    fn reconcile_drops_physically_released() {
        let mut s = Suppression::default();
        s.press(36, true);
        s.press(56, true);
        // 36 physically released, 56 still held.
        let released = s.reconcile(|code| code == 56);
        assert_eq!(released, vec![36]);
        assert!(s.is_tracking(56));
        assert!(!s.is_tracking(36));
    }
}
