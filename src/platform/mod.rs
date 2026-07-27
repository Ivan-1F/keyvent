//! Platform backends. Each backend owns its event-source thread and, where
//! the platform demands it, a supervisor.

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(any(target_os = "windows", test))]
pub(crate) mod windows;

use std::thread::JoinHandle;
use std::time::Duration;

/// Raw platform keycode (macOS keycode, Windows scan code).
pub(crate) type Keycode = u16;

/// Join a thread, but detach instead of hanging forever if it is stuck
/// (e.g. the tap thread blocked inside a misbehaving user callback).
pub(crate) fn join_with_timeout(join: JoinHandle<()>, timeout: Duration, name: &str) {
    let start = std::time::Instant::now();
    while !join.is_finished() {
        if start.elapsed() > timeout {
            log::warn!(
                "keyvent: {name} thread did not exit in time; detached (tap already invalidated)"
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = join.join();
}
