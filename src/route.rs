//! Per-event decisions shared by both platforms: which events reach the
//! callback, and which reach the OS.

use crate::dispatcher::BoxedCallback;
use crate::pairing::{Pairing, Plan};
use crate::platform::Keycode;
use crate::suppress::{Action, Suppression};
use crate::{Key, KeyEvent, KeyEventKind, Modifiers};

/// Run one converted event past [`Pairing`], the callback, and
/// [`Suppression`]; returns whether the raw event passes to the OS.
/// `snapshot` is called only when a synthetic release has to be emitted.
pub(crate) fn route(
    pairing: &mut Pairing,
    suppression: &mut Suppression,
    callback: &mut BoxedCallback,
    code: Keycode,
    event: KeyEvent,
    snapshot: impl Fn() -> Modifiers,
) -> Action {
    // Consumer side: the pairing ledger decides what the callback may see.
    // OS side: the suppression ledger decides the raw event's fate. The two
    // are independent — e.g. an orphan release is hidden from the consumer
    // but still passed through if the OS saw its press.
    match event.kind {
        KeyEventKind::Press => {
            if pairing.press(code, event.is_repeat) == Plan::CloseThenDeliver {
                // A previous release was lost: close the dangling press first.
                dispatch(callback, synthetic_release(event.key, snapshot()));
            }
            let user_swallow = dispatch(callback, event);
            suppression.press(code, event.is_repeat, user_swallow)
        }
        KeyEventKind::Release => {
            if pairing.release(code) == Plan::Deliver {
                dispatch(callback, event);
            }
            // The release's swallow decision is derived from the press
            // (see Suppression); the callback is not consulted.
            suppression.release(code)
        }
    }
}

/// Dispatch one event to the callback. Last-resort boundary: the dispatcher
/// isolates user-callback panics itself, so this only catches panics in the
/// dispatch machinery — which must never unwind into the platform's event
/// loop. Returns the callback's swallow decision (false on panic).
pub(crate) fn dispatch(callback: &mut BoxedCallback, event: KeyEvent) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(event))) {
        Ok(swallow) => swallow,
        Err(_) => {
            log::error!("keyvent: callback dispatch panicked; event passed through");
            false
        }
    }
}

/// A synthetic Release closing a dangling consumer-side press.
pub(crate) fn synthetic_release(key: Key, modifiers: Modifiers) -> KeyEvent {
    KeyEvent {
        key,
        kind: KeyEventKind::Release,
        modifiers,
        is_repeat: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<KeyEvent>>>;

    /// Fresh [`Pairing`] + [`Suppression`] and a callback that records
    /// events and always returns `swallow`.
    fn rig(swallow: bool) -> (Pairing, Suppression, BoxedCallback, Seen) {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let callback: BoxedCallback = Box::new(move |event| {
            seen2.lock().unwrap().push(event);
            swallow
        });
        (Pairing::default(), Suppression::default(), callback, seen)
    }

    fn ev(key: Key, kind: KeyEventKind, is_repeat: bool) -> KeyEvent {
        KeyEvent {
            key,
            kind,
            modifiers: Modifiers::NONE,
            is_repeat,
        }
    }

    fn press(key: Key) -> KeyEvent {
        ev(key, KeyEventKind::Press, false)
    }

    fn release(key: Key) -> KeyEvent {
        ev(key, KeyEventKind::Release, false)
    }

    fn recorded(seen: &Seen) -> Vec<KeyEvent> {
        seen.lock().unwrap().clone()
    }

    #[test]
    fn passed_cycle_delivers_and_passes() {
        let (mut p, mut s, mut cb, seen) = rig(false);
        let action = route(
            &mut p,
            &mut s,
            &mut cb,
            36,
            press(Key::Return),
            Modifiers::empty,
        );
        assert_eq!(action, Action::Pass);
        let action = route(
            &mut p,
            &mut s,
            &mut cb,
            36,
            release(Key::Return),
            Modifiers::empty,
        );
        assert_eq!(action, Action::Pass);
        assert_eq!(
            recorded(&seen),
            vec![press(Key::Return), release(Key::Return)]
        );
    }

    #[test]
    fn swallowed_cycle_closes_for_consumer_but_not_os() {
        let (mut p, mut s, mut cb, seen) = rig(true);
        let action = route(
            &mut p,
            &mut s,
            &mut cb,
            36,
            press(Key::Return),
            Modifiers::empty,
        );
        assert_eq!(action, Action::Swallow);
        // The release still reaches the consumer (their press needs closure)
        // but is withheld from the OS (matching the swallowed press).
        let action = route(
            &mut p,
            &mut s,
            &mut cb,
            36,
            release(Key::Return),
            Modifiers::empty,
        );
        assert_eq!(action, Action::Swallow);
        assert_eq!(
            recorded(&seen),
            vec![press(Key::Return), release(Key::Return)]
        );
    }

    #[test]
    fn orphan_release_is_hidden_from_consumer_but_passes() {
        let (mut p, mut s, mut cb, seen) = rig(false);
        let action = route(
            &mut p,
            &mut s,
            &mut cb,
            36,
            release(Key::Return),
            Modifiers::empty,
        );
        assert_eq!(action, Action::Pass);
        assert!(recorded(&seen).is_empty());
    }

    #[test]
    fn repress_closes_dangling_press_first() {
        let (mut p, mut s, mut cb, seen) = rig(false);
        let _ = route(
            &mut p,
            &mut s,
            &mut cb,
            36,
            press(Key::Return),
            Modifiers::empty,
        );
        // The release was lost; the re-press first emits a synthetic release
        // carrying the live snapshot's modifiers.
        let action = route(&mut p, &mut s, &mut cb, 36, press(Key::Return), || {
            Modifiers::SHIFT
        });
        assert_eq!(action, Action::Pass);
        assert_eq!(
            recorded(&seen),
            vec![
                press(Key::Return),
                synthetic_release(Key::Return, Modifiers::SHIFT),
                press(Key::Return),
            ]
        );
    }

    #[test]
    fn repeat_press_never_closes_the_pair() {
        let (mut p, mut s, mut cb, seen) = rig(false);
        let _ = route(
            &mut p,
            &mut s,
            &mut cb,
            36,
            press(Key::Return),
            Modifiers::empty,
        );
        let action = route(
            &mut p,
            &mut s,
            &mut cb,
            36,
            ev(Key::Return, KeyEventKind::Press, true),
            Modifiers::empty,
        );
        assert_eq!(action, Action::Pass);
        assert_eq!(
            recorded(&seen),
            vec![
                press(Key::Return),
                ev(Key::Return, KeyEventKind::Press, true)
            ]
        );
    }
}
