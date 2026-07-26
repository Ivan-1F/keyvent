//! keyvent — macOS global hotkeys that can never freeze your input.
//!
//! One callback in, everything else automatic: no Accessibility permission
//! means silence; granting starts the event stream; revoking stops it without
//! touching system input; re-granting resumes.
//!
//! # Example
//!
//! ```no_run
//! let guard = keyvent::start(|event| {
//!     // Return true to swallow the event system-wide.
//!     event.key == keyvent::Key::F5
//! });
//! // …later: dropping the last guard stops everything cleanly.
//! ```
//!
//! # The shared engine
//!
//! The first [`start`] call creates the process-wide engine — one event tap,
//! one supervisor thread. Later calls register additional callbacks on the
//! same tap; a press is swallowed if ANY callback returns true. Dropping a
//! [`Guard`] unregisters its callback; the engine shuts down when the last
//! guard is dropped.
//!
//! # Callback contract
//!
//! The callback runs on the event-tap thread, on the critical path of every
//! keystroke in the session. It **must return quickly**: no blocking, no
//! locking contended mutexes, no synchronous cross-thread calls. If it stalls
//! anyway, the OS watchdog disables the tap (keyboard input hesitates for up
//! to ~1s per event, then flows again), and keyvent's supervisor notices the
//! broken heartbeat and tears the tap down for good measure. The contract is
//! what keeps any of that from ever happening: do slow work on your own
//! thread — send the event over a channel and decide quickly.
//!
//! When several callbacks are registered, each runs on every event, so the
//! contract applies to each of them.
//!
//! The swallow decision (`true` return value) is only consulted for **presses**
//! (including key repeats, which are safe to swallow mid-cycle). Releases
//! always follow what the press did — a swallowed press swallows its release,
//! a passed press always passes its release — so the OS can never be left
//! with a phantom release or a stuck key.

mod event;
pub mod hotkey;
mod keycode;
mod supervisor;
mod tap;

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::AtomicU64;
use std::sync::{mpsc, Arc, Mutex, Weak};

pub use event::{Key, KeyEvent, KeyEventKind, Modifiers};
pub use hotkey::{Hotkey, HotkeyEvent, HotkeyId, HotkeyManager, Suppression};

use supervisor::StatusBroadcast;
use tap::BoxedCallback;

/// Lifecycle status of the event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Status {
    /// Events are flowing to the callback.
    Running = 0,
    /// No permission (or revoked): silent by design; nothing is hooked.
    Silent = 1,
    /// Automated recovery failed repeatedly; keyvent paused itself to protect
    /// system input. It resumes on its own when conditions change.
    Cooldown = 2,
}

impl Status {
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            0 => Status::Running,
            2 => Status::Cooldown,
            _ => Status::Silent,
        }
    }
}

/// The process-wide engine slot. Holds a weak reference: the engine is alive
/// exactly as long as at least one guard is.
static ENGINE: Mutex<Option<Weak<EngineInner>>> = Mutex::new(None);

/// A registration change sent to the dispatcher on the tap thread.
enum CallbackCommand {
    Add(u64, BoxedCallback),
    Remove(u64),
}

/// The tap thread's single callback: fans each event out to every registered
/// callback and ORs the swallow decisions. Commands are applied before the
/// event, so (un)registration takes effect on the very next event — with no
/// locking on the tap thread, per the callback contract.
struct Dispatcher {
    callbacks: Vec<(u64, BoxedCallback)>,
    cmd_rx: mpsc::Receiver<CallbackCommand>,
}

impl Dispatcher {
    fn handle(&mut self, event: KeyEvent) -> bool {
        while let Ok(command) = self.cmd_rx.try_recv() {
            match command {
                CallbackCommand::Add(id, callback) => self.callbacks.push((id, callback)),
                CallbackCommand::Remove(id) => self.callbacks.retain(|(cid, _)| *cid != id),
            }
        }
        let mut swallow = false;
        for (_, callback) in &mut self.callbacks {
            // Each callback is panic-isolated: one panicking callback must not
            // starve the others of this (or any later) event.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(event))) {
                Ok(true) => swallow = true,
                Ok(false) => {}
                Err(_) => log::error!("keyvent: user callback panicked; treated as pass-through"),
            }
        }
        swallow
    }
}

/// The process-wide engine: one tap thread + one supervisor, shared by all
/// guards. Created by the first `start`; dropped — and shut down — when the
/// last guard drops.
struct EngineInner {
    supervisor: Option<supervisor::Supervisor>,
    commands: mpsc::Sender<CallbackCommand>,
    broadcast: Arc<StatusBroadcast>,
    next_id: AtomicU64,
}

impl EngineInner {
    fn spawn() -> Arc<Self> {
        let shared = Arc::new(tap::TapShared::new());
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let mut dispatcher = Dispatcher {
            callbacks: Vec::new(),
            cmd_rx,
        };
        let tap = tap::TapThread::spawn(
            Box::new(move |event| dispatcher.handle(event)),
            shared.clone(),
        );
        let broadcast = Arc::new(StatusBroadcast::new(Status::Silent));
        let supervisor = supervisor::Supervisor::spawn(tap, shared, broadcast.clone());
        Arc::new(Self {
            supervisor: Some(supervisor),
            commands: cmd_tx,
            broadcast,
            next_id: AtomicU64::new(0),
        })
    }

