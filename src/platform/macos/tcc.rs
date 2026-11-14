//! Accessibility permission change notifications from TCC.
//!
//! tccd posts a Darwin notification whenever any app's Accessibility record changes.
//! It fires on grant, revoke, entry removal, and entry insertion, for every app system-wide.
//! It carries no payload, so it only means "re-check now", never "you were granted".
//! The name is undocumented, and any process can post it.
//! See docs/macos-permission.md, section 4.5.

use std::ffi::{CStr, c_char, c_int};

use block2::{Block, RcBlock};
use dispatch2::{DispatchQueue, DispatchRetained};

/// Posted by tccd when any app's Accessibility record changes.
const ACCESSIBILITY_CHANGED: &CStr =
    c"com.apple.TCC.kTCCServiceAccessibility.authorization.changed";

/// NOTIFY_STATUS_OK from notify.h.
const NOTIFY_STATUS_OK: u32 = 0;

// libnotify is part of libSystem, which every macOS binary links.
unsafe extern "C" {
    fn notify_register_dispatch(
        name: *const c_char,
        out_token: *mut c_int,
        queue: &DispatchQueue,
        handler: &Block<dyn Fn(c_int)>,
    ) -> u32;
    fn notify_cancel(token: c_int) -> u32;
}

/// A subscription to a Darwin notification name.
/// The handler runs on a private serial dispatch queue, so no thread of ours has to poll.
/// Dropping the watch cancels the subscription; a delivery already queued may still run once.
pub(super) struct TccWatch {
    token: c_int,
    /// libnotify retains the queue too; holding it here keeps that detail out of our correctness.
    _queue: DispatchRetained<DispatchQueue>,
}

impl TccWatch {
    /// Subscribe to Accessibility changes; None (already logged) if registration failed.
    pub(super) fn accessibility(on_change: impl Fn() + Send + Sync + 'static) -> Option<Self> {
        Self::new(ACCESSIBILITY_CHANGED, on_change)
    }

    fn new(name: &CStr, on_change: impl Fn() + Send + Sync + 'static) -> Option<Self> {
        let queue = DispatchQueue::new("dev.keyvent.tcc", None);
        let handler: RcBlock<dyn Fn(c_int)> = RcBlock::new(move |_token: c_int| on_change());
        let mut token: c_int = 0;
        // libnotify copies the block, so `handler` may drop after this call.
        let status =
            unsafe { notify_register_dispatch(name.as_ptr(), &mut token, &queue, &handler) };
        if status != NOTIFY_STATUS_OK {
            log::warn!(
                "keyvent: notify_register_dispatch failed ({status}); granting permission while silent needs a restart"
            );
            return None;
        }
        Some(Self {
            token,
            _queue: queue,
        })
    }
}

impl Drop for TccWatch {
    fn drop(&mut self) {
        unsafe { notify_cancel(self.token) };
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    unsafe extern "C" {
        fn notify_post(name: *const c_char) -> u32;
    }

    // A private name, so the test never wakes real TCC observers on the machine.
    fn test_name(tag: &str) -> CString {
        CString::new(format!("dev.keyvent.test.{}.{tag}", std::process::id())).unwrap()
    }

    fn post(name: &CStr) {
        assert_eq!(unsafe { notify_post(name.as_ptr()) }, NOTIFY_STATUS_OK);
    }

    fn watch(name: &CStr) -> (TccWatch, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let watch = TccWatch::new(name, move || {
            let _ = tx.send(());
        })
        .expect("register test notification");
        (watch, rx)
    }

    #[test]
    fn quiet_without_posts() {
        let name = test_name("quiet");
        let (_watch, rx) = watch(&name);
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn delivers_a_post() {
        let name = test_name("post");
        let (_watch, rx) = watch(&name);
        post(&name);
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn drop_cancels_the_subscription() {
        let name = test_name("cancel");
        let (watch, rx) = watch(&name);
        drop(watch);
        post(&name);
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }
}
