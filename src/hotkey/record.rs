//! `RecorderEvents`: one hotkey recording session and its event stream.
//! Dropping it cancels the session; the stream also ends on its own when
//! the gesture resolves.
//!
//! Gesture semantics:
//! - The candidate is the modifier set plus the last non-modifier key
//!   pressed; `Candidate` events report it live as the gesture builds.
//! - The recording commits when every key is released. Modifier snapshots
//!   are taken at the trigger key's press; with no trigger key the gesture
//!   is modifier-only: a lone modifier key records side-specifically
//!   (`{NONE, AltRight}`), several record the peak modifier set
//!   (`{SUPER|SHIFT, None}`) — pressing both sides of a modifier is how the
//!   side-agnostic group (`{ALT, None}`) is recorded.
//! - Bare Esc cancels; every other key is recordable, including bare Delete.
//! - All keyboard input is swallowed while recording, and every `on` guard
//!   pauses matching; both end when the session ends.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use crate::cancel::{CancelToken, cancel_token};
use crate::status::StatusBroadcast;
use crate::{Guard, Key, KeyEvent, KeyEventKind, Modifiers, Status};

use super::{Hotkey, MATCH_BITS, modifier_bit_of};

/// A recording progress or terminal event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderEvent {
    /// The would-be hotkey changed mid-gesture; show it as a live preview.
    Candidate(Hotkey),
    /// The gesture resolved.
    Finished(RecordOutcome),
}

/// How a recording gesture resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    /// The gesture finished: every key was released; this is the hotkey.
    Recorded(Hotkey),
    /// Bare Esc was pressed; nothing was recorded.
    Cancelled,
}

/// Live recording sessions, process-wide. `on` guards pause matching while
/// this is non-zero.
pub(crate) static RECORDING: AtomicUsize = AtomicUsize::new(0);

/// A hotkey recording session's control guard, see
/// [`RecorderEvents::guard`]. Dropping it cancels the recording; the event
/// channel closes at the next keyboard event.
pub struct Recording {
    status: Arc<StatusBroadcast>,
    /// Its death tells the tap-thread callback to cancel the session.
    _cancel: CancelToken,
}

/// Record a hotkey from the user's next keystrokes, returning the event
/// stream.
///
/// ```no_run
/// let events = keyvent::record();
/// while let Ok(event) = events.recv() {
///     // Live candidates, then one `Finished` event — after which the
///     // channel closes and the loop ends.
/// }
/// // Dropping `events` cancels the recording.
/// ```
pub fn record() -> RecorderEvents {
    let (cancel, cancelled) = cancel_token();
    let (tx, rx) = mpsc::channel();
    let mut tx = Some(tx);
    let mut gesture = Gesture::new();
    let mut pause = Some(RecordingPause::new());
    let mut finished = false;
    let registration = crate::start(move |event| {
        // The guard is gone: cancel the session — release every on guard
        // and close the channel.
        if cancelled.is_cancelled() {
            drop(pause.take());
            tx = None;
            return false;
        }
        // A finished session passes events through.
        if finished {
            return false;
        }
        let step = gesture.handle(&event);
        if let Some(sender) = &tx {
            for e in &step.events {
                let _ = sender.send(*e);
            }
        }
        if step.finished {
            finished = true;
            // Closing the channel ends the receiver's loop after the
            // terminal event — a recording, unlike a binding, has an end.
            tx = None;
            // Dropping the hold lets every on guard resume matching.
            drop(pause.take());
        }
        step.swallow
    });
    let status = registration.status_broadcast();
    RecorderEvents {
        rx,
        _registration: registration,
        pending_guard: Some(Recording {
            status,
            _cancel: cancel,
        }),
    }
}

impl Recording {
    /// Permission lifecycle status of the underlying engine.
    pub fn status(&self) -> Status {
        self.status.current()
    }

    /// A new subscription to engine status changes.
    pub fn status_receiver(&self) -> mpsc::Receiver<Status> {
        self.status.subscribe()
    }
}

/// The event stream of a recording session. The stream ends after the
/// `Finished` event is yielded, or when the recording is cancelled.
/// Dropping it cancels the recording.
pub struct RecorderEvents {
    rx: mpsc::Receiver<RecorderEvent>,
    /// Keeps the registration alive; dropping it cancels the recording.
    _registration: Guard,
    pending_guard: Option<Recording>,
}

impl RecorderEvents {
    /// Take this session's control guard (at most once).
    pub fn guard(&mut self) -> Option<Recording> {
        self.pending_guard.take()
    }

    /// Block until the next event, `Err` once the channel is closed.
    pub fn recv(&self) -> Result<RecorderEvent, mpsc::RecvError> {
        self.rx.recv()
    }