    fn add_callback(self: &Arc<Self>, callback: BoxedCallback) -> Guard {
        let id = self.next_id.fetch_add(1, Relaxed);
        // If the tap thread is gone the callback simply never runs; the
        // guard's Remove later fails the same harmless way.
        let _ = self.commands.send(CallbackCommand::Add(id, callback));
        Guard {
            id,
            engine: self.clone(),
            status_rx: self.broadcast.subscribe(),
        }
    }
}

impl Drop for EngineInner {
    fn drop(&mut self) {
        // Runs on the thread dropping the last guard, never under the global
        // lock. Joining the threads can take seconds in the worst case; the
        // tap is force-invalidated first thing in shutdown, so a concurrently
        // re-spawned engine never meaningfully overlaps with this one.
        if let Some(supervisor) = self.supervisor.take() {
            supervisor.shutdown();
        }
    }
}

/// Handle to one callback registration in the shared keyvent engine.
/// Dropping it unregisters the callback; dropping the LAST guard stops
/// everything: the tap is invalidated, both threads exit, no state leaks.
pub struct Guard {
    id: u64,
    engine: Arc<EngineInner>,
    status_rx: mpsc::Receiver<Status>,
}

/// Start the global event stream with permission lifecycle fully managed.
///
/// The `callback` receives every keyboard event (see its contract above) and
/// returns whether to swallow it system-wide. Without Accessibility permission
/// this simply stays silent; the stream starts automatically once permission
/// is granted, and stops cleanly on revocation.
///
/// The first call creates the process-wide engine; later calls share it (see
/// "The shared engine" above). Dropping the returned [`Guard`] unregisters
/// this callback — a callback removed mid-press never sees the release.
pub fn start(callback: impl FnMut(KeyEvent) -> bool + Send + 'static) -> Guard {
    let mut slot = ENGINE.lock().unwrap();
    let engine = match slot.as_ref().and_then(Weak::upgrade) {
        Some(engine) => engine,
        None => {
            let engine = EngineInner::spawn();
            *slot = Some(Arc::downgrade(&engine));
            engine
        }
    };
    engine.add_callback(Box::new(callback))
}

impl Guard {
    /// Current lifecycle status.
    pub fn status(&self) -> Status {
        self.engine.broadcast.current()
    }

    /// Stream of status changes. Changes emitted after this guard's `start`
    /// are retained in the channel. Use `recv` to block for the next change
    /// or `try_recv` to poll.
    pub fn status_receiver(&self) -> &mpsc::Receiver<Status> {
        &self.status_rx
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Unregister only. If this was the last guard, dropping the Arc below
        // runs EngineInner::drop, which shuts the engine down.
        let _ = self.engine.commands.send(CallbackCommand::Remove(self.id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::SeqCst;

    fn dispatcher() -> Dispatcher {
        let (_tx, rx) = mpsc::channel();
        Dispatcher {
            callbacks: Vec::new(),
            cmd_rx: rx,
        }
    }

    fn event() -> KeyEvent {
        KeyEvent {
            key: Key::KeyA,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::NONE,
            is_repeat: false,
        }
    }

    #[test]
    fn dispatcher_ors_swallow_decisions() {
        let mut d = dispatcher();
        d.callbacks.push((0, Box::new(|_| false)));
        d.callbacks.push((1, Box::new(|_| true)));
        d.callbacks.push((2, Box::new(|_| false)));
        assert!(d.handle(event()));
    }

    #[test]
    fn dispatcher_applies_commands_before_events() {
        let (tx, rx) = mpsc::channel();
        let mut d = Dispatcher {
            callbacks: Vec::new(),
            cmd_rx: rx,
        };
        tx.send(CallbackCommand::Add(0, Box::new(|_| true))).unwrap();
        assert!(d.handle(event()));
        tx.send(CallbackCommand::Remove(0)).unwrap();
        assert!(!d.handle(event()));
    }

    #[test]
    fn dispatcher_isolates_callback_panics() {
        let mut d = dispatcher();
        let called = Arc::new(AtomicBool::new(false));
        let called2 = called.clone();
        d.callbacks.push((0, Box::new(|_| panic!("boom"))));
        d.callbacks.push((
            1,
            Box::new(move |_| {
                called2.store(true, SeqCst);
                false
            }),
        ));
        // The panic counts as pass-through, and later callbacks still ran.
        assert!(!d.handle(event()));
        assert!(called.load(SeqCst));
    }

    // NOTE: uses the process-global ENGINE; no other test may call `start`.
    #[test]
    fn guards_share_and_release_engine() {
        let g1 = start(|_| false);
        let g2 = start(|_| false);
        assert_eq!(
            ENGINE.lock().unwrap().as_ref().unwrap().strong_count(),
            2
        );
        assert_eq!(g1.status(), g2.status());
        drop(g1);
        assert!(
            ENGINE
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .upgrade()
                .is_some()
        );
        drop(g2);
        assert!(
            ENGINE
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .upgrade()
                .is_none()
        );
    }
}
