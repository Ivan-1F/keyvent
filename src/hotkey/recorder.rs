//! `HotkeyRecorder`: record a new hotkey from the user's keystrokes — the
//! inverse of `HotkeyManager` matching. The gesture state machine (`Session`)
//! is pure logic, fully unit-testable; consumers receive `RecorderEvent`s
//! over a channel, and no user code runs on the tap thread.
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
//! - Bare Esc cancels and bare Delete clears; with modifiers held, both are
//!   ordinary recordable keys.
//! - All keyboard input is swallowed while recording.

use std::collections::HashSet;
use std::sync::mpsc;

use crate::{Guard, Key, KeyEvent, KeyEventKind, Modifiers, Status};

use super::{Hotkey, MATCH_BITS, modifier_bit_of};

/// A recording progress or terminal event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderEvent {
    /// The would-be hotkey changed mid-gesture; show it as a live preview.
    Candidate(Hotkey),
    /// The gesture finished: every key was released.
    Recorded(Hotkey),
    /// Bare Esc was pressed; nothing was recorded.
    Cancelled,
    /// Bare Delete was pressed; clear the existing binding.
    Cleared,
}

/// A recording-mode change sent to the session on the tap thread.
enum Command {
    Start,
    Cancel,
}

/// Records one hotkey from the user's next keystrokes.
///
/// Recorders share the process-wide keyvent engine (see [`crate::start`]).
pub struct HotkeyRecorder {
    commands: mpsc::Sender<Command>,
    rx: mpsc::Receiver<RecorderEvent>,
    engine: Guard,
}

impl HotkeyRecorder {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let mut session = Session::new();
        let engine = crate::start(move |event| {
            // Pending commands are applied before the event, so they take
            // effect immediately — with no locking on the tap thread.
            while let Ok(command) = cmd_rx.try_recv() {
                match command {
                    Command::Start => session.start(),
                    Command::Cancel => session.cancel(),
                }
            }
            let (events, swallow) = session.handle(&event);
            for e in events {
                let _ = tx.send(e);
            }
            swallow
        });
        Self {
            commands: cmd_tx,
            rx,
            engine,
        }
    }

    /// Enter recording mode: all keyboard input is swallowed and the gesture
    /// is reported over the channel. Calling again restarts the gesture.
    pub fn start_recording(&self) {
        let _ = self.commands.send(Command::Start);
    }

    /// Leave recording mode; nothing is recorded.
    pub fn cancel(&self) {
        let _ = self.commands.send(Command::Cancel);
    }

    /// Channel of recording events. Use `recv` to block or `try_recv` to poll.
    pub fn receiver(&self) -> &mpsc::Receiver<RecorderEvent> {
        &self.rx
    }

    /// Permission lifecycle status of the underlying engine.
    pub fn status(&self) -> Status {
        self.engine.status()
    }

    /// Stream of engine status changes.
    pub fn status_receiver(&self) -> &mpsc::Receiver<Status> {
        self.engine.status_receiver()
    }
}

impl Default for HotkeyRecorder {
    fn default() -> Self {
        Self::new()
    }
}

/// The gesture state machine: pure logic, no FFI, fully unit-testable.
pub(crate) struct Session {
    gesture: Option<Gesture>,
}

/// One in-progress recording gesture.
#[derive(Default)]
struct Gesture {
    /// Keys pressed since the gesture started and not yet released.
    held: HashSet<Key>,
    /// Every key pressed since the gesture started, released or not.
    pressed: HashSet<Key>,
    /// Last non-modifier key pressed, and the modifier snapshot at its press.
    trigger: Option<(Key, Modifiers)>,
    /// Union of all modifier snapshots seen during the gesture.
    peak: Modifiers,
    /// Last emitted candidate; candidates are emitted only on change.
    candidate: Option<Hotkey>,
}

impl Gesture {
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

impl Session {
    pub(crate) fn new() -> Self {
        Self { gesture: None }
    }

    /// Start a fresh gesture (also restarts an in-progress one).
    pub(crate) fn start(&mut self) {
        self.gesture = Some(Gesture::default());
    }

