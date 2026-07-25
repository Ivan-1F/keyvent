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
//! // …later: drop(guard) stops everything cleanly.
//! ```
//!
//! # Callback contract
//!
//! The callback runs on the event-tap thread, on the critical path of every
//! keystroke in the session. It **must return quickly**: no blocking, no
//! locking contended mutexes, no synchronous cross-thread calls. A callback
//! that stalls gets the tap killed by the OS watchdog (keyboard input stalls
//! for ~1s per event until then) and keyvent will tear the tap down — but the
//! contract is what keeps that from ever happening. Do slow work on your own
//! thread: send the event over a channel and decide quickly.

mod event;
mod keycode;
mod supervisor;
mod tap;

use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicU8};
use std::sync::{Arc, Mutex};

pub use event::{Key, KeyEvent, KeyEventKind, Modifiers};

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
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Status::Running,
            2 => Status::Cooldown,
            _ => Status::Silent,
        }
    }
}

/// Handle to a running keyvent session. Dropping it stops everything:
/// the tap is invalidated, both threads exit, no state leaks.
pub struct Guard {
    status: Arc<AtomicU8>,
    status_cb: Arc<Mutex<Option<Box<dyn Fn(Status) + Send>>>>,
    supervisor: Option<supervisor::Supervisor>,
}

/// Start the global event stream with permission lifecycle fully managed.
///
/// The `callback` receives every keyboard event (see its contract above) and
/// returns whether to swallow it system-wide. Without Accessibility permission
/// this simply stays silent; the stream starts automatically once permission
/// is granted, and stops cleanly on revocation.
pub fn start(callback: impl FnMut(KeyEvent) -> bool + Send + 'static) -> Guard {
    let shared = Arc::new(tap::TapShared::new());
    let tap = tap::TapThread::spawn(Box::new(callback), shared.clone());
    let status = Arc::new(AtomicU8::new(Status::Silent as u8));
    let status_cb: Arc<Mutex<Option<Box<dyn Fn(Status) + Send>>>> = Arc::new(Mutex::new(None));
    let supervisor = supervisor::Supervisor::spawn(
        tap,
        shared,
        status.clone(),
        status_cb.clone(),
    );
    Guard {
        status,
        status_cb,
        supervisor: Some(supervisor),
    }
}

impl Guard {
    /// Current lifecycle status.
    pub fn status(&self) -> Status {
        Status::from_u8(self.status.load(SeqCst))
    }

    /// Subscribe to status changes. The callback runs on the supervisor thread;
    /// keep it cheap (e.g. send to a channel).
    pub fn on_status_change(&self, f: impl Fn(Status) + Send + 'static) {
        *self.status_cb.lock().unwrap() = Some(Box::new(f));
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(supervisor) = self.supervisor.take() {
            supervisor.shutdown();
        }
    }
}
