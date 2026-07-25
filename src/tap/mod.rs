//! The tap thread: owns the CGEventTap, its CFRunLoop, and the heartbeat.
//!
//! Threading model:
//! - The tap thread is the ONLY thread that runs the event-tap callback, so all
//!   conversion/tracking/suppression state is lock-free thread-local data.
//! - The supervisor thread talks to it through a command channel (`Install` /
//!   `Teardown` / `Shutdown`) and shared atomics (`TapShared`).
//! - In the emergency case (user callback blocked inside the tap callback), the
//!   supervisor can bypass the command channel and `CFMachPortInvalidate` the
//!   tap directly through the shared handle — that call is thread-safe and is
//!   what keeps a wedged callback from freezing the keyboard.

mod convert;
mod pairing;
mod suppress;

use std::ffi::c_void;
use std::ptr::{NonNull, null_mut};
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes,
    kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapCallBack,
    CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
};

use crate::keycode::{self, Keycode};
use crate::{KeyEvent, KeyEventKind, Modifiers};

use convert::{ModifierTracker, RawKind};
use pairing::{Pairing, Plan};
use suppress::{Action, Suppression};

/// Keyboard-only mask: mouse/scroll/gesture events never route through the tap,
/// so a stalled tap cannot freeze pointer input.
const TAP_EVENT_MASK: u64 = (1u64 << CGEventType::KeyDown.0)
    | (1u64 << CGEventType::KeyUp.0)
    | (1u64 << CGEventType::FlagsChanged.0);

const RUN_LOOP_SLICE: Duration = Duration::from_millis(100);
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
const INSTALL_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// F20: in the macOS keycode space but absent from most physical keyboards.
const HEARTBEAT_KEYCODE: Keycode = 90;
/// Marker in the high 32 bits of `kCGEventSourceUserData` for our own injected events.
const MAGIC_HIGH: i64 = 0x4B56_5400_0000_0000;

/// State shared between the tap thread and the supervisor thread.
pub(crate) struct TapShared {
    /// Live tap handle. The Mutex serializes the slot swap; the CF operations on
    /// the handle itself (enable/invalidate) are documented thread-safe.
    handle: Mutex<Option<TapHandle>>,
    hb_sent: AtomicU32,
    hb_acked: AtomicU32,
    /// kCGEventTapDisabledByTimeout deliveries since spawn.
    timeouts: AtomicU32,
    /// kCGEventTapDisabledByUserInput delivered; the supervisor tears down on this.
    user_input_disabled: AtomicBool,
    /// User-callback panics caught at the FFI boundary.
    callback_panics: AtomicU32,
}

struct TapHandle(CFRetained<CFMachPort>);
// CFMachPort refcounting is atomic and CGEventTapEnable/Invalidate are
// documented callable from any thread; the Mutex in TapShared serializes the slot.
unsafe impl Send for TapHandle {}
unsafe impl Sync for TapHandle {}

impl TapShared {
    pub(crate) fn new() -> Self {
        Self {
            handle: Mutex::new(None),
            hb_sent: AtomicU32::new(0),
            hb_acked: AtomicU32::new(0),
            timeouts: AtomicU32::new(0),
            user_input_disabled: AtomicBool::new(false),
            callback_panics: AtomicU32::new(0),
        }
    }

    /// Force-invalidate the live tap from any thread. Returns true if a tap was
    /// present. The tap thread notices the empty slot on its next iteration and
    /// drops its own references.
    pub(crate) fn force_invalidate(&self) -> bool {
        let handle = self.handle.lock().unwrap().take();
        if let Some(handle) = handle {
            CFMachPort::invalidate(&handle.0);
            true
        } else {
            false
        }
    }

    /// Number of heartbeats sent but not yet echoed back through the tap.
    pub(crate) fn heartbeat_gap(&self) -> u32 {
        self.hb_sent.load(SeqCst).saturating_sub(self.hb_acked())
    }

    /// Last heartbeat sequence number echoed back.
    pub(crate) fn hb_acked(&self) -> u32 {
        self.hb_acked.load(SeqCst)
    }

    pub(crate) fn take_user_input_disabled(&self) -> bool {
        self.user_input_disabled.swap(false, SeqCst)
    }

    // Reserved for future status reporting (panic/timeout counters surfaced
    // through Guard); kept as part of the shared-state contract.
    #[allow(dead_code)]
    pub(crate) fn timeouts(&self) -> u32 {
        self.timeouts.load(SeqCst)
    }

    #[allow(dead_code)]
    pub(crate) fn callback_panics(&self) -> u32 {
        self.callback_panics.load(SeqCst)
    }
}