    /// The next event if one is pending.
    pub fn try_recv(&self) -> Result<RecorderEvent, mpsc::TryRecvError> {
        self.rx.try_recv()
    }

    /// Block up to `timeout` for the next event.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<RecorderEvent, mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    /// An iterator blocking on each event.
    pub fn iter(&self) -> mpsc::Iter<'_, RecorderEvent> {
        self.rx.iter()
    }

    /// An iterator over the pending events.
    pub fn try_iter(&self) -> mpsc::TryIter<'_, RecorderEvent> {
        self.rx.try_iter()
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

/// A recording session's hold on the live-recording count: while held, every
/// `on` guard pauses matching; dropping it lets them resume.
struct RecordingPause;

impl RecordingPause {
    fn new() -> Self {
        RECORDING.fetch_add(1, Ordering::Relaxed);
        RecordingPause
    }
}

impl Drop for RecordingPause {
    fn drop(&mut self) {
        RECORDING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The one gesture a session records.
#[derive(Default)]
struct Gesture {
    /// Keys pressed so far and not yet released.
    held: HashSet<Key>,
    /// Every key pressed so far, released or not.
    pressed: HashSet<Key>,
    /// Last non-modifier key pressed, and the modifier snapshot at its press.
    trigger: Option<(Key, Modifiers)>,
    /// Union of all modifier snapshots seen so far.
    peak: Modifiers,
    /// Last emitted candidate; candidates are emitted only on change.
    candidate: Option<Hotkey>,
}

/// The outcome of feeding one event to the gesture.
struct Step {
    /// The recorder events to emit, ending with the terminal one if any.
    events: Vec<RecorderEvent>,
    /// Whether the raw event should be swallowed.
    swallow: bool,
    /// Whether the gesture resolved with this event.
    finished: bool,
}

impl Gesture {
    fn new() -> Self {
        Self::default()
    }

    fn handle(&mut self, event: &KeyEvent) -> Step {
        let mut step = Step {
            events: Vec::new(),
            swallow: true,
            finished: false,
        };
        let mods = event.modifiers & MATCH_BITS;
        match event.kind {
            KeyEventKind::Press if !event.is_repeat => {
                // Bare Esc cancels; every other key is recordable.
                if mods.is_empty() && event.key == Key::Escape {
                    step.events
                        .push(RecorderEvent::Finished(RecordOutcome::Cancelled));
                    step.finished = true;
                } else {
                    self.held.insert(event.key);
                    self.pressed.insert(event.key);
                    self.peak |= mods;
                    if modifier_bit_of(event.key).is_none() {
                        self.trigger = Some((event.key, mods));
                    }
                    let candidate = self.candidate_hotkey();
                    if self.candidate != Some(candidate) {
                        self.candidate = Some(candidate);
                        step.events.push(RecorderEvent::Candidate(candidate));
                    }
                }
            }
            KeyEventKind::Release => {
                self.held.remove(&event.key);
                if self.held.is_empty()
                    && mods.is_empty()
                    && (self.trigger.is_some() || !self.peak.is_empty())
                {
                    step.events
                        .push(RecorderEvent::Finished(RecordOutcome::Recorded(
                            self.candidate_hotkey(),
                        )));
                    step.finished = true;
                }
            }
            _ => {}
        }
        step
    }

    fn candidate_hotkey(&self) -> Hotkey {
        if let Some((key, mods)) = self.trigger {
            return Hotkey::new(mods, Some(key));
        }
        // Modifier-only so far. A lone modifier key records the exact
        // physical key; anything else records the peak set. The peak must
        // equal the key's own group — a mismatch means unseen presses from
        // before the gesture started (mid-hold recording).
        if self.pressed.len() == 1 {
            let key = *self.pressed.iter().next().unwrap();
            if modifier_bit_of(key) == Some(self.peak) {
                return Hotkey::new(Modifiers::NONE, Some(key));
            }
        }
        Hotkey::new(self.peak, None)
    }
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

    fn repeat(key: Key, mods: Modifiers) -> KeyEvent {
        KeyEvent {
            key,
            kind: KeyEventKind::Press,
            modifiers: mods,
            is_repeat: true,
        }
    }

    fn press(key: Key, mods: Modifiers) -> KeyEvent {
        ev(key, KeyEventKind::Press, mods)
    }

    fn release(key: Key, mods: Modifiers) -> KeyEvent {
        ev(key, KeyEventKind::Release, mods)
    }

    #[test]
    fn pause_holds_the_count_for_its_lifetime() {
        RECORDING.store(0, Ordering::Relaxed);

        {
            let _pause = RecordingPause::new();
            assert_eq!(RECORDING.load(Ordering::Relaxed), 1);
        } // Drop releases the hold.
        assert_eq!(RECORDING.load(Ordering::Relaxed), 0);

        {
            let mut pause = Some(RecordingPause::new());
            assert_eq!(RECORDING.load(Ordering::Relaxed), 1);
            drop(pause.take()); // Released early, as at gesture end.
            assert_eq!(RECORDING.load(Ordering::Relaxed), 0);
            drop(pause); // Nothing left to release.
        }
        assert_eq!(RECORDING.load(Ordering::Relaxed), 0);
    }

    fn recorded(hotkey: Hotkey) -> RecorderEvent {
        RecorderEvent::Finished(RecordOutcome::Recorded(hotkey))
    }

    #[test]
    fn bare_key_records_on_release() {
        let mut g = Gesture::new();
        let step = g.handle(&press(Key::KeyK, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::NONE,
                Some(Key::KeyK)
            ))]
        );
        assert!(step.swallow && !step.finished);
        let step = g.handle(&release(Key::KeyK, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::NONE, Some(Key::KeyK)))]
        );
        assert!(step.swallow && step.finished);
    }

    #[test]
    fn cmd_k_commits_on_last_release() {
        let mut g = Gesture::new();
        // A lone modifier previews as the exact key...
        let step = g.handle(&press(Key::SuperLeft, Modifiers::SUPER));
        assert_eq!(
            step.events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::NONE,
                Some(Key::SuperLeft)
            ))]
        );
        // ...and a struck key turns it into a plain modifier+trigger.
        let step = g.handle(&press(Key::KeyK, Modifiers::SUPER));
        assert_eq!(
            step.events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::SUPER,
                Some(Key::KeyK)
            ))]
        );
        // Trigger released but Super still held: no commit.
        let step = g.handle(&release(Key::KeyK, Modifiers::SUPER));
        assert!(step.events.is_empty() && !step.finished);
        let step = g.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::SUPER, Some(Key::KeyK)))]
        );
        assert!(step.finished);
    }

    #[test]
    fn last_trigger_wins_and_sticks() {
        let mut g = Gesture::new();
        g.handle(&press(Key::SuperLeft, Modifiers::SUPER));
        g.handle(&press(Key::KeyK, Modifiers::SUPER));
        // Still holding Super, strike J instead: candidate becomes Super+J.
        let step = g.handle(&press(Key::KeyJ, Modifiers::SUPER));
        assert_eq!(
            step.events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::SUPER,
                Some(Key::KeyJ)
            ))]
        );
        // Releasing a trigger does not degrade the candidate.
        let step = g.handle(&release(Key::KeyJ, Modifiers::SUPER));
        assert!(step.events.is_empty());
        g.handle(&release(Key::KeyK, Modifiers::SUPER));
        let step = g.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::SUPER, Some(Key::KeyJ)))]
        );
    }

    #[test]
    fn trigger_modifiers_frozen_at_its_press() {
        let mut g = Gesture::new();
        // K struck before Super: bare K, even though Super is held at the end.
        g.handle(&press(Key::KeyK, Modifiers::NONE));
        g.handle(&press(Key::SuperLeft, Modifiers::SUPER));
        g.handle(&release(Key::KeyK, Modifiers::SUPER));
        let step = g.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::NONE, Some(Key::KeyK)))]
        );
    }

    #[test]
    fn modifier_only_records_peak_set() {
        let mut g = Gesture::new();
        g.handle(&press(Key::SuperLeft, Modifiers::SUPER));
        let step = g.handle(&press(Key::ShiftLeft, Modifiers::SUPER | Modifiers::SHIFT));
        assert_eq!(
            step.events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::SUPER | Modifiers::SHIFT,
                None
            ))]
        );
        // Shift released early: the peak still includes it.
        let step = g.handle(&release(Key::ShiftLeft, Modifiers::SUPER));
        assert!(step.events.is_empty() && !step.finished);
        let step = g.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(
                Modifiers::SUPER | Modifiers::SHIFT,
                None
            ))]
        );
        assert!(step.finished);
    }

    #[test]
    fn lone_modifier_records_the_exact_key() {
        let mut g = Gesture::new();
        // The live preview already shows the exact key, not the group.
        let step = g.handle(&press(Key::AltRight, Modifiers::ALT));
        assert_eq!(
            step.events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::NONE,
                Some(Key::AltRight)
            ))]
        );
        let step = g.handle(&release(Key::AltRight, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::NONE, Some(Key::AltRight)))]
        );
    }

    #[test]
    fn both_option_sides_record_the_group() {
        let mut g = Gesture::new();
        g.handle(&press(Key::AltLeft, Modifiers::ALT));
        // Second side joins: the candidate flips to the side-agnostic group.
        let step = g.handle(&press(Key::AltRight, Modifiers::ALT));
        assert_eq!(
            step.events,
            vec![RecorderEvent::Candidate(Hotkey::new(Modifiers::ALT, None))]
        );
        let step = g.handle(&release(Key::AltLeft, Modifiers::ALT));
        assert!(step.events.is_empty());
        let step = g.handle(&release(Key::AltRight, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::ALT, None))]
        );
    }

    #[test]
    fn mid_hold_modifier_start_falls_back_to_peak() {
        // Recording started with Super already held; its press was never seen,
        // so the lone-key rule cannot apply and the peak set is the record.
        let mut g = Gesture::new();
        g.handle(&press(Key::ShiftLeft, Modifiers::SUPER | Modifiers::SHIFT));
        g.handle(&release(Key::ShiftLeft, Modifiers::SUPER));
        let step = g.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(
                Modifiers::SUPER | Modifiers::SHIFT,
                None
            ))]
        );
    }

    #[test]
    fn fn_alone_and_fn_k() {
        let mut g = Gesture::new();
        g.handle(&press(Key::Function, Modifiers::FN));
        let step = g.handle(&release(Key::Function, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::FN, None))]
        );

        let mut g = Gesture::new();
        g.handle(&press(Key::Function, Modifiers::FN));
        g.handle(&press(Key::KeyK, Modifiers::FN));
        g.handle(&release(Key::KeyK, Modifiers::FN));
        let step = g.handle(&release(Key::Function, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::FN, Some(Key::KeyK)))]
        );
    }

    #[test]
    fn caps_lock_records_as_trigger() {
        let mut g = Gesture::new();
        g.handle(&press(Key::CapsLock, Modifiers::NONE));
        let step = g.handle(&release(Key::CapsLock, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::NONE, Some(Key::CapsLock)))]
        );
    }

    #[test]
    fn bare_esc_cancels() {
        let mut g = Gesture::new();
        let step = g.handle(&press(Key::Escape, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![RecorderEvent::Finished(RecordOutcome::Cancelled)]
        );
        assert!(step.swallow && step.finished);
    }

    #[test]
    fn esc_with_modifiers_records() {
        let mut g = Gesture::new();
        g.handle(&press(Key::ControlLeft, Modifiers::CONTROL));
        g.handle(&press(Key::Escape, Modifiers::CONTROL));
        g.handle(&release(Key::Escape, Modifiers::CONTROL));
        let step = g.handle(&release(Key::ControlLeft, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::CONTROL, Some(Key::Escape)))]
        );
    }

    #[test]
    fn bare_delete_records() {
        let mut g = Gesture::new();
        // Bare Delete is an ordinary recordable key.
        let step = g.handle(&press(Key::Delete, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::NONE,
                Some(Key::Delete)
            ))]
        );
        assert!(step.swallow && !step.finished);
        let step = g.handle(&release(Key::Delete, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::NONE, Some(Key::Delete)))]
        );
        assert!(step.finished);
    }

    #[test]
    fn repeats_are_ignored() {
        let mut g = Gesture::new();
        g.handle(&press(Key::SuperLeft, Modifiers::SUPER));
        g.handle(&press(Key::KeyK, Modifiers::SUPER));
        // Holding K repeats it: no new candidate, still swallowed.
        let step = g.handle(&repeat(Key::KeyK, Modifiers::SUPER));
        assert!(step.events.is_empty() && step.swallow);
        g.handle(&release(Key::KeyK, Modifiers::SUPER));
        let step = g.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::SUPER, Some(Key::KeyK)))]
        );
    }

    #[test]
    fn stray_release_commits_nothing() {
        // Recording started while these keys were held; their presses were
        // never seen.
        let mut g = Gesture::new();
        let step = g.handle(&release(Key::KeyK, Modifiers::NONE));
        assert!(step.events.is_empty() && step.swallow && !step.finished);
        let step = g.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert!(step.events.is_empty() && !step.finished);
    }

    #[test]
    fn mid_hold_start_uses_snapshots() {
        let mut g = Gesture::new();
        // Recording started with Super already held; its press was never seen.
        let step = g.handle(&press(Key::KeyK, Modifiers::SUPER));
        assert_eq!(
            step.events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::SUPER,
                Some(Key::KeyK)
            ))]
        );
        // K released while Super still held (per the snapshot): no commit.
        let step = g.handle(&release(Key::KeyK, Modifiers::SUPER));
        assert!(step.events.is_empty());
        // Super released: commit, with the modifiers from K's press snapshot.
        let step = g.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            step.events,
            vec![recorded(Hotkey::new(Modifiers::SUPER, Some(Key::KeyK)))]
        );
    }
}
