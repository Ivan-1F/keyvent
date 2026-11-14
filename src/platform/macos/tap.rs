//! The tap thread, which runs the CFRunLoop hosting the [`Stream`].
//!
//! Other threads never touch the stream directly; they hand it closures, which run in order between event callbacks.

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use block2::RcBlock;
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext, kCFRunLoopCommonModes,
};

use super::stream::{self, SharedTap, Stream};
use crate::dispatcher::BoxedCallback;
use crate::platform::Worker;

const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct TapThread {
    run_loop: RemoteRunLoop,
    worker: Worker,
}

struct RemoteRunLoop(CFRetained<CFRunLoop>);

// Only CFRunLoopPerformBlock and CFRunLoopWakeUp are called through this; both are meant for other threads.
// Refcounting is atomic, so the final release may happen on any thread.
unsafe impl Send for RemoteRunLoop {}

impl TapThread {
    /// Spawn the tap thread; returns once it can receive closures.
    pub(super) fn spawn(callback: BoxedCallback, shared: Arc<SharedTap>) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = Worker::spawn("keyvent-tap", move || {
            let mut stream = Stream::new(callback, shared);
            let run_loop = CFRunLoop::current().expect("tap thread run loop");
            // CFRunLoopRun returns at once from a loop without sources, as this one is while no tap is installed.
            let keep_alive = idle_source();
            unsafe { run_loop.add_source(Some(&keep_alive), kCFRunLoopCommonModes) };
            if ready_tx.send(RemoteRunLoop(run_loop)).is_ok() {
                // Returns once `shutdown` stops the loop.
                stream::run(&mut stream, CFRunLoop::run);
            }
            stream.teardown();
            log::debug!("keyvent: tap thread exited");
        });
        let run_loop = ready_rx.recv().expect("keyvent-tap thread died at startup");
        Self { run_loop, worker }
    }

    /// Run `f` on the tap thread and wait for its result, for as long as the thread is busy.
    /// None if the stream was unavailable.
    pub(super) fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Stream) -> R + Send + 'static,
    ) -> Option<R> {
        let (reply, result) = mpsc::channel();
        self.post(move |stream| {
            let _ = reply.send(f(stream));
        });
        result.recv().ok()
    }

    pub(super) fn post(&self, f: impl FnOnce(&mut Stream) + Send + 'static) {
        self.perform(move || {
            stream::with_current(f);
        });
    }

    /// Stop the thread, detaching it if it does not exit in time.
    /// The caller closes the shared tap first, so a thread stuck in a user callback is inert.
    pub(super) fn shutdown(self) {
        // Queued rather than called directly: a stop sent before the loop starts running would be lost.
        self.perform(|| {
            if let Some(run_loop) = CFRunLoop::current() {
                run_loop.stop();
            }
        });
        self.worker.join_timeout(SHUTDOWN_JOIN_TIMEOUT);
    }

    fn perform(&self, f: impl FnOnce() + Send + 'static) {
        // A block must be Fn, so the FnOnce is moved out when the block runs.
        let f = Cell::new(Some(f));
        let block = RcBlock::new(move || {
            if let Some(f) = f.take() {
                f();
            }
        });
        unsafe {
            self.run_loop
                .0
                .perform_block(kCFRunLoopCommonModes.map(|mode| &**mode), Some(&block));
        }
        // PerformBlock only queues; it does not wake a sleeping loop.
        self.run_loop.0.wake_up();
    }
}

/// A source that is never signaled; it only keeps CFRunLoopRun from returning.
fn idle_source() -> CFRetained<CFRunLoopSource> {
    unsafe extern "C-unwind" fn never(_info: *mut c_void) {}
    let mut context = CFRunLoopSourceContext {
        version: 0,
        info: null_mut(),
        retain: None,
        release: None,
        copyDescription: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: Some(never),
    };
    // CFRunLoopSourceCreate copies the context.
    unsafe { CFRunLoopSource::new(None, 0, &mut context) }
        .expect("create the tap thread's idle source")
}
