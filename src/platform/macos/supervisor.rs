//! The supervisor thread: drives the [`Lifecycle`] state machine with real time and the real OS.
//! It sleeps until a TCC notification, shutdown, or the machine's next deadline.

use std::ptr::null_mut;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{self as channel, RecvTimeoutError};
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

/// Notifications beyond this are dropped while the supervisor is busy, which is harmless because they coalesce.
const INBOX_CAPACITY: usize = 64;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

enum Message {
    TccChanged,
    Shutdown,
}

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
    inbox: channel::Sender<Message>,
    shared: Arc<SharedTap>,
    worker: Worker,
}

impl Supervisor {
    /// Spawn the tap thread and the supervisor thread that manages it.
    /// Returns after the first check, so the status is already meaningful.
    pub(crate) fn spawn(callback: BoxedCallback, broadcast: Arc<StatusBroadcast>) -> Self {
        let (inbox, messages) = channel::bounded(INBOX_CAPACITY);
        let shared = Arc::new(SharedTap::new());
        let tap = TapThread::spawn(callback, shared.clone());
        let mut system = System {
            tap,
            shared: shared.clone(),
            broadcast,
        };
        // Subscribed before the first check, so a change right after it is never missed.
        let tcc_inbox = inbox.clone();
        let watch = TccWatch::accessibility(move || {
            let _ = tcc_inbox.try_send(Message::TccChanged);
        });
        let lifecycle = Lifecycle::start(Instant::now(), &mut system);
        let worker = Worker::spawn("keyvent-supervisor", move || {
            supervise(system, lifecycle, watch, messages)
        });
        Self {
            inbox,
            shared,
            worker,
        }
    }

    pub(crate) fn shutdown(self) {
        // Free system input immediately, even if the tap thread is stuck inside a user callback.
        // Closing also refuses an install that a stuck tap thread may still be about to run.
        self.shared.close();
        // Blocks only while the inbox is full, which ends as soon as the supervisor takes one message.
        let _ = self.inbox.send_timeout(Message::Shutdown, SHUTDOWN_TIMEOUT);
        self.worker.join_timeout(SHUTDOWN_TIMEOUT);
    }
}

fn supervise(
    mut system: System,
    mut lifecycle: Lifecycle,
    _watch: Option<TccWatch>,
    messages: channel::Receiver<Message>,
) {
    loop {
        let received = match lifecycle.deadline() {
            Some(deadline) => messages.recv_deadline(deadline),
            None => messages.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(Message::TccChanged) => lifecycle.notified(Instant::now()),
            Err(RecvTimeoutError::Timeout) => {}
            Ok(Message::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
        }
        // After every message too, so a notification flood cannot starve a due deadline.
        lifecycle.advance(Instant::now(), &mut system);
    }

    system.tap.shutdown();
    log::debug!("keyvent: supervisor exited");
}
