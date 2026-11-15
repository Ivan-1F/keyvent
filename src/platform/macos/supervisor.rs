//! The supervisor thread: drives the [`Lifecycle`] state machine with real time and the real OS.
//! It sleeps until shutdown, a TCC notification, or the machine's next deadline.

use std::convert::Infallible;
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{self as channel, select_biased};
use objc2_core_foundation::CFMachPort;
use objc2_core_graphics::{
    CGEvent, CGEventTapCallBack, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType,
};

use crate::Status;
use crate::dispatcher::BoxedCallback;
use crate::platform::Worker;
use crate::status::StatusBroadcast;

use super::lifecycle::{Lifecycle, Platform};
use super::stream::{InstallOutcome, SharedTap, Stream, TAP_EVENT_MASK};
use super::tap::TapThread;
use super::tcc::TccWatch;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Ask WindowServer, with a throwaway tap, whether we may filter keyboard events now.
fn probe_tap() -> bool {
    unsafe extern "C-unwind" fn noop(
        _proxy: objc2_core_graphics::CGEventTapProxy,
        _event_type: CGEventType,
        event: std::ptr::NonNull<CGEvent>,
        _user_info: *mut std::ffi::c_void,
    ) -> *mut CGEvent {
        event.as_ptr()
    }
    let callback: CGEventTapCallBack = Some(noop);
    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            TAP_EVENT_MASK,
            callback,
            null_mut(),
        )
    };
    match tap {
        Some(tap) => {
            CFMachPort::invalidate(&tap);
            true
        }
        None => false,
    }
}

/// The real world, as [`Lifecycle`] sees it.
struct System {
    tap: TapThread,
    shared: Arc<SharedTap>,
    broadcast: Arc<StatusBroadcast>,
}

impl Platform for System {
    fn install(&mut self) -> InstallOutcome {
        // Waits as long as the tap thread is busy: until then there is nothing else worth doing.
        self.tap
            .call(Stream::install)
            .unwrap_or(InstallOutcome::Failed)
    }

    fn probe(&mut self) -> bool {
        probe_tap()
    }

    fn tap_enabled(&self) -> bool {
        self.shared.is_enabled()
    }

    fn teardown(&mut self) {
        // Invalidating here works even if the tap thread is stuck in a user callback.
        // The posted teardown then drops the tap thread's own references.
        self.shared.invalidate();
        self.tap.post(Stream::teardown);
    }

    fn publish(&mut self, status: Status) {
        self.broadcast.publish(status);
    }
}

pub(crate) struct Supervisor {
    /// Dropping it is the shutdown signal, which unlike a message can never be lost.
    stop: channel::Sender<Infallible>,
    shared: Arc<SharedTap>,
    worker: Worker,
}

impl Supervisor {
    /// Spawn the tap thread and the supervisor thread that manages it.
    /// Returns after the first check, so the status is already meaningful.
    pub(crate) fn spawn(callback: BoxedCallback, broadcast: Arc<StatusBroadcast>) -> Self {
        let (stop, stopped) = channel::bounded(0);
        // Notifications carry nothing, so one pending is as good as many.
        let (notify, notifications) = channel::bounded(1);
        let shared = Arc::new(SharedTap::new());
        let tap = TapThread::spawn(callback, shared.clone());
        let mut system = System {
            tap,
            shared: shared.clone(),
            broadcast,
        };
        // Subscribed before the first check, so a change right after it is never missed.
        let watch = TccWatch::accessibility(move || {
            let _ = notify.try_send(());
        });
        // A failed subscription drops the sender, and a disconnected channel would always be ready.
        let notifications = if watch.is_some() {
            notifications
        } else {
            channel::never()
        };
        let lifecycle = Lifecycle::start(Instant::now(), &mut system);
        let worker = Worker::spawn("keyvent-supervisor", move || {
            let _watch = watch;
            run(lifecycle, &mut system, &notifications, &stopped);
            system.tap.shutdown();
            log::debug!("keyvent: supervisor exited");
        });
        Self {
            stop,
            shared,
            worker,
        }
    }

    pub(crate) fn shutdown(self) {
        // Free system input immediately, even if the tap thread is stuck inside a user callback.
        // Closing also refuses an install that a stuck tap thread may still be about to run.
        self.shared.close();
        drop(self.stop);
        self.worker.join_timeout(SHUTDOWN_TIMEOUT);
    }
}

/// Drive `lifecycle` until `stopped` disconnects.
fn run(
    mut lifecycle: Lifecycle,
    platform: &mut impl Platform,
    notifications: &channel::Receiver<()>,
    stopped: &channel::Receiver<Infallible>,
) {
    loop {
        let deadline = lifecycle
            .deadline()
            .map_or_else(channel::never, channel::at);
        select_biased! {
            recv(stopped) -> _ => return,
            recv(notifications) -> _ => lifecycle.notified(Instant::now()),
            recv(deadline) -> _ => {}
        }
        // After every notification too, so a flood cannot starve a due deadline.
        lifecycle.advance(Instant::now(), platform);
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::super::lifecycle::SETTLE;
    use super::*;

    /// Refuses the startup check, then blocks every install until released, as a stuck user callback does.
    struct StuckInstall {
        installs: u32,
        entered: channel::Sender<()>,
        release: channel::Receiver<()>,
    }

    impl Platform for StuckInstall {
        fn install(&mut self) -> InstallOutcome {
            self.installs += 1;
            if self.installs == 1 {
                return InstallOutcome::Denied;
            }
            let _ = self.entered.send(());
            let _ = self.release.recv();
            // What an install returns once shutdown has closed the shared tap.
            InstallOutcome::Failed
        }

        fn probe(&mut self) -> bool {
            false
        }

        fn tap_enabled(&self) -> bool {
            false
        }

        fn teardown(&mut self) {}

        fn publish(&mut self, _status: Status) {}
    }

    #[test]
    fn shutdown_during_a_stuck_install_wins_over_a_pending_notification() {
        let (entered_tx, entered) = channel::bounded(1);
        let (release, release_rx) = channel::bounded(0);
        let (notify, notifications) = channel::bounded(1);
        let (stop, stopped) = channel::bounded::<Infallible>(0);
        let (done_tx, done) = channel::bounded(1);
        thread::spawn(move || {
            let mut platform = StuckInstall {
                installs: 0,
                entered: entered_tx,
                release: release_rx,
            };
            // Started one SETTLE ago, so the retry after the refused startup check is due at once.
            let lifecycle = Lifecycle::start(Instant::now() - SETTLE, &mut platform);
            run(lifecycle, &mut platform, &notifications, &stopped);
            let _ = done_tx.send(());
        });

        entered
            .recv_timeout(Duration::from_secs(1))
            .expect("the retry never ran");
        notify.try_send(()).unwrap();
        drop(stop);
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_millis(500))
            .expect("the loop kept running after shutdown");
    }
}
