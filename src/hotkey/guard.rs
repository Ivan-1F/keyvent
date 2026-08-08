//! `HotkeyEvents`: one registered hotkey and its event stream; dropping it
//! unregisters. [`HotkeyEvents::guard`] hands out a second handle for
//! controlling the registration from another thread.
//!
//! The matching state machine (`Matcher`) is pure logic, fully
//! unit-testable; no user code ever runs on the tap thread.
//!
//! Activation semantics (see `docs` on [`Hotkey`]):
//! - Trigger hotkey: Down when the trigger is pressed with an exact modifier
//!   match; Up when the trigger is released. Modifier drift mid-hold does not
//!   end it.
//! - Modifier-only hotkey: Down when the snapshot first equals the required
//!   set exactly; Up when the snapshot no longer contains the full set (extra
//!   modifiers pressed mid-hold do not end it).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossbeam_channel as channel;
use crossbeam_channel::select_biased;

use crate::cancel::{CancelToken, CancelWatcher, cancel_token};
use crate::status::StatusBroadcast;
use crate::{Guard, KeyEvent, KeyEventKind, Status};

use super::record::RECORDING;
use super::{Hotkey, RecvError, RecvTimeoutError, Suppression, TryRecvError};

/// A hotkey activation or deactivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// The hotkey became active.
    Down,
    /// The hotkey became inactive; `held_for` is how long it was active.
    Up { held_for: Duration },
}

/// A registered hotkey and its event stream. Receive activations directly on
/// it; dropping it unregisters the hotkey.
///
/// HotkeyEvents share the process-wide keyvent engine (see [`crate::start`]).
pub struct HotkeyEvents {
    rx: channel::Receiver<HotkeyEvent>,
    /// Sees the guard's death; closing the stream is a receiver-side act.
    cancelled: CancelWatcher,
    /// Keeps the registration alive; dropping it unregisters the hotkey.
    _registration: Guard,
    pending_guard: Option<HotkeyGuard>,
    commands: mpsc::Sender<Command>,
}

/// A registered hotkey's control guard, see [`HotkeyEvents::guard`].
/// Dropping it unregisters the hotkey; the event channel closes at once.
pub struct HotkeyGuard {
    status: Arc<StatusBroadcast>,
    commands: mpsc::Sender<Command>,
    /// Its death tells the tap-thread callback to unregister.
    _cancel: CancelToken,
}

/// A pause/resume change sent to the matcher on the tap thread.
enum Command {
    SetEnabled(bool),
}

/// Register `hotkey`, returning its event stream. Matching is suspended
/// while a recording is in progress (see [`record`](crate::record)).
///
/// ```no_run
/// let hotkey: keyvent::Hotkey = "Super+K".parse().unwrap();
/// let events = keyvent::on(hotkey, keyvent::Suppression::Swallow);
/// while let Ok(event) = events.recv() {
///     // HotkeyEvent::Down / HotkeyEvent::Up { held_for }
/// }
/// // Dropping `events` unregisters the hotkey.
/// ```
pub fn on(hotkey: Hotkey, suppression: Suppression) -> HotkeyEvents {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (cancel, cancelled) = cancel_token();
    // The tap-thread closure watches its own clone; cancellation reaches
    // every watcher.
    let cancelled_on_tap = cancelled.clone();
    let (tx, rx) = channel::unbounded();
    let mut tx = Some(tx);
    let mut matcher = Matcher::new(hotkey, suppression);
    let mut enabled = true;
    let registration = crate::start(move |event| {
        // The guard is gone: stop matching, pass everything through. The
        // receiver side sees the cancellation on its own; dropping our
        // sender just stops queueing for a dead listener.
        if cancelled_on_tap.is_cancelled() {
            tx = None;
            return false;
        }
        // Pending pause/resume changes are applied before the match, so they
        // take effect on the very next event — with no locking on the tap
        // thread, per the callback contract.
        while let Ok(Command::SetEnabled(user)) = cmd_rx.try_recv() {
            enabled = user;
        }
        // Recording sessions pause every on guard; an in-flight activation
        // still completes on release, per the matcher's enabled semantics.
        matcher.set_enabled(enabled && RECORDING.load(Ordering::Relaxed) == 0);
        let (hotkey_event, swallow) = matcher.handle(&event);
        if let Some(e) = hotkey_event {
            if let Some(sender) = &tx {
                let _ = sender.send(e);
            }
        }
        swallow
    });
    let status = registration.status_broadcast();
    HotkeyEvents {
        rx,
        cancelled,
        _registration: registration,
        pending_guard: Some(HotkeyGuard {
            status,
            commands: cmd_tx.clone(),
            _cancel: cancel,
        }),
        commands: cmd_tx,
    }
}

