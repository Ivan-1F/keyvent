//! The event tap, its callback, and the key ledgers, all living on the tap thread.

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr::{NonNull, null_mut};
use std::sync::{Arc, Mutex};

use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventType,
};

use super::convert::{self, ModifierTracker, RawKind};
use super::keycode::{self, CAPS_LOCK, Keycode};
use crate::Modifiers;
use crate::dispatcher::BoxedCallback;
use crate::pairing::Pairing;
use crate::route;
use crate::suppress::{Action, Suppression};

/// Keyboard-only mask, so a stalled tap cannot freeze pointer input.
pub(super) const TAP_EVENT_MASK: u64 = (1u64 << CGEventType::KeyDown.0)
    | (1u64 << CGEventType::KeyUp.0)
    | (1u64 << CGEventType::FlagsChanged.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InstallOutcome {
    Installed,
    /// No Accessibility permission. The refused tap_create leaked a Mach port pair inside the OS.
    Denied,
    /// Any other failure, including an install after shutdown.
    Failed,
}

/// The live tap's port, so the supervisor can check and invalidate the tap while the tap thread is stuck in a user callback.
pub(super) struct SharedTap {
    slot: Mutex<Slot>,
}

struct Slot {
    port: Option<Port>,
    /// Set by `close`; no tap may be published afterwards.
    closed: bool,
}

struct Port(CFRetained<CFMachPort>);
// CFMachPort refcounting is atomic, and CGEventTapIsEnabled and CFMachPortInvalidate are thread-safe.
unsafe impl Send for Port {}

impl SharedTap {
    pub(super) fn new() -> Self {
        Self {
            slot: Mutex::new(Slot {
                port: None,
                closed: false,
            }),
        }
    }

    pub(super) fn is_enabled(&self) -> bool {
        let slot = self.slot.lock().unwrap();
        slot.port
            .as_ref()
            .is_some_and(|port| CGEvent::tap_is_enabled(&port.0))
    }

    /// Remove the live tap from the event chain at once, whatever the tap thread is doing.
    pub(super) fn invalidate(&self) {
        if let Some(port) = self.slot.lock().unwrap().port.take() {
            CFMachPort::invalidate(&port.0);
        }
    }

    /// Invalidate the live tap and refuse every later one.
    pub(super) fn close(&self) {
        let mut slot = self.slot.lock().unwrap();
        slot.closed = true;
        if let Some(port) = slot.port.take() {
            CFMachPort::invalidate(&port.0);
        }
    }

    /// False if closed, in which case the caller must discard the tap.
    fn publish(&self, port: &CFRetained<CFMachPort>) -> bool {
        let mut slot = self.slot.lock().unwrap();
        if slot.closed {
            return false;
        }
        slot.port = Some(Port(port.clone()));
        true
    }

    fn clear(&self) {
        self.slot.lock().unwrap().port = None;
    }
}

pub(super) struct Stream {
    callback: BoxedCallback,
    tracker: ModifierTracker,
    suppression: Suppression,
    pairing: Pairing,
    tap: Option<InstalledTap>,
    shared: Arc<SharedTap>,
}

struct InstalledTap {
    port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
}

thread_local! {
    /// Set for the duration of [`run`].
    /// Taken out while in use, so a nested callout finds it empty instead of aliasing it.
    static CURRENT: Cell<*mut Stream> = const { Cell::new(null_mut()) };
}

/// Run `body` with `stream` reachable through [`with_current`].
pub(super) fn run(stream: &mut Stream, body: impl FnOnce()) {
    CURRENT.with(|current| current.set(stream));
    body();
    CURRENT.with(|current| current.set(null_mut()));
}

/// None outside [`run`] or when already inside a callout.
pub(super) fn with_current<R>(f: impl FnOnce(&mut Stream) -> R) -> Option<R> {
    let ptr = CURRENT.with(|current| current.replace(null_mut()));
    if ptr.is_null() {
        return None;
    }
    // Only this thread ever sees the pointer, and it stays taken until `f` returns.
    let result = f(unsafe { &mut *ptr });
    CURRENT.with(|current| current.set(ptr));
    Some(result)
}

impl Stream {
    pub(super) fn new(callback: BoxedCallback, shared: Arc<SharedTap>) -> Self {
        Self {
            callback,
            tracker: ModifierTracker::default(),
            suppression: Suppression::default(),
            pairing: Pairing::default(),
            tap: None,
            shared,
        }
    }

    pub(super) fn install(&mut self) -> InstallOutcome {
        if self.tap.is_some() {
            return InstallOutcome::Installed;
        }
        let port = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                TAP_EVENT_MASK,
                Some(raw_callback),
                null_mut(),
            )
        };
        let Some(port) = port else {
            log::debug!("keyvent: tap_create returned NULL (no permission or not yet effective)");
            return InstallOutcome::Denied;
        };
        // Published before wiring, so a concurrent close always reaches it.
        if !self.shared.publish(&port) {
            CFMachPort::invalidate(&port);
            return InstallOutcome::Failed;
        }
        let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
            log::error!("keyvent: failed to create run loop source");
            self.shared.invalidate();
            return InstallOutcome::Failed;
        };
        // Keys already held when the stream starts must be tracked too.
        self.tracker.init_from_physical(|code| {
            CGEventSource::key_state(CGEventSourceStateID::CombinedSessionState, code)
        });
        // Reconcile here rather than at teardown: the key-state query needs the permission, known present only now.
        self.reconcile();
        // Wired only after reconciling: a release arriving earlier could slip past the ledger it just cleared.
        let run_loop = CFRunLoop::current().expect("tap thread run loop");
        unsafe { run_loop.add_source(Some(&source), kCFRunLoopCommonModes) };
        CGEvent::tap_enable(&port, true);
        self.tap = Some(InstalledTap { port, source });
        log::info!("keyvent: event tap installed");
        InstallOutcome::Installed
    }

    /// Leaves the ledgers untouched: synthesizing releases here would fabricate them for keys the user may still hold.
    /// The next install reconciles them instead.
    pub(super) fn teardown(&mut self) {
        self.shared.clear();
        let Some(tap) = self.tap.take() else {
            return;
        };
        if let Some(run_loop) = CFRunLoop::current() {
            unsafe { run_loop.remove_source(Some(&tap.source), kCFRunLoopCommonModes) };
        }
        CGEvent::tap_enable(&tap.port, false);
        CFMachPort::invalidate(&tap.port);
        log::info!("keyvent: event tap torn down");
    }

    /// Close every open press whose key is physically up with a synthetic release.
    fn reconcile(&mut self) {
        // key_state reports the lock state for Caps Lock, so a caps entry is always closed.
        // That is safe: a physical caps tap closes atomically, so an open entry is a posted event that lost its release.
        let held = |code| {
            code != CAPS_LOCK
                && CGEventSource::key_state(CGEventSourceStateID::CombinedSessionState, code)
        };
        self.suppression.reconcile(held);
        for code in self.pairing.reconcile(held) {
            let event =
                route::synthetic_release(keycode::key_from_code(code), current_modifier_snapshot());
            route::dispatch(&mut self.callback, event);
        }
    }

    /// Returns the event to pass on, or null to swallow it.
    /// Runs on every keystroke: no locks, no allocations, no cross-thread calls.
    fn on_event(&mut self, event_type: CGEventType, event: NonNull<CGEvent>) -> *mut CGEvent {
        let cg = unsafe { event.as_ref() };

        if event_type == CGEventType::TapDisabledByTimeout {
            // Releases may have been missed while disabled.
            self.reconcile();
            if let Some(tap) = &self.tap {
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
        let snapshot = modifiers_from_cg(CGEvent::flags(Some(cg)));

        if self.suppression.swallows_globe_action(keycode) {
            return null_mut();
        }

        match convert::convert(keycode, is_repeat, snapshot, &mut self.tracker, kind) {
            convert::Converted::None => event.as_ptr(),
            convert::Converted::Pair(press, release) => {
                // Caps Lock is observed only; its raw event always passes (see `hotkey::Suppression`).
                for key_event in [press, release] {
                    route::route_observe(
                        &mut self.pairing,
                        &mut self.callback,
                        keycode,
                        key_event,
                        current_modifier_snapshot,
                    );
                }
                event.as_ptr()
            }
            convert::Converted::One(key_event) => match route::route(
                &mut self.pairing,
                &mut self.suppression,
                &mut self.callback,
                keycode,
                key_event,
                current_modifier_snapshot,
            ) {
                Action::Swallow => null_mut(),
                Action::Pass => event.as_ptr(),
            },
        }
    }
}

unsafe extern "C-unwind" fn raw_callback(
    _proxy: objc2_core_graphics::CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    _user_info: *mut c_void,
) -> *mut CGEvent {
    with_current(|stream| stream.on_event(event_type, event)).unwrap_or(event.as_ptr())
}

fn current_modifier_snapshot() -> Modifiers {
    modifiers_from_cg(CGEventSource::flags_state(
        CGEventSourceStateID::CombinedSessionState,
    ))
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