/// Commands from the supervisor to the tap thread.
pub(crate) enum TapCommand {
    /// Create and install the tap; replies with success.
    Install(mpsc::Sender<bool>),
    /// Tear down the tap (remove from event chain, keep the thread).
    Teardown,
    /// Tear down and exit the thread.
    Shutdown,
}

/// Handle to the running tap thread.
pub(crate) struct TapThread {
    cmd: mpsc::Sender<TapCommand>,
    join: Option<JoinHandle<()>>,
}

impl TapThread {
    pub(crate) fn spawn(
        callback: Box<dyn FnMut(KeyEvent) -> bool + Send>,
        shared: Arc<TapShared>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("keyvent-tap".into())
            .spawn(move || thread_main(callback, shared, cmd_rx))
            .expect("spawn keyvent-tap thread");
        Self {
            cmd: cmd_tx,
            join: Some(join),
        }
    }

    /// Ask the tap thread to create + install the tap; blocks up to
    /// `INSTALL_REPLY_TIMEOUT` for the result.
    pub(crate) fn install(&self) -> bool {
        let (tx, rx) = mpsc::channel();
        if self.cmd.send(TapCommand::Install(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(INSTALL_REPLY_TIMEOUT).unwrap_or(false)
    }

    /// Ask the tap thread to tear the tap down (non-blocking).
    pub(crate) fn teardown(&self) {
        let _ = self.cmd.send(TapCommand::Teardown);
    }

    /// Shut the thread down. If the thread is stuck (e.g. inside a misbehaving
    /// user callback), detach instead of hanging forever — the supervisor has
    /// already force-invalidated the tap, so the stuck thread is inert.
    pub(crate) fn shutdown(mut self) {
        let _ = self.cmd.send(TapCommand::Shutdown);
        if let Some(join) = self.join.take() {
            join_with_timeout(join, Duration::from_secs(2), "tap");
        }
    }
}

/// Join a thread, but detach instead of hanging forever if it is stuck
/// (e.g. the tap thread blocked inside a misbehaving user callback).
pub(crate) fn join_with_timeout(join: JoinHandle<()>, timeout: Duration, name: &str) {
    let start = Instant::now();
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

/// Everything the tap callback needs, living on the tap thread's stack.
/// The callback receives a raw pointer to this via user_info; it is valid for
/// the whole lifetime of the run loop.
struct Ctx {
    callback: Box<dyn FnMut(KeyEvent) -> bool + Send>,
    tracker: ModifierTracker,
    suppression: Suppression,
    pairing: Pairing,
    tap: Option<CFRetained<CFMachPort>>,
    source: Option<CFRetained<CFRunLoopSource>>,
    hb_next: Instant,
    hb_seq: u32,
    shared: Arc<TapShared>,
}

fn thread_main(
    callback: Box<dyn FnMut(KeyEvent) -> bool + Send>,
    shared: Arc<TapShared>,
    cmd_rx: mpsc::Receiver<TapCommand>,
) {
    let mut ctx = Ctx {
        callback,
        tracker: ModifierTracker::default(),
        suppression: Suppression::default(),
        pairing: Pairing::default(),
        tap: None,
        source: None,
        hb_next: Instant::now(),
        hb_seq: 0,
        shared,
    };

    'outer: loop {
        // Drain pending commands first (low-latency path between run loop slices).
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                TapCommand::Install(reply) => {
                    let _ = reply.send(install(&mut ctx));
                }
                TapCommand::Teardown => teardown(&mut ctx),
                TapCommand::Shutdown => break 'outer,
            }
        }

        if ctx.tap.is_some() {
            unsafe {
                CFRunLoop::run_in_mode(
                    kCFRunLoopDefaultMode,
                    RUN_LOOP_SLICE.as_secs_f64(),
                    true,
                )
            };
            // Did the supervisor force-invalidate behind our back?
            if ctx.shared.handle.lock().unwrap().is_none() {
                log::warn!("keyvent: tap was force-invalidated externally; dropping local state");
                drop_local_tap(&mut ctx);
            }
            // Heartbeat.
            if ctx.tap.is_some() && Instant::now() >= ctx.hb_next {
                post_heartbeat(&mut ctx);
                ctx.hb_next = Instant::now() + HEARTBEAT_INTERVAL;
            }
        } else {
            // No tap installed: wait for commands without burning CPU.
            match cmd_rx.recv_timeout(RUN_LOOP_SLICE) {
                Ok(TapCommand::Install(reply)) => {
                    let _ = reply.send(install(&mut ctx));
                }
                Ok(TapCommand::Teardown) => {}
                Ok(TapCommand::Shutdown) => break 'outer,
                Err(_) => {}
            }
        }
    }

