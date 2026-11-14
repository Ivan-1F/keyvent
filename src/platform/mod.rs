//! Platform backends.
//! Each backend owns its event-source thread, plus a supervisor where the platform needs one.

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(any(target_os = "windows", test))]
pub(crate) mod windows;

use std::convert::Infallible;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{self as channel, RecvTimeoutError};

/// Raw platform keycode (macOS keycode, Windows scan code).
pub(crate) type Keycode = u16;

/// A named thread whose exit can be awaited with a timeout, without polling.
pub(crate) struct Worker {
    join: JoinHandle<()>,
    /// Disconnects once the thread body returns or unwinds; nothing is ever sent on it.
    /// Crossbeam rather than std, because a Worker must be Sync to live in the shared engine.
    exited: channel::Receiver<Infallible>,
}

impl Worker {
    pub(crate) fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> Self {
        let (exit_signal, exited) = channel::bounded::<Infallible>(0);
        let join = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                // Dropped when the body returns or unwinds, which disconnects `exited`.
                let _exit_signal = exit_signal;
                body();
            })
            .unwrap_or_else(|err| panic!("spawn {name} thread: {err}"));
        Self { join, exited }
    }

    /// Join the thread, but detach it instead of hanging if it does not exit in time.
    /// A thread can get stuck inside a misbehaving user callback; callers make that state inert first.
    pub(crate) fn join_timeout(self, timeout: Duration) {
        match self.exited.recv_timeout(timeout) {
            Ok(never) => match never {},
            Err(RecvTimeoutError::Disconnected) => {
                let _ = self.join.join();
            }
            Err(RecvTimeoutError::Timeout) => {
                let name = self.join.thread().name().unwrap_or("keyvent worker");
                log::warn!("keyvent: {name} thread did not exit in time; detached");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::Worker;

    #[test]
    fn join_returns_as_soon_as_the_thread_exits() {
        let worker = Worker::spawn("keyvent-test-quick", || {});
        let start = Instant::now();
        worker.join_timeout(Duration::from_secs(5));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn join_detaches_a_stuck_thread() {
        let (release, wait) = mpsc::channel::<()>();
        let worker = Worker::spawn("keyvent-test-stuck", move || {
            let _ = wait.recv();
        });
        let start = Instant::now();
        worker.join_timeout(Duration::from_millis(50));
        assert!(start.elapsed() >= Duration::from_millis(50));
        drop(release);
    }

    #[test]
    fn join_survives_a_panicking_thread() {
        let worker = Worker::spawn("keyvent-test-panic", || panic!("expected test panic"));
        worker.join_timeout(Duration::from_secs(5));
    }
}
