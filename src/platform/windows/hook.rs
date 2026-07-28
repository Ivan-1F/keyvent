//! The hook thread: owns the `WH_KEYBOARD_LL` hook and its message loop.
//!
//! The system invokes the hook proc inside this thread's message pump, so all
//! state is lock-free and thread-local. Shutdown is a posted `WM_QUIT`; the
//! thread unhooks and closes the consumer stream before exiting.

use std::cell::Cell;
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MAPVK_VSC_TO_VK_EX, MapVirtualKeyW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, PM_NOREMOVE,
    PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN, WM_USER,
};

use super::convert::Tracker;
use super::keycode::{self, Keycode};
use crate::Status;
use crate::dispatcher::BoxedCallback;
use crate::pairing::Pairing;
use crate::route;
use crate::status::StatusBroadcast;
use crate::suppress::{Action, Suppression};

/// Handle to the running hook thread.
pub(crate) struct HookThread {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl HookThread {
    /// Spawn the hook thread; returns once the thread has a message queue, so
    /// a shutdown posted right after can never be lost. The status becomes
    /// Running once the hook is installed.
    pub(crate) fn spawn(callback: BoxedCallback, broadcast: Arc<StatusBroadcast>) -> Self {
        let (id_tx, id_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("keyvent-hook".into())
            .spawn(move || thread_main(callback, broadcast, id_tx))
            .expect("spawn keyvent-hook thread");
        let thread_id = id_rx.recv().expect("hook thread died at spawn");
        Self {
            thread_id,
            join: Some(join),
        }
    }

    /// Post `WM_QUIT` and join. A stuck thread (misbehaving user callback) is
    /// detached instead of joined forever: the OS skips a wedged low-level
    /// hook, so system input is never frozen.
    pub(crate) fn shutdown(mut self) {
        unsafe {
            if PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) == 0 {
                log::error!("keyvent: failed to post WM_QUIT to the hook thread");
            }
        }
        if let Some(join) = self.join.take() {
            crate::platform::join_with_timeout(join, Duration::from_secs(2), "hook");
        }
    }
}

/// Everything the hook proc needs, living on the hook thread's stack. The
/// proc reaches it through a thread-local pointer, valid for the whole
/// lifetime of the message loop.
struct Ctx {
    callback: BoxedCallback,
    tracker: Tracker,
    suppression: Suppression,
    pairing: Pairing,
}

thread_local! {
    static HOOK_CTX: Cell<*mut Ctx> = const { Cell::new(null_mut()) };
}

fn thread_main(callback: BoxedCallback, broadcast: Arc<StatusBroadcast>, id_tx: mpsc::Sender<u32>) {
    let mut ctx = Ctx {
        callback,
        tracker: Tracker::default(),
        suppression: Suppression::default(),
        pairing: Pairing::default(),
    };
    unsafe {
        // Force the message queue into existence before publishing the id.
        let mut msg: MSG = std::mem::zeroed();
        PeekMessageW(&mut msg, null_mut(), WM_USER, WM_USER, PM_NOREMOVE);
        HOOK_CTX.with(|c| c.set(&mut ctx));
        let _ = id_tx.send(GetCurrentThreadId());

        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), null_mut(), 0);
        if hook.is_null() {
            // No permission dance exists on Windows; failure is not recoverable
            // here, so the stream simply stays Silent.
            log::error!("keyvent: SetWindowsHookEx failed; stream stays silent");
            return;
        }
        // Seed held state before the first pumped event (see Tracker).
        ctx.tracker.init_from_physical(is_physically_held);
        broadcast.publish(Status::Running);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {}
        UnhookWindowsHookEx(hook);
    }

    // The stream ends here: close every dangling consumer-side press.
    let dangling = ctx.pairing.close_all();
    for code in dangling {
        let event = route::synthetic_release(keycode::key_from_code(code), ctx.tracker.snapshot());
        route::dispatch(&mut ctx.callback, event);
    }
    HOOK_CTX.with(|c| c.set(null_mut()));
    log::debug!("keyvent: hook thread exited");
}

/// The raw hook proc. Runs on the hook thread only, inside the message loop.
///
/// Latency contract: everything here is O(µs) — convert, bookkeeping, and the
/// user callback (which the API contract requires to be non-blocking).
/// Windows skips — and can silently remove — a low-level hook whose proc
/// exceeds its timeout budget.
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let ctx = HOOK_CTX.with(Cell::get);
        if !ctx.is_null() {
            let ctx = unsafe { &mut *ctx };
            let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let code = keycode::encode(kb.vkCode, kb.scanCode, kb.flags & LLKHF_EXTENDED != 0);
            // Close keys whose release was missed (secure desktop, skipped
            // events) before converting the new event.
            let skip = (!down).then_some(code);
            for stale in ctx.tracker.reconcile(is_queryable_held, skip) {
                let event =
                    route::synthetic_release(keycode::key_from_code(stale), ctx.tracker.snapshot());
                route::route(
                    &mut ctx.pairing,
                    &mut ctx.suppression,
                    &mut ctx.callback,
                    stale,
                    event,
                    || ctx.tracker.snapshot(),
                );
            }
            let swallow = match ctx.tracker.convert(code, down) {
                // A duplicate modifier press leaves the stream silently.
                None => true,
                Some(event) => {
                    route::route(
                        &mut ctx.pairing,
                        &mut ctx.suppression,
                        &mut ctx.callback,
                        code,
                        event,
                        || ctx.tracker.snapshot(),
                    ) == Action::Swallow
                }
            };
            if swallow {
                return 1;
            }
        }
    }
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

/// Physical held-state query for install-time seeding.
fn is_physically_held(code: Keycode) -> bool {
    unsafe { (GetAsyncKeyState(vk_of(code)) as u16) & 0x8000 != 0 }
}

/// Physical held-state query for reconcile; keys with no VK mapping report as
/// held — they cannot be verified, so the event stream stays their truth.
fn is_queryable_held(code: Keycode) -> bool {
    vk_of(code) != 0 && is_physically_held(code)
}

/// The virtual-key code a keycode's state is reported under. MAPVK_VSC_TO_VK_EX
/// honors the 0xE0 prefix in the scan code's high byte, so extended codes map
/// to their extended virtual keys directly; only VK-encoded codes are special.
fn vk_of(code: Keycode) -> i32 {
    let vk = if code & 0xFF00 == 0xF000 {
        (code & 0xFF) as u32
    } else {
        unsafe { MapVirtualKeyW(code as u32, MAPVK_VSC_TO_VK_EX) }
    };
    vk as i32
}