    teardown(&mut ctx);
    log::debug!("keyvent: tap thread exited");
}

/// Create + install the tap on this thread's run loop.
fn install(ctx: &mut Ctx) -> bool {
    if ctx.tap.is_some() {
        return true;
    }
    let callback: CGEventTapCallBack = Some(raw_callback);
    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            TAP_EVENT_MASK,
            callback,
            ctx as *mut Ctx as *mut c_void,
        )
    };
    let Some(tap) = tap else {
        log::debug!("keyvent: tap_create returned NULL (no permission or not yet effective)");
        return false;
    };
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&tap), 0) else {
        log::error!("keyvent: failed to create run loop source");
        return false;
    };
    let run_loop = CFRunLoop::current().unwrap();
    unsafe { run_loop.add_source(Some(&source), kCFRunLoopCommonModes) };
    CGEvent::tap_enable(&tap, true);
    *ctx.shared.handle.lock().unwrap() = Some(TapHandle(tap.clone()));
    ctx.tap = Some(tap);
    ctx.source = Some(source);
    ctx.hb_next = Instant::now();
    // Initialize modifier tracking from physical truth, so keys held while the
    // stream starts are tracked correctly instead of guessed later.
    ctx.tracker
        .init_from_physical(|code| CGEventSource::key_state(CGEventSourceStateID::CombinedSessionState, code));
    log::info!("keyvent: event tap installed");
    true
}

/// Full teardown: remove from run loop, disable, invalidate. Safe to call with
/// no tap installed. Closing the stream means every unclosed consumer-side
/// press is completed with a synthetic release.
fn teardown(ctx: &mut Ctx) {
    // OS-side book: drop entries whose key is physically up so a later rebuild
    // doesn't force-swallow releases for presses the OS actually saw.
    ctx.suppression
        .reconcile(|code| CGEventSource::key_state(CGEventSourceStateID::CombinedSessionState, code));
    // Consumer-side ledger: the stream ends here, close everything.
    let dangling = ctx.pairing.close_all();
    emit_synthetic_releases(ctx, dangling);
    drop_local_tap(ctx);
    ctx.shared.handle.lock().unwrap().take();
}

/// Drop run-loop-local tap state without touching the shared handle slot
/// (used when the supervisor already invalidated it).
fn drop_local_tap(ctx: &mut Ctx) {
    if let (Some(tap), Some(source)) = (ctx.tap.take(), ctx.source.take()) {
        if let Some(run_loop) = CFRunLoop::current() {
            unsafe { run_loop.remove_source(Some(&source), kCFRunLoopCommonModes) };
        }
        CGEvent::tap_enable(&tap, false);
        CFMachPort::invalidate(&tap);
        log::info!("keyvent: event tap torn down");
    }
}

/// Recovery reconcile after a tap-disabled event (timeout / userInput): the
/// stream continues, so only presses whose key is physically up are closed —
/// keys still held stay open and close on their real release.
fn reconcile_books(ctx: &mut Ctx) {
    ctx.suppression
        .reconcile(|code| CGEventSource::key_state(CGEventSourceStateID::CombinedSessionState, code));
    let released = ctx
        .pairing
        .reconcile(|code| CGEventSource::key_state(CGEventSourceStateID::CombinedSessionState, code));
    emit_synthetic_releases(ctx, released);
}

fn emit_synthetic_releases(ctx: &mut Ctx, codes: Vec<Keycode>) {
    for code in codes {
        let event = KeyEvent {
            key: keycode::key_from_code(code),
            kind: KeyEventKind::Release,
            modifiers: current_modifier_snapshot(),
            is_repeat: false,
        };
        dispatch(ctx, event);
    }
}

/// Dispatch one event to the user callback with panic isolation.
/// Returns the callback's swallow decision (false on panic).
fn dispatch(ctx: &mut Ctx, event: KeyEvent) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (ctx.callback)(event))) {
        Ok(swallow) => swallow,
        Err(_) => {
            ctx.shared.callback_panics.fetch_add(1, SeqCst);
            log::error!("keyvent: user callback panicked; event passed through");
            false
        }
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
        bits |= Modifiers::OPTION.bits();
    }
    if flags.contains(CGEventFlags::MaskCommand) {
        bits |= Modifiers::COMMAND.bits();
    }
    if flags.contains(CGEventFlags::MaskSecondaryFn) {
        bits |= Modifiers::FN.bits();
    }
    if flags.contains(CGEventFlags::MaskAlphaShift) {
        bits |= Modifiers::CAPS_LOCK.bits();
    }
    Modifiers::from_bits(bits)
}

