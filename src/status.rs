//! Lifecycle status fan-out.

use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::{Mutex, mpsc};

use crate::Status;

/// Fan-out of lifecycle status changes: a current-value cell plus a list of
/// bounded subscriber channels. The platform layer is the only publisher.
pub(crate) struct StatusBroadcast {
    current: AtomicU8,
    subscribers: Mutex<Vec<mpsc::SyncSender<Status>>>,
}

impl StatusBroadcast {
    pub(crate) fn new(initial: Status) -> Self {
        Self {
            current: AtomicU8::new(initial as u8),
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Current lifecycle status.
    pub(crate) fn current(&self) -> Status {
        Status::from_u8(self.current.load(SeqCst))
    }

    /// A new subscriber channel. Bounded: if nobody drains it, new statuses
    /// are dropped rather than queueing forever — `current()` always has the
    /// latest value.
    pub(crate) fn subscribe(&self) -> mpsc::Receiver<Status> {
        let (tx, rx) = mpsc::sync_channel(16);
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Publish a status change: update the current-value cell and queue the
    /// change for all subscribers. No-op if the status is unchanged. A full
    /// channel drops the change silently; subscribers whose receiver is gone
    /// (guard dropped) are pruned lazily.
    pub(crate) fn publish(&self, next: Status) {
        let prev = self.current.swap(next as u8, SeqCst);
        if prev == next as u8 {
            return;
        }
        log::info!("keyvent: status {:?} -> {next:?}", Status::from_u8(prev));
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|tx| match tx.try_send(next) {
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => true,
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        });
    }
}