impl HotkeyEvents {
    /// Take this registration's control guard (at most once).
    pub fn guard(&mut self) -> Option<HotkeyGuard> {
        self.pending_guard.take()
    }

    /// Block until the next activation, `Err` once the channel is closed.
    pub fn recv(&self) -> Result<HotkeyEvent, RecvError> {
        // Cancellation wins over queued events: once the guard is gone the
        // stream is closed, whatever is still buffered.
        select_biased! {
            recv(self.cancelled.as_receiver()) -> _ => Err(RecvError),
            recv(&self.rx) -> msg => msg.map_err(|_| RecvError),
        }
    }

    /// The next activation if one is pending.
    pub fn try_recv(&self) -> Result<HotkeyEvent, TryRecvError> {
        if self.cancelled.is_cancelled() {
            return Err(TryRecvError::Closed);
        }
        self.rx.try_recv().map_err(TryRecvError::from_channel_error)
    }

    /// Block up to `timeout` for the next activation.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<HotkeyEvent, RecvTimeoutError> {
        select_biased! {
            recv(self.cancelled.as_receiver()) -> _ => Err(RecvTimeoutError::Closed),
            recv(&self.rx) -> msg => msg.map_err(|_| RecvTimeoutError::Closed),
            recv(channel::after(timeout)) -> _ => Err(RecvTimeoutError::Timeout),
        }
    }

    /// An iterator blocking on each activation.
    pub fn iter(&self) -> impl Iterator<Item = HotkeyEvent> + '_ {
        std::iter::from_fn(|| self.recv().ok())
    }

    /// An iterator over the pending activations.
    pub fn try_iter(&self) -> impl Iterator<Item = HotkeyEvent> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }

    /// Suspend matching: no new activations start and nothing is swallowed.
    /// An activation already in progress still fires Up when released.
    pub fn pause(&self) {
        let _ = self.commands.send(Command::SetEnabled(false));
    }

    /// Resume matching after [`pause`](Self::pause).
    pub fn resume(&self) {
        let _ = self.commands.send(Command::SetEnabled(true));
    }

    /// Permission lifecycle status of the underlying engine.
    pub fn status(&self) -> Status {
        self._registration.status()
    }

    /// A new subscription to engine status changes.
    pub fn status_receiver(&self) -> mpsc::Receiver<Status> {
        self._registration.status_receiver()
    }
}

impl HotkeyGuard {
    /// Suspend matching: no new activations start and nothing is swallowed.
    /// An activation already in progress still fires Up when released.
    pub fn pause(&self) {
        let _ = self.commands.send(Command::SetEnabled(false));
    }

    /// Resume matching after [`pause`](Self::pause).
    pub fn resume(&self) {
        let _ = self.commands.send(Command::SetEnabled(true));
    }

    /// Permission lifecycle status of the underlying engine.
    pub fn status(&self) -> Status {
        self.status.current()
    }

    /// A new subscription to engine status changes.
    pub fn status_receiver(&self) -> mpsc::Receiver<Status> {
        self.status.subscribe()
    }
}

/// One registered hotkey's matching state: pure logic, no FFI.
struct Matcher {
    hotkey: Hotkey,
    suppression: Suppression,
    /// When the hotkey went down, if it is currently active.
    active: Option<Instant>,
    /// When false, no new activations start and nothing is swallowed.
    enabled: bool,
}

impl Matcher {
    fn new(hotkey: Hotkey, suppression: Suppression) -> Self {
        Self {
            hotkey,
            suppression,
            active: None,
            enabled: true,
        }
    }

    /// Suspension stops new activations and all swallowing; an in-flight
    /// activation still completes on release.
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Process one event. Returns the activation event to emit, if any, and
    /// whether the raw event should be swallowed.
    fn handle(&mut self, event: &KeyEvent) -> (Option<HotkeyEvent>, bool) {
        let mut hotkey_event = None;
        match self.hotkey.key() {
            Some(trigger) => {
                if event.key == trigger && !event.is_repeat {
                    match event.kind {
                        KeyEventKind::Press if self.enabled && self.active.is_none() => {
                            if self.hotkey.modifiers_match(event.modifiers) {
                                self.active = Some(Instant::now());
                                hotkey_event = Some(HotkeyEvent::Down);
                            }
                        }
                        KeyEventKind::Release => hotkey_event = self.deactivate(),
                        _ => {}
                    }
                }
            }
            None => {
                if self.hotkey.modifiers_match(event.modifiers) {
                    if self.enabled && self.active.is_none() {
                        self.active = Some(Instant::now());
                        hotkey_event = Some(HotkeyEvent::Down);
                    }
                } else if !event.modifiers.contains(self.hotkey.modifiers()) {
                    hotkey_event = self.deactivate();
                }
            }
        }

        // Swallow this raw event if it belongs to an active (or
        // just-activated) hotkey registered with Suppression::Swallow.
        // For releases, the raw engine follows the press decision
        // regardless of what we return here.
        let swallow = self.enabled
            && self.suppression == Suppression::Swallow
            && self.hotkey.involves(event.key)
            && self.active.is_some();
        (hotkey_event, swallow)
    }

