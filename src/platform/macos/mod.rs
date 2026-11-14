//! The tap thread: owns the CGEventTap and its CFRunLoop.
//!
//! Caps Lock is delivered as a momentary key.
//! Its FlagsChanged events (keycode 57) signal the lock state toggling, not the physical release.
//! So each one arrives as a Press immediately followed by a synthesized Release.
//! The physical hold is not representable in this stream.
//! One physical tap may produce more than one pair on some system paths.
//! The system action echoes are not physical keys (see [`convert`]).
//! Raw caps events pass to the OS unsuppressed (see [`crate::hotkey::Suppression`]).
//!
//! Threading model:
//! - Only the tap thread runs the event-tap callback, so its state is lock-free thread-local data.
//! - The thread sleeps in its run loop. Commands arrive on a channel, and a run loop source wakes it to drain them.
//! - If a user callback blocks inside the tap callback, the supervisor can still invalidate the tap.
//!   It calls `CFMachPortInvalidate` on the shared handle, which is thread-safe.
//!   This is what keeps a wedged callback from freezing the keyboard.

mod convert;
pub(crate) mod keycode;
mod lifecycle;
mod supervisor;
mod tcc;

pub(crate) use supervisor::Supervisor;

use std::ffi::c_void;
use std::ptr::{NonNull, null_mut};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext,
    kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapCallBack,
    CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
};

use self::keycode::{CAPS_LOCK, Keycode};
use crate::Modifiers;
use crate::dispatcher::BoxedCallback;
use crate::pairing::Pairing;
use crate::platform::Worker;
use crate::route;
use crate::suppress::{Action, Suppression};
use convert::{ModifierTracker, RawKind};

/// Keyboard-only mask, so a stalled tap cannot freeze pointer input.
const TAP_EVENT_MASK: u64 = (1u64 << CGEventType::KeyDown.0)
    | (1u64 << CGEventType::KeyUp.0)
    | (1u64 << CGEventType::FlagsChanged.0);

const INSTALL_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Result of asking the tap thread to install the tap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallOutcome {
    /// The tap is installed and delivering events.
    Installed,
    /// WindowServer refused the tap: no Accessibility permission right now.
    /// The refused tap_create leaked a Mach port pair inside the OS.
    Denied,
    /// The install failed for another reason, such as an unresponsive tap thread.
    Failed,
}

/// State shared between the tap thread and the supervisor thread.
struct TapShared {
    /// Live tap handle.
    /// The Mutex serializes the slot swap; enable and invalidate are documented thread-safe.
    handle: Mutex<Option<TapHandle>>,
}

struct TapHandle(CFRetained<CFMachPort>);
// CFMachPort refcounting is atomic, and CGEventTapEnable/Invalidate are callable from any thread.
// The Mutex in TapShared serializes the slot.
unsafe impl Send for TapHandle {}
unsafe impl Sync for TapHandle {}

impl TapShared {
    fn new() -> Self {
        Self {
            handle: Mutex::new(None),
        }
    }

    /// Force-invalidate the live tap from any thread.
    /// The tap thread drops its own references when it handles the Teardown that always follows.
    fn force_invalidate(&self) {
        if let Some(handle) = self.handle.lock().unwrap().take() {
            CFMachPort::invalidate(&handle.0);
        }
    }

    /// Whether the live tap is currently delivering events. None if no tap.
    fn tap_is_enabled(&self) -> Option<bool> {
        let handle = self.handle.lock().unwrap();
        handle.as_ref().map(|h| CGEvent::tap_is_enabled(&h.0))
    }
}

/// Commands from the supervisor to the tap thread.
enum TapCommand {
    /// Create and install the tap; replies with the outcome.
    Install(mpsc::Sender<InstallOutcome>),
    /// Tear down the tap (remove from event chain, keep the thread).
    Teardown,
    /// Tear down and exit the thread.
    Shutdown,
}

/// Wakes the tap thread's run loop so it drains the command channel.
struct RunLoopWaker {
    run_loop: CFRetained<CFRunLoop>,
    source: CFRetained<CFRunLoopSource>,
}

// Only CFRunLoopSourceSignal and CFRunLoopWakeUp are called through this, and both are meant for other threads.
// Refcounting is atomic, so the final release may happen on any thread.
unsafe impl Send for RunLoopWaker {}

impl RunLoopWaker {
    fn wake(&self) {
        self.source.signal();
        self.run_loop.wake_up();
    }
}

