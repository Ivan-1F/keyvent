//! The supervisor thread: owns the permission-lifecycle state machine.
//!
//! Detection philosophy (validated by spike on macOS 26.1): never trust
//! `AXIsProcessTrusted` or tap-disabled notifications — probe functionally.
//!
//! - **tap_create probe** (every 1s): ground truth for "can this process filter
//!   keyboard events right now". NULL = no.
//! - **heartbeat echo**: end-to-end proof that the tap + callback path is alive.
//!
//! Revocation = heartbeat lost AND probe NULL. A lost heartbeat with a healthy
//! probe is transient (secure input, screen lock, sleep/wake, inert tap) and is
//! healed by a rebuild. All automated recovery is budgeted; the only unbudgeted
//! action is *tearing down*, which always moves the system toward safety.

use std::ptr::null_mut;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use objc2_core_foundation::CFMachPort;
use objc2_core_graphics::{
    CGEvent, CGEventTapCallBack, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType,
};

use crate::Status;
use crate::tap::{TapShared, TapThread};

const TICK: Duration = Duration::from_millis(200);
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// Heartbeat gaps (at 500ms interval) before entering Suspicion.
const HB_LOSS_GAP: u32 = 3;
/// Time in Suspicion after a rebuild before counting a failed recovery cycle.
const SUSPICION_GRACE: Duration = Duration::from_secs(3);
/// Failed recovery cycles before Cooldown.
const MAX_RECOVERY_CYCLES: u32 = 3;
const COOLDOWN_DURATION: Duration = Duration::from_secs(30);
/// AX-hint says trusted but the probe says NULL for this long -> StalePermission.
const STALE_HINT_AFTER: Duration = Duration::from_secs(60);

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

// HIToolbox is not independently linkable; the Carbon umbrella re-exports it.
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
}

fn ax_hint() -> bool {
    unsafe { AXIsProcessTrusted() }
}

fn secure_input_active() -> bool {
    unsafe { IsSecureEventInputEnabled() }
}

/// Create + immediately invalidate a throwaway tap: the functional permission probe.
fn probe_tap() -> bool {
    unsafe extern "C-unwind" fn noop(
        _proxy: objc2_core_graphics::CGEventTapProxy,
        _event_type: CGEventType,
        event: std::ptr::NonNull<CGEvent>,
        _user_info: *mut std::ffi::c_void,
    ) -> *mut CGEvent {
        event.as_ptr()
    }
    const MASK: u64 = (1u64 << CGEventType::KeyDown.0)
        | (1u64 << CGEventType::KeyUp.0)
        | (1u64 << CGEventType::FlagsChanged.0);
    let callback: CGEventTapCallBack = Some(noop);
    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            MASK,
            callback,
            null_mut(),
        )
    };
    if let Some(tap) = tap {
        CFMachPort::invalidate(&tap);
        true
    } else {
        false
    }
}

enum State {
    /// No tap installed; probing for permission.
    Silent,
    /// Tap installed and heartbeats echoing.
    Running,
    /// Heartbeat lost; cause not yet discriminated.
    Suspicion {
        entered: Instant,
        rebuilt: bool,
        acked_at_entry: u32,
    },
    /// Recovery budget exhausted; doing nothing but probing.
    Cooldown {
        until: Instant,
    },
}

pub(crate) struct Supervisor {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Supervisor {
    pub(crate) fn spawn(
        tap: TapThread,
        shared: Arc<TapShared>,
        status: Arc<AtomicU8>,
        status_cb: Arc<Mutex<Option<Box<dyn Fn(Status) + Send>>>>,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown2 = shutdown.clone();
        let join = thread::Builder::new()
            .name("keyvent-supervisor".into())
            .spawn(move || supervise(tap, shared, status, status_cb, shutdown2))
            .expect("spawn keyvent-supervisor thread");
        Self {
            shutdown,
            join: Some(join),
        }
    }