    /// End the activation, emitting Up with the held duration if active.
    fn deactivate(&mut self) -> Option<HotkeyEvent> {
        self.active.take().map(|started| HotkeyEvent::Up {
            held_for: started.elapsed(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn super_k(suppression: Suppression) -> Matcher {
        Matcher::new(Hotkey::new(Modifiers::SUPER, Some(Key::KeyK)), suppression)
    }

    #[test]
    fn trigger_hotkey_down_up() {
        let mut m = super_k(Suppression::Pass);
        // K without Super: nothing.
        let (event, swallow) = m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::NONE));
        assert!(event.is_none() && !swallow);
        // Super+K: Down.
        let (event, swallow) = m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::SUPER));
        assert_eq!(event, Some(HotkeyEvent::Down));
        assert!(!swallow);
        // Repeat while held: no new event.
        let (event, _) = m.handle(&repeat(Key::KeyK, Modifiers::SUPER));
        assert!(event.is_none());
        // Modifier drifts mid-hold (Super released first): still active, no Up.
        let (event, _) = m.handle(&ev(Key::SuperLeft, KeyEventKind::Release, Modifiers::NONE));
        assert!(event.is_none());
        // K released: Up.
        let (event, _) = m.handle(&ev(Key::KeyK, KeyEventKind::Release, Modifiers::NONE));
        assert!(matches!(event, Some(HotkeyEvent::Up { .. })));
    }

    #[test]
    fn exact_match_rejects_extra_modifiers() {
        let mut m = super_k(Suppression::Pass);
        let (event, _) = m.handle(&ev(
            Key::KeyK,
            KeyEventKind::Press,
            Modifiers::SUPER | Modifiers::SHIFT,
        ));
        assert!(event.is_none());
    }

