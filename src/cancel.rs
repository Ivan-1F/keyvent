//! A drop-triggered cancellation signal: one side holds the [`CancelToken`],
//! another polls the [`CancelWatcher`]. The token cannot send — nothing is
//! ever sent — so the only event the watcher can observe is the token's
//! death. Neither half ever blocks; the watcher is safe to poll on the tap
//! thread.

use crossbeam_channel::{self as channel, Receiver, Sender};

/// A cancellation pair: dropping the [`CancelToken`] flips
/// [`CancelWatcher::is_cancelled`] to `true`.
pub(crate) fn cancel_token() -> (CancelToken, CancelWatcher) {
    let (tx, rx) = channel::unbounded();
    (CancelToken { _signal: tx }, CancelWatcher(rx))
}

/// The signalling half of a cancellation signal, held by the handle whose
/// death means "cancelled". Dropping it is the only way the signal fires —
/// and it cannot fail to fire, since it takes no method call.
pub(crate) struct CancelToken {
    /// Kept only so that dropping it disconnects the watcher.
    _signal: Sender<()>,
}

/// The observing half of a cancellation signal.
pub(crate) struct CancelWatcher(Receiver<()>);

impl CancelWatcher {
    /// Whether the token is gone. Never blocks.
    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self.0.try_recv(), Err(channel::TryRecvError::Disconnected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_flips_only_when_token_drops() {
        let (token, watcher) = cancel_token();
        assert!(!watcher.is_cancelled());
        drop(token);
        assert!(watcher.is_cancelled());
    }
}
