//! The hook thread: owns the `WH_KEYBOARD_LL` hook and its message loop.
//!
//! The system invokes the hook proc inside this thread's message pump, so all
//! state is lock-free and thread-local. Shutdown is a posted `WM_QUIT`; the
//! thread unhooks and closes the consumer stream before exiting.

use std::cell::Cell;
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::mpsc;
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

use super::convert::{self, Tracker};
use super::keycode::{self, Keycode};
use crate::Status;
use crate::dispatcher::BoxedCallback;
use crate::pairing::Pairing;
use crate::platform::Worker;
use crate::route;
use crate::status::StatusBroadcast;
use crate::suppress::{Action, Suppression};

/// Handle to the running hook thread.
pub(crate) struct HookThread {
    thread_id: u32,
    worker: Worker,
}

impl HookThread {
    /// Spawn the hook thread; returns once the hook is installed or has failed, so the status is meaningful.
    /// The thread has a message queue by then, so a shutdown posted right after can never be lost.
    pub(crate) fn spawn(callback: BoxedCallback, broadcast: Arc<StatusBroadcast>) -> Self {
        let (id_tx, id_rx) = mpsc::channel();
        let worker = Worker::spawn("keyvent-hook", move || {
            thread_main(callback, broadcast, id_tx)
        });
        let thread_id = id_rx.recv().expect("hook thread died at spawn");
        Self { thread_id, worker }
    }

    /// Post `WM_QUIT` and join. A stuck thread (misbehaving user callback) is
    /// detached instead of joined forever: the OS skips a wedged low-level
    /// hook, so system input is never frozen.
    pub(crate) fn shutdown(self) {
        unsafe {
            if PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) == 0 {
                log::error!("keyvent: failed to post WM_QUIT to the hook thread");
            }
        }
        self.worker.join_timeout(Duration::from_secs(2));
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
        let thread_id = GetCurrentThreadId();

        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), null_mut(), 0);
        if hook.is_null() {
            // Windows has no permission to wait for, so the stream stays Silent.
            log::error!("keyvent: SetWindowsHookEx failed; stream stays silent");
            let _ = id_tx.send(thread_id);
            return;
        }
        // Seed held state before the first pumped event (see Tracker).
        ctx.tracker.init_from_physical(is_physically_held);
        broadcast.publish(Status::Running);
        let _ = id_tx.send(thread_id);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {}
        UnhookWindowsHookEx(hook);
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
            for stale in convert::reconcile_before_convert(
                &mut ctx.tracker,
                &ctx.suppression,
                code,
                down,
                is_queryable_held,
            ) {
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

/// Physical held-state query for reconcile; keys with no VK mapping cannot be
/// verified and report as not held, so reconcile closes them at the next event.
fn is_queryable_held(code: Keycode) -> bool {
    let vk = vk_of(code);
    vk != 0 && unsafe { (GetAsyncKeyState(vk) as u16) & 0x8000 != 0 }
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
