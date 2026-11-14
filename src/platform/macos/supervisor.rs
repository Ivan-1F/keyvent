//! The supervisor thread: drives the [`Lifecycle`] state machine with real time and the real OS.
//!
//! The thread sleeps until something happens: a TCC notification, shutdown, or the machine's next deadline.
//! Silent waiting for a notification has no deadline at all, so it never wakes on its own.
//! Every wakeup is followed by [`Lifecycle::advance`], so a notification flood can never starve a due deadline.

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
use super::tcc::TccWatch;
use super::{InstallOutcome, TAP_EVENT_MASK, TapShared, TapThread};

/// Notifications beyond this are dropped while the supervisor is busy, which is harmless because they coalesce.
const INBOX_CAPACITY: usize = 64;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

enum Message {
    TccChanged,
    Shutdown,
}

/// Create and immediately invalidate a throwaway tap: asks WindowServer whether we may filter now.
/// It is never added to a run loop and is unrelated to the real tap.
/// A refusal leaks a Mach port pair inside the OS, so only Running calls this on a timer.
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
    shared: Arc<TapShared>,
    broadcast: Arc<StatusBroadcast>,
}

impl Platform for System {
    fn install(&mut self) -> InstallOutcome {
        self.tap.install()
    }

    fn probe(&mut self) -> bool {
        probe_tap()
    }

    fn tap_enabled(&self) -> bool {
        // An empty slot in Running means the tap is gone, which is as dead as disabled.
        // This also catches ByUserInput, which leaves the tap disabled.
        self.shared.tap_is_enabled().unwrap_or(false)
    }

    fn teardown(&mut self) {
        // force_invalidate works even if the tap thread is stuck in a user callback.
        // The queued Teardown then drops the tap thread's local state.
        self.shared.force_invalidate();
        self.tap.teardown();
    }

    fn publish(&mut self, status: Status) {
        self.broadcast.publish(status);
    }
}

pub(crate) struct Supervisor {
    inbox: channel::Sender<Message>,
    shared: Arc<TapShared>,
    worker: Worker,
}

impl Supervisor {
    /// Spawn the tap thread and the supervisor thread that manages it.
    pub(crate) fn spawn(callback: BoxedCallback, broadcast: Arc<StatusBroadcast>) -> Self {
        let (inbox, messages) = channel::bounded(INBOX_CAPACITY);
        let shared = Arc::new(TapShared::new());
        let tap = TapThread::spawn(callback, shared.clone());
        let system = System {
            tap,
            shared: shared.clone(),
            broadcast,
        };
        let tcc_inbox = inbox.clone();
        let worker = Worker::spawn("keyvent-supervisor", move || {
            supervise(system, messages, tcc_inbox)
        });
        Self {
            inbox,
            shared,
            worker,
        }
    }

    pub(crate) fn shutdown(self) {
        // Free system input immediately, even if the tap thread is stuck inside a user callback.
        self.shared.force_invalidate();
        // Blocks only while the inbox is full, which ends as soon as the supervisor takes one message.
        let _ = self.inbox.send_timeout(Message::Shutdown, SHUTDOWN_TIMEOUT);
        self.worker.join_timeout(SHUTDOWN_TIMEOUT);
    }
}

fn supervise(
    mut system: System,
    messages: channel::Receiver<Message>,
    tcc_inbox: channel::Sender<Message>,
) {
    // Subscribe before the first install, so a change right after startup is never missed.
    let _watch = TccWatch::accessibility(move || {
        let _ = tcc_inbox.try_send(Message::TccChanged);
    });
    let mut lifecycle = Lifecycle::new(Instant::now());

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
        lifecycle.advance(Instant::now(), &mut system);
    }

    system.tap.shutdown();
    log::debug!("keyvent: supervisor exited");
}