    #[test]
    fn swallow_flag_only_for_swallow_registration() {
        let mut m = super_k(Suppression::Swallow);
        let (event, swallow) = m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::SUPER));
        assert_eq!(event, Some(HotkeyEvent::Down));
        assert!(swallow);
    }

    #[test]
    fn modifier_only_single_alt() {
        let mut m = Matcher::new(Hotkey::new(Modifiers::ALT, None), Suppression::Swallow);
        // Alt down (snapshot ALT): Down + swallow.
        let (event, swallow) = m.handle(&ev(Key::AltLeft, KeyEventKind::Press, Modifiers::ALT));
        assert_eq!(event, Some(HotkeyEvent::Down));
        assert!(swallow);
        // Typing while held: no transition, no swallow (unrelated key).
        let (event, swallow) = m.handle(&ev(Key::KeyA, KeyEventKind::Press, Modifiers::ALT));
        assert!(event.is_none() && !swallow);
        // Alt up: Up.
        let (event, _) = m.handle(&ev(Key::AltLeft, KeyEventKind::Release, Modifiers::NONE));
        assert!(matches!(event, Some(HotkeyEvent::Up { .. })));
    }

    #[test]
    fn modifier_only_survives_extra_modifier_mid_hold() {
        let mut m = Matcher::new(Hotkey::new(Modifiers::FN, None), Suppression::Pass);
        m.handle(&ev(Key::Function, KeyEventKind::Press, Modifiers::FN));
        // Shift pressed mid-hold: set still contains FN, no Up.
        let (event, _) = m.handle(&ev(
            Key::ShiftLeft,
            KeyEventKind::Press,
            Modifiers::FN | Modifiers::SHIFT,
        ));
        assert!(event.is_none());
        // Fn released (snapshot SHIFT): Up.
        let (event, _) = m.handle(&ev(Key::Function, KeyEventKind::Release, Modifiers::SHIFT));
        assert!(matches!(event, Some(HotkeyEvent::Up { .. })));
    }

    #[test]
    fn side_specific_modifier_trigger() {
        let mut m = Matcher::new(
            Hotkey::new(Modifiers::NONE, Some(Key::AltRight)),
            Suppression::Swallow,
        );
        // Left option: not this hotkey.
        let (event, swallow) = m.handle(&ev(Key::AltLeft, KeyEventKind::Press, Modifiers::ALT));
        assert!(event.is_none() && !swallow);
        // Right option: Down + swallow.
        let (event, swallow) = m.handle(&ev(Key::AltRight, KeyEventKind::Press, Modifiers::ALT));
        assert_eq!(event, Some(HotkeyEvent::Down));
        assert!(swallow);
        // Right released while left still held: Up.
        let (event, _) = m.handle(&ev(Key::AltRight, KeyEventKind::Release, Modifiers::ALT));
        assert!(matches!(event, Some(HotkeyEvent::Up { .. })));
    }

    #[test]
    fn held_for_is_reported() {
        let mut m = super_k(Suppression::Pass);
        m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::SUPER));
        std::thread::sleep(Duration::from_millis(10));
        let (event, _) = m.handle(&ev(Key::KeyK, KeyEventKind::Release, Modifiers::SUPER));
        match event {
            Some(HotkeyEvent::Up { held_for }) => {
                assert!(held_for >= Duration::from_millis(10));
            }
            other => panic!("expected Up, got {other:?}"),
        }
    }

    #[test]
    fn paused_emits_nothing_and_swallows_nothing() {
        let mut m = super_k(Suppression::Swallow);
        m.set_enabled(false);
        let (event, swallow) = m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::SUPER));
        assert!(event.is_none() && !swallow);
        // Resumed: matching resumes.
        m.set_enabled(true);
        let (event, swallow) = m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::SUPER));
        assert_eq!(event, Some(HotkeyEvent::Down));
        assert!(swallow);
    }

    #[test]
    fn pause_mid_hold_completes_on_release() {
        let mut m = super_k(Suppression::Pass);
        m.handle(&ev(Key::KeyK, KeyEventKind::Press, Modifiers::SUPER));
        m.set_enabled(false);
        // The in-flight activation completes on the real release.
        let (event, swallow) = m.handle(&ev(Key::KeyK, KeyEventKind::Release, Modifiers::NONE));
        assert!(matches!(event, Some(HotkeyEvent::Up { .. })));
        assert!(!swallow);
    }

    #[test]
    fn modifier_only_held_across_pause_cycle_does_not_refire() {
        let mut m = Matcher::new(Hotkey::new(Modifiers::ALT, None), Suppression::Pass);
        m.handle(&ev(Key::AltLeft, KeyEventKind::Press, Modifiers::ALT));
        m.set_enabled(false);
        m.set_enabled(true);
        // Still held from before the cycle: no second Down.
        let (event, _) = m.handle(&ev(Key::KeyA, KeyEventKind::Press, Modifiers::ALT));
        assert!(event.is_none());
        // One hold, one Up.
        let (event, _) = m.handle(&ev(Key::AltLeft, KeyEventKind::Release, Modifiers::NONE));
        assert!(matches!(event, Some(HotkeyEvent::Up { .. })));
    }

    #[test]
    fn guard_drop_closes_a_blocked_recv_at_once() {
        let _lock = crate::ENGINE_TEST_LOCK.lock().unwrap();
        let mut events = on(
            Hotkey::new(Modifiers::SUPER, Some(Key::KeyK)),
            Suppression::Pass,
        );
        let guard = events.guard().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = done_tx.send(events.recv());
        });
        drop(guard);
        // No keyboard event is needed — the close is receiver-side.
        match done_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Err(_)) => {}
            other => panic!("expected a closed channel, got {other:?}"),
        }
        // `done` only means the send ran; join so the worker's events (and
        // its engine reference) are gone before the lock is released.
        worker.join().unwrap();
    }

    #[test]
    fn closed_channel_stays_closed() {
        let _lock = crate::ENGINE_TEST_LOCK.lock().unwrap();
        let mut events = on(
            Hotkey::new(Modifiers::SUPER, Some(Key::KeyK)),
            Suppression::Pass,
        );
        let guard = events.guard().unwrap();
        drop(guard);
        assert!(events.recv().is_err());
        assert!(matches!(events.try_recv(), Err(TryRecvError::Closed)));
        assert!(matches!(
            events.recv_timeout(Duration::from_millis(10)),
            Err(RecvTimeoutError::Closed)
        ));
        assert_eq!(events.try_iter().count(), 0);
        assert_eq!(events.iter().count(), 0);
    }

    #[test]
    fn recv_timeout_times_out_while_open() {
        let _lock = crate::ENGINE_TEST_LOCK.lock().unwrap();
        let events = on(
            Hotkey::new(Modifiers::SUPER, Some(Key::KeyK)),
            Suppression::Pass,
        );
        let started = Instant::now();
        assert!(matches!(
            events.recv_timeout(Duration::from_millis(10)),
            Err(RecvTimeoutError::Timeout)
        ));
        assert!(started.elapsed() >= Duration::from_millis(10));
    }
}
