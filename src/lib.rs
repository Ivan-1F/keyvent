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
//! locking contended mutexes, no synchronous cross-thread calls. If it stalls
//! anyway, the OS watchdog disables the tap (keyboard input hesitates for up
//! to ~1s per event, then flows again), and keyvent's supervisor notices the
//! broken heartbeat and tears the tap down for good measure. The contract is
//! what keeps any of that from ever happening: do slow work on your own
//! thread — send the event over a channel and decide quickly.
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

use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::AtomicU8;
use std::sync::{mpsc, Arc};

pub use event::{Key, KeyEvent, KeyEventKind, Modifiers};
pub use hotkey::{Hotkey, HotkeyEvent, HotkeyId, HotkeyManager, Suppression};

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
    status_rx: mpsc::Receiver<Status>,
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
    // Bounded: if nobody drains the channel, new statuses are dropped rather
    // than queueing forever — `Guard::status()` always has the current value.
    let (status_tx, status_rx) = mpsc::sync_channel(16);
    let supervisor = supervisor::Supervisor::spawn(tap, shared, status.clone(), status_tx);
    Guard {
        status,
        status_rx,
        supervisor: Some(supervisor),
    }
}

impl Guard {
    /// Current lifecycle status.
    pub fn status(&self) -> Status {
        Status::from_u8(self.status.load(SeqCst))
    }

    /// Stream of status changes. Events emitted between `start` and the first
    /// read are retained in the channel. Use `recv` to block for the next
    /// change or `try_recv` to poll.
    pub fn status_receiver(&self) -> &mpsc::Receiver<Status> {
        &self.status_rx
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(supervisor) = self.supervisor.take() {
            supervisor.shutdown();
        }
    }
}
