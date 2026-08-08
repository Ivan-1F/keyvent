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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use crossbeam_channel as channel;
use crossbeam_channel::select_biased;

use crate::cancel::{CancelToken, CancelWatcher, cancel_token};
use crate::status::StatusBroadcast;
use crate::{Guard, Key, KeyEvent, KeyEventKind, Modifiers, Status};

use super::{Hotkey, MATCH_BITS, RecvError, RecvTimeoutError, TryRecvError, modifier_bit_of};

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
/// channel closes at once.
pub struct Recording {
    status: Arc<StatusBroadcast>,
    /// Dropping the guard lifts the recording pause synchronously.
    pause: Arc<RecordingPause>,
    /// Its death tells the tap-thread callback to cancel the session.
    _cancel: CancelToken,
}

impl Drop for Recording {
    fn drop(&mut self) {
        // Runs before `_cancel` drops, so the channel can only report
        // closed after the pause is already lifted.
        self.pause.release();
    }
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
    // The tap-thread closure watches its own clone; cancellation reaches
    // every watcher.
    let cancelled_on_tap = cancelled.clone();
    let (tx, rx) = channel::unbounded();
    let mut tx = Some(tx);
    let mut gesture = Gesture::new();
    // Shared by the closure, the stream and the Recording guard; the first
    // release wins.
    let pause = Arc::new(RecordingPause::new());
    let closure_pause = pause.clone();
    let events_pause = pause.clone();
    let mut finished = false;
    let registration = crate::start(move |event| {
        // The guard is gone: pass events through. Its drop has already
        // lifted the recording pause; dropping our sender stops queueing
        // for a dead listener.
        if cancelled_on_tap.is_cancelled() {
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
            // Lifting the hold lets every on guard resume matching.
            closure_pause.release();
        }
        step.swallow
    });
    let status = registration.status_broadcast();
    RecorderEvents {
        rx,
        cancelled,
        pause: events_pause,
        _registration: registration,
        pending_guard: Some(Recording {
            status,
            pause,
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
    rx: channel::Receiver<RecorderEvent>,
    /// Sees the guard's death; closing the stream is a receiver-side act.
    cancelled: CancelWatcher,
    /// Dropping the stream lifts the recording pause synchronously.
    pause: Arc<RecordingPause>,
    /// Keeps the registration alive; dropping it cancels the recording.
    _registration: Guard,
    pending_guard: Option<Recording>,
}

impl Drop for RecorderEvents {
    fn drop(&mut self) {
        // "Dropping events cancels the recording": lift the pause at once,
        // even if the Recording guard was taken out and is still alive.
        self.pause.release();
    }
}

impl RecorderEvents {
    /// Take this session's control guard (at most once).
    pub fn guard(&mut self) -> Option<Recording> {
        self.pending_guard.take()
    }

    /// Block until the next event, `Err` once the channel is closed.
    pub fn recv(&self) -> Result<RecorderEvent, RecvError> {
        // Cancellation wins over queued events: once the guard is gone the
        // stream is closed, whatever is still buffered.
        select_biased! {
            recv(self.cancelled.as_receiver()) -> _ => Err(RecvError),
            recv(&self.rx) -> msg => msg.map_err(|_| RecvError),
        }
    }

    /// The next event if one is pending.
    pub fn try_recv(&self) -> Result<RecorderEvent, TryRecvError> {
        if self.cancelled.is_cancelled() {
            return Err(TryRecvError::Closed);
        }
        self.rx.try_recv().map_err(TryRecvError::from_channel_error)
    }

    /// Block up to `timeout` for the next event.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<RecorderEvent, RecvTimeoutError> {
        select_biased! {
            recv(self.cancelled.as_receiver()) -> _ => Err(RecvTimeoutError::Closed),
            recv(&self.rx) -> msg => msg.map_err(|_| RecvTimeoutError::Closed),
            recv(channel::after(timeout)) -> _ => Err(RecvTimeoutError::Timeout),
        }
    }

    /// An iterator blocking on each event.
    pub fn iter(&self) -> impl Iterator<Item = RecorderEvent> + '_ {
        std::iter::from_fn(|| self.recv().ok())
    }

    /// An iterator over the pending events.
    pub fn try_iter(&self) -> impl Iterator<Item = RecorderEvent> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
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
/// `on` guard pauses matching. Shared by the tap-thread closure and the
/// `Recording` guard; [`release`](Self::release) is idempotent — the first
/// release wins — so dropping the guard lifts the pause at once instead of
/// at the next keyboard event.
struct RecordingPause {
    held: AtomicBool,
}

impl RecordingPause {
    fn new() -> Self {
        RECORDING.fetch_add(1, Ordering::Relaxed);
        RecordingPause {
            held: AtomicBool::new(true),
        }
    }

    /// Lift the pause; a no-op if already released.
    fn release(&self) {
        if self.held.swap(false, Ordering::Relaxed) {
            RECORDING.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for RecordingPause {
    fn drop(&mut self) {
        self.release();
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

    /// Serializes tests touching the process-wide RECORDING count.
    static RECORDING_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn pause_holds_the_count_for_its_lifetime() {
        let _lock = RECORDING_TEST_LOCK.lock().unwrap();
        RECORDING.store(0, Ordering::Relaxed);

        {
            let _pause = RecordingPause::new();
            assert_eq!(RECORDING.load(Ordering::Relaxed), 1);
        } // Drop releases the hold.
        assert_eq!(RECORDING.load(Ordering::Relaxed), 0);

        {
            let pause = RecordingPause::new();
            assert_eq!(RECORDING.load(Ordering::Relaxed), 1);
            pause.release(); // Released early, as at gesture end.
            pause.release(); // Idempotent: the second release is a no-op.
            assert_eq!(RECORDING.load(Ordering::Relaxed), 0);
            drop(pause); // Drop after release: no double decrement.
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

    #[test]
    fn recording_guard_drop_closes_the_channel_at_once() {
        let _engine_lock = crate::ENGINE_TEST_LOCK.lock().unwrap();
        let _lock = RECORDING_TEST_LOCK.lock().unwrap();
        RECORDING.store(0, Ordering::Relaxed);
        let mut events = record();
        let recording = events.guard().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = done_tx.send(events.recv());
        });
        drop(recording);
        // No keyboard event is needed — the close is receiver-side.
        match done_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Err(_)) => {}
            other => panic!("expected a closed channel, got {other:?}"),
        }
        // Cancelling lifts the recording pause synchronously, not at the
        // next keyboard event.
        assert_eq!(RECORDING.load(Ordering::Relaxed), 0);
        // `done` only means the send ran; join so the worker's events (and
        // its engine reference) are gone before the locks are released.
        worker.join().unwrap();
        // The worker held the last engine reference, so teardown has run:
        // the tap-thread closure is gone and its pause hold was released
        // exactly once — no double decrement.
        assert_eq!(RECORDING.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dropping_events_with_guard_alive_releases_the_pause() {
        let _engine_lock = crate::ENGINE_TEST_LOCK.lock().unwrap();
        let _lock = RECORDING_TEST_LOCK.lock().unwrap();
        RECORDING.store(0, Ordering::Relaxed);
        let mut events = record();
        let recording = events.guard().unwrap();
        assert_eq!(RECORDING.load(Ordering::Relaxed), 1);
        // Dropping the stream cancels the session: the pause lifts at once,
        // even though the Recording guard is still alive.
        drop(events);
        assert_eq!(RECORDING.load(Ordering::Relaxed), 0);
        // The guard's own release is now an idempotent no-op.
        drop(recording);
        assert_eq!(RECORDING.load(Ordering::Relaxed), 0);
    }
}