/// Inject a marked heartbeat event; the tap callback recognizes the marker,
/// records the echo, and swallows it.
fn post_heartbeat(ctx: &mut Ctx) {
    ctx.hb_seq = ctx.hb_seq.wrapping_add(1);
    let seq = ctx.hb_seq;
    let marker = MAGIC_HIGH | seq as i64;
    if let Some(src) = CGEventSource::new(CGEventSourceStateID::Private) {
        CGEventSource::set_user_data(Some(&src), marker);
        for down in [true, false] {
            if let Some(ev) = CGEvent::new_keyboard_event(Some(&src), HEARTBEAT_KEYCODE, down) {
                CGEvent::set_integer_value_field(
                    Some(&ev),
                    CGEventField::EventSourceUserData,
                    marker,
                );
                CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&ev));
            }
        }
    }
    ctx.shared.hb_sent.store(seq, SeqCst);
}

/// The raw event-tap callback. Runs on the tap thread only.
///
/// Latency contract: everything here is O(µs) — convert, bookkeeping, and the
/// user callback (which the API contract requires to be non-blocking). There
/// are no locks, no allocations on the hot path, and no cross-thread calls.
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
        ctx.shared.timeouts.fetch_add(1, SeqCst);
        reconcile_books(ctx);
        if let Some(tap) = &ctx.tap {
            CGEvent::tap_enable(tap, true);
        }
        log::warn!("keyvent: tap disabled by timeout; re-enabled");
        return event.as_ptr();
    }
    if event_type == CGEventType::TapDisabledByUserInput {
        // Never re-enable on this path (it may mean permission loss). The
        // supervisor decides what to do (teardown closes the stream).
        ctx.shared.user_input_disabled.store(true, SeqCst);
        log::warn!("keyvent: tap disabled by user input");
        return event.as_ptr();
    }

    // Our own heartbeat?
    let marker = CGEvent::integer_value_field(Some(cg), CGEventField::EventSourceUserData);
    if (marker & MAGIC_HIGH) == MAGIC_HIGH {
        let seq = (marker & 0xFFFF_FFFF) as u32;
        ctx.shared.hb_acked.store(seq, SeqCst);
        return null_mut();
    }

    let kind = match event_type {
        CGEventType::KeyDown => RawKind::Down,
        CGEventType::KeyUp => RawKind::Up,
        CGEventType::FlagsChanged => RawKind::FlagsChanged,
        _ => return event.as_ptr(),
    };

    let keycode = CGEvent::integer_value_field(Some(cg), CGEventField::KeyboardEventKeycode) as Keycode;
    let is_repeat = kind == RawKind::Down
        && CGEvent::integer_value_field(Some(cg), CGEventField::KeyboardEventAutorepeat) != 0;
    let snapshot = modifiers_from_cg(CGEvent::flags(Some(cg)));

    let key_event = convert::convert(keycode, is_repeat, snapshot, &mut ctx.tracker, kind);

    // Consumer side: pairing ledger decides what the callback may see.
    // OS side: suppression bookkeeping decides what happens to the raw event.
    // The two ledgers are independent — e.g. an orphan release is hidden from
    // the consumer but still passed through to the OS if the OS saw its press.
    match key_event.kind {
        KeyEventKind::Press => {
            if ctx.pairing.press(keycode, is_repeat) == Plan::CloseThenDeliver {
                // A previous release was lost: close the dangling press first.
                dispatch(ctx, synthetic_release(keycode));
            }
            let user_swallow = dispatch(ctx, key_event);
            match ctx.suppression.press(keycode, is_repeat, user_swallow) {
                Action::Swallow => null_mut(),
                Action::Pass => event.as_ptr(),
            }
        }
        KeyEventKind::Release => {
            let deliver = ctx.pairing.release(keycode) == Plan::Deliver;
            if deliver {
                dispatch(ctx, key_event);
            }
            // The release's swallow decision is derived from the press
            // (see Suppression); the callback is not consulted.
            match ctx.suppression.release(keycode) {
                Action::Swallow => null_mut(),
                Action::Pass => event.as_ptr(),
            }
        }
    }
}

fn synthetic_release(code: Keycode) -> KeyEvent {
    KeyEvent {
        key: keycode::key_from_code(code),
        kind: KeyEventKind::Release,
        modifiers: current_modifier_snapshot(),
        is_repeat: false,
    }
}