    pub(crate) fn cancel(&mut self) {
        self.gesture = None;
    }

    /// Process one event. Returns the recorder events to emit and whether the
    /// raw event should be swallowed.
    pub(crate) fn handle(&mut self, event: &KeyEvent) -> (Vec<RecorderEvent>, bool) {
        let Some(mut g) = self.gesture.take() else {
            return (Vec::new(), false);
        };
        let mut out = Vec::new();
        let mut done = false;
        let mods = event.modifiers & MATCH_BITS;
        match event.kind {
            KeyEventKind::Press if !event.is_repeat => {
                if mods.is_empty() && event.key == Key::Escape {
                    out.push(RecorderEvent::Cancelled);
                    done = true;
                } else if mods.is_empty() && event.key == Key::Delete {
                    out.push(RecorderEvent::Cleared);
                    done = true;
                } else {
                    g.held.insert(event.key);
                    g.pressed.insert(event.key);
                    g.peak |= mods;
                    if modifier_bit_of(event.key).is_none() {
                        g.trigger = Some((event.key, mods));
                    }
                    let candidate = g.candidate_hotkey();
                    if g.candidate != Some(candidate) {
                        g.candidate = Some(candidate);
                        out.push(RecorderEvent::Candidate(candidate));
                    }
                }
            }
            KeyEventKind::Release => {
                g.held.remove(&event.key);
                if g.held.is_empty()
                    && mods.is_empty()
                    && (g.trigger.is_some() || !g.peak.is_empty())
                {
                    out.push(RecorderEvent::Recorded(g.candidate_hotkey()));
                    done = true;
                }
            }
            _ => {}
        }
        self.gesture = if done { None } else { Some(g) };
        (out, true)
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

    fn recording() -> Session {
        let mut s = Session::new();
        s.start();
        s
    }

    #[test]
    fn idle_passes_events_through() {
        let mut s = Session::new();
        let (events, swallow) = s.handle(&press(Key::KeyK, Modifiers::NONE));
        assert!(events.is_empty() && !swallow);
    }

    #[test]
    fn bare_key_records_on_release() {
        let mut s = recording();
        let (events, swallow) = s.handle(&press(Key::KeyK, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::NONE,
                Some(Key::KeyK)
            ))]
        );
        assert!(swallow);
        let (events, swallow) = s.handle(&release(Key::KeyK, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::NONE,
                Some(Key::KeyK)
            ))]
        );
        assert!(swallow);
        // Committed: back to idle, events pass through.
        let (events, swallow) = s.handle(&press(Key::KeyJ, Modifiers::NONE));
        assert!(events.is_empty() && !swallow);
    }

    #[test]
    fn cmd_k_commits_on_last_release() {
        let mut s = recording();
        // A lone modifier previews as the exact key...
        let (events, _) = s.handle(&press(Key::SuperLeft, Modifiers::SUPER));
        assert_eq!(
            events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::NONE,
                Some(Key::SuperLeft)
            ))]
        );
        // ...and a struck key turns it into a plain modifier+trigger.
        let (events, _) = s.handle(&press(Key::KeyK, Modifiers::SUPER));
        assert_eq!(
            events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::SUPER,
                Some(Key::KeyK)
            ))]
        );
        // Trigger released but Super still held: no commit.
        let (events, _) = s.handle(&release(Key::KeyK, Modifiers::SUPER));
        assert!(events.is_empty());
        let (events, _) = s.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::SUPER,
                Some(Key::KeyK)
            ))]
        );
    }

    #[test]
    fn last_trigger_wins_and_sticks() {
        let mut s = recording();
        s.handle(&press(Key::SuperLeft, Modifiers::SUPER));
        s.handle(&press(Key::KeyK, Modifiers::SUPER));
        // Still holding Super, strike J instead: candidate becomes Super+J.
        let (events, _) = s.handle(&press(Key::KeyJ, Modifiers::SUPER));
        assert_eq!(
            events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::SUPER,
                Some(Key::KeyJ)
            ))]
        );
        // Releasing a trigger does not degrade the candidate.
        let (events, _) = s.handle(&release(Key::KeyJ, Modifiers::SUPER));
        assert!(events.is_empty());
        s.handle(&release(Key::KeyK, Modifiers::SUPER));
        let (events, _) = s.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::SUPER,
                Some(Key::KeyJ)
            ))]
        );
    }

    #[test]
    fn trigger_modifiers_frozen_at_its_press() {
        let mut s = recording();
        // K struck before Super: bare K, even though Super is held at the end.
        s.handle(&press(Key::KeyK, Modifiers::NONE));
        s.handle(&press(Key::SuperLeft, Modifiers::SUPER));
        s.handle(&release(Key::KeyK, Modifiers::SUPER));
        let (events, _) = s.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::NONE,
                Some(Key::KeyK)
            ))]
        );
    }

    #[test]
    fn modifier_only_records_peak_set() {
        let mut s = recording();
        s.handle(&press(Key::SuperLeft, Modifiers::SUPER));
        let (events, _) = s.handle(&press(Key::ShiftLeft, Modifiers::SUPER | Modifiers::SHIFT));
        assert_eq!(
            events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::SUPER | Modifiers::SHIFT,
                None
            ))]
        );
        // Shift released early: the peak still includes it.
        let (events, _) = s.handle(&release(Key::ShiftLeft, Modifiers::SUPER));
        assert!(events.is_empty());
        let (events, _) = s.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::SUPER | Modifiers::SHIFT,
                None
            ))]
        );
    }

    #[test]
    fn lone_modifier_records_the_exact_key() {
        let mut s = recording();
        // The live preview already shows the exact key, not the group.
        let (events, _) = s.handle(&press(Key::AltRight, Modifiers::ALT));
        assert_eq!(
            events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::NONE,
                Some(Key::AltRight)
            ))]
        );
        let (events, _) = s.handle(&release(Key::AltRight, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::NONE,
                Some(Key::AltRight)
            ))]
        );
    }

    #[test]
    fn both_option_sides_record_the_group() {
        let mut s = recording();
        s.handle(&press(Key::AltLeft, Modifiers::ALT));
        // Second side joins: the candidate flips to the side-agnostic group.
        let (events, _) = s.handle(&press(Key::AltRight, Modifiers::ALT));
        assert_eq!(
            events,
            vec![RecorderEvent::Candidate(Hotkey::new(Modifiers::ALT, None))]
        );
        let (events, _) = s.handle(&release(Key::AltLeft, Modifiers::ALT));
        assert!(events.is_empty());
        let (events, _) = s.handle(&release(Key::AltRight, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(Modifiers::ALT, None))]
        );
    }

    #[test]
    fn mid_hold_modifier_start_falls_back_to_peak() {
        // Recording started with Super already held; its press was never seen,
        // so the lone-key rule cannot apply and the peak set is the record.
        let mut s = recording();
        s.handle(&press(Key::ShiftLeft, Modifiers::SUPER | Modifiers::SHIFT));
        s.handle(&release(Key::ShiftLeft, Modifiers::SUPER));
        let (events, _) = s.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::SUPER | Modifiers::SHIFT,
                None
            ))]
        );
    }

    #[test]
    fn fn_alone_and_fn_k() {
        let mut s = recording();
        s.handle(&press(Key::Function, Modifiers::FN));
        let (events, _) = s.handle(&release(Key::Function, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(Modifiers::FN, None))]
        );

        let mut s = recording();
        s.handle(&press(Key::Function, Modifiers::FN));
        s.handle(&press(Key::KeyK, Modifiers::FN));
        s.handle(&release(Key::KeyK, Modifiers::FN));
        let (events, _) = s.handle(&release(Key::Function, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::FN,
                Some(Key::KeyK)
            ))]
        );
    }

    #[test]
    fn caps_lock_records_as_trigger() {
        let mut s = recording();
        // The OS reports the CAPS_LOCK toggle bit in the snapshot; it is not
        // a held modifier and must not leak into the hotkey.
        s.handle(&press(Key::CapsLock, Modifiers::CAPS_LOCK));
        let (events, _) = s.handle(&release(Key::CapsLock, Modifiers::CAPS_LOCK));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::NONE,
                Some(Key::CapsLock)
            ))]
        );
    }

    #[test]
    fn bare_esc_cancels() {
        let mut s = recording();
        let (events, swallow) = s.handle(&press(Key::Escape, Modifiers::NONE));
        assert_eq!(events, vec![RecorderEvent::Cancelled]);
        assert!(swallow);
        // Cancelled: back to idle.
        let (events, swallow) = s.handle(&release(Key::Escape, Modifiers::NONE));
        assert!(events.is_empty() && !swallow);
    }

    #[test]
    fn esc_with_modifiers_records() {
        let mut s = recording();
        s.handle(&press(Key::ControlLeft, Modifiers::CONTROL));
        s.handle(&press(Key::Escape, Modifiers::CONTROL));
        s.handle(&release(Key::Escape, Modifiers::CONTROL));
        let (events, _) = s.handle(&release(Key::ControlLeft, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::CONTROL,
                Some(Key::Escape)
            ))]
        );
    }

    #[test]
    fn bare_delete_clears() {
        let mut s = recording();
        let (events, swallow) = s.handle(&press(Key::Delete, Modifiers::NONE));
        assert_eq!(events, vec![RecorderEvent::Cleared]);
        assert!(swallow);
        let (events, swallow) = s.handle(&release(Key::Delete, Modifiers::NONE));
        assert!(events.is_empty() && !swallow);
    }

    #[test]
    fn repeats_are_ignored() {
        let mut s = recording();
        s.handle(&press(Key::SuperLeft, Modifiers::SUPER));
        s.handle(&press(Key::KeyK, Modifiers::SUPER));
        // Holding K repeats it: no new candidate, still swallowed.
        let (events, swallow) = s.handle(&repeat(Key::KeyK, Modifiers::SUPER));
        assert!(events.is_empty() && swallow);
        s.handle(&release(Key::KeyK, Modifiers::SUPER));
        let (events, _) = s.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::SUPER,
                Some(Key::KeyK)
            ))]
        );
    }

    #[test]
    fn stray_release_commits_nothing() {
        let mut s = recording();
        // Recording started while these keys were held; their presses were
        // never seen.
        let (events, swallow) = s.handle(&release(Key::KeyK, Modifiers::NONE));
        assert!(events.is_empty() && swallow);
        let (events, _) = s.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert!(events.is_empty());
    }

    #[test]
    fn mid_hold_start_uses_snapshots() {
        let mut s = recording();
        // Recording started with Super already held; its press was never seen.
        let (events, _) = s.handle(&press(Key::KeyK, Modifiers::SUPER));
        assert_eq!(
            events,
            vec![RecorderEvent::Candidate(Hotkey::new(
                Modifiers::SUPER,
                Some(Key::KeyK)
            ))]
        );
        // K released while Super still held (per the snapshot): no commit.
        let (events, _) = s.handle(&release(Key::KeyK, Modifiers::SUPER));
        assert!(events.is_empty());
        // Super released: commit, with the modifiers from K's press snapshot.
        let (events, _) = s.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::SUPER,
                Some(Key::KeyK)
            ))]
        );
    }

    #[test]
    fn restart_resets_gesture() {
        let mut s = recording();
        s.handle(&press(Key::KeyK, Modifiers::NONE));
        s.start();
        // K's release belongs to the abandoned gesture: no commit.
        let (events, _) = s.handle(&release(Key::KeyK, Modifiers::NONE));
        assert!(events.is_empty());
        s.handle(&press(Key::KeyJ, Modifiers::NONE));
        let (events, _) = s.handle(&release(Key::KeyJ, Modifiers::NONE));
        assert_eq!(
            events,
            vec![RecorderEvent::Recorded(Hotkey::new(
                Modifiers::NONE,
                Some(Key::KeyJ)
            ))]
        );
    }

    #[test]
    fn cancel_stops_recording() {
        let mut s = recording();
        s.handle(&press(Key::SuperLeft, Modifiers::SUPER));
        s.cancel();
        let (events, swallow) = s.handle(&release(Key::SuperLeft, Modifiers::NONE));
        assert!(events.is_empty() && !swallow);
    }
}