    pub(crate) fn shutdown(mut self) {
        self.shutdown.store(true, SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn announce(
    status: &Arc<AtomicU8>,
    status_cb: &Arc<Mutex<Option<Box<dyn Fn(Status) + Send>>>>,
    current: &mut Status,
    next: Status,
) {
    if *current == next {
        return;
    }
    log::info!("keyvent: status {current:?} -> {next:?}");
    *current = next;
    status.store(next as u8, SeqCst);
    let cb = status_cb.lock().unwrap();
    if let Some(cb) = cb.as_ref() {
        cb(next);
    }
}

#[allow(clippy::too_many_arguments)]
fn supervise(
    tap: TapThread,
    shared: Arc<TapShared>,
    status: Arc<AtomicU8>,
    status_cb: Arc<Mutex<Option<Box<dyn Fn(Status) + Send>>>>,
    shutdown: Arc<AtomicBool>,
) {
    let mut state = State::Silent;
    let mut current_status = Status::Silent;
    let mut probe_due = Instant::now(); // probe immediately on start
    let mut cycles = 0u32;
    let mut stale_since: Option<Instant> = None;

    announce(&status, &status_cb, &mut current_status, Status::Silent);

    while !shutdown.load(SeqCst) {
        std::thread::sleep(TICK);
        let now = Instant::now();

        // Periodic probe; entering Suspicion forces it sooner.
        let mut probed: Option<bool> = None;
        if now >= probe_due {
            probed = Some(probe_tap());
            probe_due = now + PROBE_INTERVAL;
        }

        match &mut state {
            State::Silent => {
                if probed == Some(true) && tap.install() {
                    cycles = 0;
                    stale_since = None;
                    state = State::Running;
                    announce(&status, &status_cb, &mut current_status, Status::Running);
                    continue;
                }
                // Stale-permission hint: AX claims trusted but the functional
                // probe keeps failing (classic post-update csreq mismatch).
                let stale_candidate = ax_hint() && probed == Some(false);
                match (&mut stale_since, stale_candidate) {
                    (s @ None, true) => *s = Some(now),
                    (s, true) => {
                        if s.unwrap().elapsed() > STALE_HINT_AFTER {
                            announce(&status, &status_cb, &mut current_status, Status::StalePermission);
                        }
                    }
                    (s @ Some(_), false) => *s = None,
                    (None, false) => {}
                }
                if current_status != Status::StalePermission {
                    announce(&status, &status_cb, &mut current_status, Status::Silent);
                }
            }

            State::Running => {
                let gap = shared.heartbeat_gap();
                if shared.take_user_input_disabled() {
                    log::warn!("keyvent: tap disabled by user input; entering suspicion");
                    state = State::Suspicion {
                        entered: now,
                        rebuilt: false,
                        acked_at_entry: shared.hb_acked(),
                    };
                    probe_due = now; // discriminate immediately
                } else if gap >= HB_LOSS_GAP {
                    log::warn!("keyvent: heartbeat gap {gap}; entering suspicion");
                    state = State::Suspicion {
                        entered: now,
                        rebuilt: false,
                        acked_at_entry: shared.hb_acked(),
                    };
                    probe_due = now;
                }
            }

            State::Suspicion {
                entered,
                rebuilt,
                acked_at_entry,
            } => {
                // Recovery check: any new echo since entry means we're healing.
                if shared.hb_acked() > *acked_at_entry {
                    log::info!("keyvent: heartbeat recovered; back to running");
                    cycles = 0;
                    state = State::Running;
                    continue;
                }
                match probed {
                    Some(false) => {
                        // Revocation confirmed: tear down immediately. force_invalidate
                        // works even if the tap thread is stuck in a user callback.
                        log::warn!("keyvent: permission revoked (probe NULL); tearing down tap");
                        shared.force_invalidate();
                        tap.teardown();
                        cycles = 0;
                        state = State::Silent;
                        announce(&status, &status_cb, &mut current_status, Status::Silent);
                    }
                    Some(true) => {
                        if secure_input_active() {
                            // Password field / lock screen: events bypass taps by
                            // design. Wait it out; do NOT escalate.
                            continue;
                        }
                        if !*rebuilt {
                            // Transient death (sleep/wake, inert tap): rebuild once.
                            log::info!("keyvent: probe healthy but heartbeat lost; rebuilding tap");
                            tap.teardown();
                            if tap.install() {
                                *rebuilt = true;
                                *acked_at_entry = shared.hb_acked();
                                *entered = now;
                            } else {
                                cycles += 1;
                                if cycles >= MAX_RECOVERY_CYCLES {
                                    state = State::Cooldown {
                                        until: now + COOLDOWN_DURATION,
                                    };
                                    announce(&status, &status_cb, &mut current_status, Status::Cooldown);
                                }
                            }
                        } else if entered.elapsed() > SUSPICION_GRACE {
                            // Rebuild didn't restore the echo: count a cycle, retry.
                            cycles += 1;
                            log::warn!("keyvent: rebuild did not restore heartbeat (cycle {cycles})");
                            if cycles >= MAX_RECOVERY_CYCLES {
                                state = State::Cooldown {
                                    until: now + COOLDOWN_DURATION,
                                };
                                announce(&status, &status_cb, &mut current_status, Status::Cooldown);
                            } else {
                                *rebuilt = false;
                                *entered = now;
                            }
                        }
                    }
                    None => {}
                }
            }

            State::Cooldown { until } => {
                // Permission loss ends the cooldown into a clean Silent; otherwise
                // wait it out (a healthy probe must NOT cut the cooldown short,
                // since transient-failure cooldowns happen with a healthy probe).
                if now >= *until || probed == Some(false) {
                    log::info!("keyvent: cooldown over; resuming normal operation");
                    cycles = 0;
                    state = State::Silent;
                    announce(&status, &status_cb, &mut current_status, Status::Silent);
                }
            }
        }
    }

    tap.shutdown();
    log::debug!("keyvent: supervisor exited");
}