/// Handle to the running tap thread.
struct TapThread {
    commands: mpsc::Sender<TapCommand>,
    waker: RunLoopWaker,
    /// Reply channel of an install that timed out. At most one such install is ever outstanding.
    stalled_install: Option<mpsc::Receiver<InstallOutcome>>,
    worker: Worker,
}

impl TapThread {
    /// Spawn the tap thread; returns once its run loop can be woken, so no command is ever lost.
    fn spawn(callback: BoxedCallback, shared: Arc<TapShared>) -> Self {
        let (commands, command_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = Worker::spawn("keyvent-tap", move || {
            thread_main(callback, shared, command_rx, ready_tx)
        });
        let waker = ready_rx.recv().expect("keyvent-tap thread died at startup");
        Self {
            commands,
            waker,
            stalled_install: None,
            worker,
        }
    }

    fn send(&self, command: TapCommand) -> bool {
        if self.commands.send(command).is_err() {
            return false;
        }
        self.waker.wake();
        true
    }

    /// Ask the tap thread to create and install the tap, waiting up to `INSTALL_REPLY_TIMEOUT`.
    fn install(&mut self) -> InstallOutcome {
        if let Some(stalled) = &self.stalled_install {
            if let Err(TryRecvError::Empty) = stalled.try_recv() {
                // Still busy with the last install, e.g. stuck in a user callback: queue nothing more.
                return InstallOutcome::Failed;
            }
            self.stalled_install = None;
        }
        let (reply, outcome) = mpsc::channel();
        if !self.send(TapCommand::Install(reply)) {
            return InstallOutcome::Failed;
        }
        match outcome.recv_timeout(INSTALL_REPLY_TIMEOUT) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => {
                // The queued Install may still run much later, after a stuck user callback returns.
                // The Teardown right behind it makes the pair's net effect zero.
                self.send(TapCommand::Teardown);
                self.stalled_install = Some(outcome);
                InstallOutcome::Failed
            }
            Err(RecvTimeoutError::Disconnected) => InstallOutcome::Failed,
        }
    }

    /// Ask the tap thread to tear the tap down (non-blocking).
    fn teardown(&self) {
        self.send(TapCommand::Teardown);
    }

    /// Shut the thread down, detaching instead of hanging if it is stuck in a user callback.
    /// The supervisor has already force-invalidated the tap, so a stuck thread is inert.
    fn shutdown(self) {
        self.send(TapCommand::Shutdown);
        self.worker.join_timeout(SHUTDOWN_JOIN_TIMEOUT);
    }
}

/// Everything the tap thread's callouts need.
/// It lives on the heap for the whole run loop, and every callout reaches it through the same raw pointer.
struct Ctx {
    callback: BoxedCallback,
    tracker: ModifierTracker,
    suppression: Suppression,
    pairing: Pairing,
    tap: Option<InstalledTap>,
    shared: Arc<TapShared>,
    commands: mpsc::Receiver<TapCommand>,
}

/// The tap as the tap thread holds it.
struct InstalledTap {
    port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
}

fn thread_main(
    callback: BoxedCallback,
    shared: Arc<TapShared>,
    commands: mpsc::Receiver<TapCommand>,
    ready: mpsc::Sender<RunLoopWaker>,
) {
    let ctx = Box::into_raw(Box::new(Ctx {
        callback,
        tracker: ModifierTracker::default(),
        suppression: Suppression::default(),
        pairing: Pairing::default(),
        tap: None,
        shared,
        commands,
    }));
    let mut context = CFRunLoopSourceContext {
        version: 0,
        info: ctx.cast(),
        retain: None,
        release: None,
        copyDescription: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: Some(perform_commands),
    };
    // CFRunLoopSourceCreate copies the context; `info` stays valid until the source is invalidated below.
    let source = unsafe { CFRunLoopSource::new(None, 0, &mut context) }
        .expect("create the tap thread's command source");
    let run_loop = CFRunLoop::current().expect("tap thread run loop");
    unsafe { run_loop.add_source(Some(&source), kCFRunLoopCommonModes) };

    let waker = RunLoopWaker {
        run_loop,
        source: source.clone(),
    };
    if ready.send(waker).is_ok() {
        // Returns once a Shutdown command stops the run loop.
        CFRunLoop::run();
    }

    source.invalidate();
    // Every callout is finished and none can start, so this is the last access through `ctx`.
    let mut ctx = unsafe { Box::from_raw(ctx) };
    teardown(&mut ctx);
    log::debug!("keyvent: tap thread exited");
}

