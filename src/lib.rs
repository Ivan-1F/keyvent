//! Global keyboard event stream for macOS, with automatic Accessibility
//! permission lifecycle management.
//!
//! # Usage
//!
//! ```no_run
//! // This will return immediately.
//! let guard = keyvent::start(|event| {
//!     // Return true to suppress the event system-wide.
//!     event.key == keyvent::Key::F5
//! });
//! // Dropping the guard stops the callback, dropping the last guard stops everything cleanly.
//! ```
//!
//! # Platform Caveats
//!
//! ## macOS
//!
//! The process needs to be granted the Accessibility permission (System
//! Preferences > Privacy & Security > Accessibility)
//!
//! Without Accessibility permission, the stream simply stays silent; the stream
//! starts automatically once permission is granted, and stops cleanly on
//! revocation.

mod dispatcher;
mod event;
pub mod hotkey;
mod keycode;
mod supervisor;
mod tap;

use std::sync::{Arc, Mutex, Weak, mpsc};

pub use event::{Key, KeyEvent, KeyEventKind, Modifiers};
pub use hotkey::{Hotkey, HotkeyEvent, HotkeyId, HotkeyManager, Suppression};

use crate::dispatcher::{Dispatcher, DispatcherCommand};
use supervisor::StatusBroadcast;

/// Lifecycle status of the event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Status {
    /// Events are flowing to the callback.
    Running = 0,
    /// No Accessibility permission. Nothing is sent to the callback.
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
static ENGINE: Mutex<Option<Weak<Engine>>> = Mutex::new(None);

/// The process-wide engine: one tap thread + one supervisor, shared by all
/// guards. Created by the first `start`; dropped — and shut down — when the
/// last guard drops.
struct Engine {
    supervisor: Option<supervisor::Supervisor>,
    commands: mpsc::Sender<DispatcherCommand>,
    broadcast: Arc<StatusBroadcast>,
}

impl Engine {
    fn spawn() -> Arc<Self> {
        let shared = Arc::new(tap::TapShared::new());
        let (commands, mut dispatcher) = Dispatcher::new();
        let tap = tap::TapThread::spawn(
            Box::new(move |event| dispatcher.handle(event)),
            shared.clone(),
        );
        let broadcast = Arc::new(StatusBroadcast::new(Status::Silent));
        let supervisor = supervisor::Supervisor::spawn(tap, shared, broadcast.clone());
        Arc::new(Self {
            supervisor: Some(supervisor),
            commands,
            broadcast,
        })
    }

    fn add_callback(
        self: &Arc<Self>,
        callback: impl FnMut(KeyEvent) -> bool + Send + 'static,
    ) -> Guard {
        let registration = Arc::new(());
        // If the tap thread is gone the callback simply never runs; it is
        // pruned on the next event all the same.
        let _ = self.commands.send(DispatcherCommand::Add(
            Box::new(callback),
            Arc::downgrade(&registration),
        ));
        Guard {
            _registration: registration,
            engine: self.clone(),
            status_rx: self.broadcast.subscribe(),
        }
    }
}

impl Drop for Engine {
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

/// Handle to one callback registration.
///
/// Dropping it stops the callback, dropping the last guard stops everything
/// cleanly.
pub struct Guard {
    /// Keeps the registration alive; the dispatcher prunes the callback once
    /// this token is gone.
    _registration: Arc<()>,
    engine: Arc<Engine>,
    status_rx: mpsc::Receiver<Status>,
}

/// Start the global event stream.
///
/// ```no_run
/// // This will return immediately.
/// let guard = keyvent::start(|event| {
///     // Return true to suppress the event system-wide.
///     event.key == keyvent::Key::F5
/// });
/// // Dropping the guard stops the callback, dropping the last guard stops everything cleanly.
/// ```
///
/// The first call creates the process-wide engine, later calls share it.
/// Dropping the returned [`Guard`] unregisters this callback, dropping the last
/// guard stops everything cleanly.
///
/// ## Callback
///
/// The `callback` receives every keyboard event and returns whether to suppress
/// it system-wide.
///
/// The callback runs on the event-tap thread, on the critical path of every
/// keystroke. It **must return quickly**: no blocking, no locking contended
/// mutexes, no synchronous cross-thread calls.
///
/// Do slow work on your own thread: send the event over a channel and decide
/// quickly.
///
/// ### Suppressing the event
///
/// Return `true` from the callback to suppress this event and return `false` to
/// let it pass. This is only consulted for presses (including key repeats).
/// Releases always follow what the press did, so the OS can never be left with
/// a phantom release or a stuck key.
///
/// ## Platform Caveats
///
/// ### macOS
///
/// The process needs to be granted the Accessibility permission (System
/// Preferences > Privacy & Security > Accessibility)
/// Without Accessibility permission, the stream simply stays silent; the stream
/// starts automatically once permission is granted, and stops cleanly on
/// revocation.
pub fn start(callback: impl FnMut(KeyEvent) -> bool + Send + 'static) -> Guard {
    let mut slot = ENGINE.lock().unwrap();
    let engine = match slot.as_ref().and_then(Weak::upgrade) {
        Some(engine) => engine,
        None => {
            let engine = Engine::spawn();
            *slot = Some(Arc::downgrade(&engine));
            engine
        }
    };
    engine.add_callback(callback)
}

impl Guard {
    /// Current engine status.
    pub fn status(&self) -> Status {
        self.engine.broadcast.current()
    }

    /// Stream of status changes.
    pub fn status_receiver(&self) -> &mpsc::Receiver<Status> {
        &self.status_rx
    }
}

#[cfg(test)]
mod tests {
    use crate::ENGINE;

    #[test]
    fn guards_share_and_release_engine() {
        let g1 = crate::start(|_| false);
        let g2 = crate::start(|_| false);
        assert_eq!(ENGINE.lock().unwrap().as_ref().unwrap().strong_count(), 2);
        assert_eq!(g1.status(), g2.status());
        drop(g1);
        assert!(ENGINE.lock().unwrap().as_ref().unwrap().upgrade().is_some());
        drop(g2);
        assert!(ENGINE.lock().unwrap().as_ref().unwrap().upgrade().is_none());
    }
}