/// Run loop callout for the command source: drain every queued command.
unsafe extern "C-unwind" fn perform_commands(info: *mut c_void) {
    let ctx = info.cast::<Ctx>();
    loop {
        // The shared borrow ends with this statement, before any command takes `&mut Ctx`.
        let command = unsafe { (*ctx).commands.try_recv() };
        let Ok(command) = command else {
            return;
        };
        match command {
            TapCommand::Install(reply) => {
                let _ = reply.send(unsafe { install(ctx) });
            }
            TapCommand::Teardown => teardown(unsafe { &mut *ctx }),
            TapCommand::Shutdown => {
                teardown(unsafe { &mut *ctx });
                if let Some(run_loop) = CFRunLoop::current() {
                    run_loop.stop();
                }
                return;
            }
        }
    }
}

/// Create and install the tap on this thread's run loop.
///
/// # Safety
///
/// `ctx_ptr` is the tap thread's context pointer, and no other borrow of it is alive.
unsafe fn install(ctx_ptr: *mut Ctx) -> InstallOutcome {
    let ctx = unsafe { &mut *ctx_ptr };
    if ctx.tap.is_some() {
        return InstallOutcome::Installed;
    }
    let callback: CGEventTapCallBack = Some(raw_callback);
    // The tap callback gets the same pointer every other callout uses.
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            TAP_EVENT_MASK,
            callback,
            ctx_ptr.cast(),
        )
    };
    let Some(port) = port else {
        log::debug!("keyvent: tap_create returned NULL (no permission or not yet effective)");
        return InstallOutcome::Denied;
    };
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        log::error!("keyvent: failed to create run loop source");
        CFMachPort::invalidate(&port);
        return InstallOutcome::Failed;
    };
    // Initialize modifier tracking from physical truth, so keys held at stream start are tracked.
    ctx.tracker.init_from_physical(|code| {
        CGEventSource::key_state(CGEventSourceStateID::CombinedSessionState, code)
    });
    // Reconcile the ledgers against physical truth now, not at teardown.
    // The query needs the permission, which we only know is present right now.
    // Presses released while the stream was down close with synthetic releases.
    // Keys still held stay open and close on their real release.
    reconcile_books(ctx);
    // Only now wire the tap into the event chain, so new events meet reconciled ledgers.
    // Enabling earlier could let a release slip past the ledger it just cleared (a phantom release).
    let run_loop = CFRunLoop::current().expect("tap thread run loop");
    unsafe { run_loop.add_source(Some(&source), kCFRunLoopCommonModes) };
    CGEvent::tap_enable(&port, true);
    *ctx.shared.handle.lock().unwrap() = Some(TapHandle(port.clone()));
    ctx.tap = Some(InstalledTap { port, source });
    log::info!("keyvent: event tap installed");
    InstallOutcome::Installed
}

/// Full teardown: remove from run loop, disable, invalidate; safe with no tap installed.
/// The ledgers are deliberately left untouched, so presses can be reconciled at the next install.
/// Synthesizing releases here would fabricate events for keys the user may still be holding.
fn teardown(ctx: &mut Ctx) {
    // Already empty if the supervisor force-invalidated first.
    ctx.shared.handle.lock().unwrap().take();
    let Some(tap) = ctx.tap.take() else {
        return;
    };
    if let Some(run_loop) = CFRunLoop::current() {
        unsafe { run_loop.remove_source(Some(&tap.source), kCFRunLoopCommonModes) };
    }
    CGEvent::tap_enable(&tap.port, false);
    CFMachPort::invalidate(&tap.port);
    log::info!("keyvent: event tap torn down");
}

/// Reconcile the ledgers against physical key state.
/// Called at tap install and on tap-disabled recovery (timeout).
/// Presses whose key is physically up are closed with synthetic releases.
/// Keys still held stay open and close on their real release.
fn reconcile_books(ctx: &mut Ctx) {
    // key_state reports the lock state for Caps Lock, so it cannot verify a caps entry.
    // Physical caps taps close atomically (press and release dispatch back to back).
    // So a dangling caps entry is always a posted event whose release was lost: close it.
    ctx.suppression.reconcile(|code| {
        code != CAPS_LOCK
            && CGEventSource::key_state(CGEventSourceStateID::CombinedSessionState, code)
    });
    let released = ctx.pairing.reconcile(|code| {
        code != CAPS_LOCK
            && CGEventSource::key_state(CGEventSourceStateID::CombinedSessionState, code)
    });
    emit_synthetic_releases(ctx, released);
}

fn emit_synthetic_releases(ctx: &mut Ctx, codes: Vec<Keycode>) {
    for code in codes {
        let event =
            route::synthetic_release(keycode::key_from_code(code), current_modifier_snapshot());
        route::dispatch(&mut ctx.callback, event);
    }
}

/// Snapshot the current modifier flags from the OS (side-agnostic).
fn current_modifier_snapshot() -> Modifiers {
    let flags = CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState);
    modifiers_from_cg(flags)
}

fn modifiers_from_cg(flags: CGEventFlags) -> Modifiers {
    let mut bits = 0u32;
    if flags.contains(CGEventFlags::MaskShift) {
        bits |= Modifiers::SHIFT.bits();
    }
    if flags.contains(CGEventFlags::MaskControl) {
        bits |= Modifiers::CONTROL.bits();
    }
    if flags.contains(CGEventFlags::MaskAlternate) {
        bits |= Modifiers::ALT.bits();
    }
    if flags.contains(CGEventFlags::MaskCommand) {
        bits |= Modifiers::SUPER.bits();
    }
    if flags.contains(CGEventFlags::MaskSecondaryFn) {
        bits |= Modifiers::FN.bits();
    }
    Modifiers::from_bits(bits)
}

/// The raw event-tap callback. Runs on the tap thread only.
///
/// Latency contract: everything here is O(µs), including the non-blocking user callback.
/// There are no locks, no allocations on the hot path, and no cross-thread calls.
unsafe extern "C-unwind" fn raw_callback(
    _proxy: objc2_core_graphics::CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    let ctx = unsafe { &mut *(user_info as *mut Ctx) };
    let cg = unsafe { event.as_ref() };

    // Tap-disabled housekeeping events.
    if event_type == CGEventType::TapDisabledByTimeout {
        reconcile_books(ctx);
        if let Some(tap) = &ctx.tap {
            CGEvent::tap_enable(&tap.port, true);
        }
        log::warn!("keyvent: tap disabled by timeout; re-enabled");
        return event.as_ptr();
    }
    if event_type == CGEventType::TapDisabledByUserInput {
        // Never re-enable on this path, since it may mean permission loss.
        // The tap stays disabled, so the supervisor's next check tears it down.
        log::warn!("keyvent: tap disabled by user input");
        return event.as_ptr();
    }

    // Our own injected events no longer exist; every event here is real.
    let kind = match event_type {
        CGEventType::KeyDown => RawKind::Down,
        CGEventType::KeyUp => RawKind::Up,
        CGEventType::FlagsChanged => RawKind::FlagsChanged,
        _ => return event.as_ptr(),
    };

    let keycode =
        CGEvent::integer_value_field(Some(cg), CGEventField::KeyboardEventKeycode) as Keycode;
    let is_repeat = kind == RawKind::Down
        && CGEvent::integer_value_field(Some(cg), CGEventField::KeyboardEventAutorepeat) != 0;
    let flags = CGEvent::flags(Some(cg));
    let snapshot = modifiers_from_cg(flags);

    // Swallow the globe-action event when its Fn cycle is being suppressed.
    if ctx.suppression.swallows_globe_action(keycode) {
        return null_mut();
    }

    match convert::convert(keycode, is_repeat, snapshot, &mut ctx.tracker, kind) {
        convert::Converted::None => event.as_ptr(),
        convert::Converted::Pair(press, release) => {
            // Momentary caps pair; the raw event passes unsuppressed.
            route::route_observe(
                &mut ctx.pairing,
                &mut ctx.callback,
                keycode,
                press,
                current_modifier_snapshot,
            );
            route::route_observe(
                &mut ctx.pairing,
                &mut ctx.callback,
                keycode,
                release,
                current_modifier_snapshot,
            );
            event.as_ptr()
        }
        convert::Converted::One(key_event) => {
            match route::route(
                &mut ctx.pairing,
                &mut ctx.suppression,
                &mut ctx.callback,
                keycode,
                key_event,
                current_modifier_snapshot,
            ) {
                Action::Swallow => null_mut(),
                Action::Pass => event.as_ptr(),
            }
        }
    }
}
