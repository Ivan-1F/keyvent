//! The supervisor thread: owns the permission-lifecycle state machine.
//!
//! Detection philosophy: never trust `AXIsProcessTrusted` or tap-disabled
//! notifications — probe functionally, and only ever *observe* (keyvent
//! injects nothing into the OS event stream):
//!
//! - **tap_create probe** (every 1s): ground truth for "can this process filter
//!   keyboard events right now". NULL = no. Drives both revocation (teardown)
//!   and re-grant (rebuild).
//! - **tap_is_enabled** (every tick): detects a watchdog-disabled or otherwise
//!   dead tap that the disabled-notification path missed.
//! - **kCGEventTapDisabledByUserInput**: administrative disable hint (unreliable
//!   on revocation, free to act on when it does arrive).
//!
//! All automated recovery is budgeted (Cooldown); the only unbudgeted action is
//! *tearing down*, which always moves the system toward safety.

use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use objc2_core_foundation::CFMachPort;
use objc2_core_graphics::{
    CGEvent, CGEventTapCallBack, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType,
};

use crate::Status;
use crate::status::StatusBroadcast;

use super::{TapShared, TapThread};

const TICK: Duration = Duration::from_millis(200);
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// How long the tap may stay disabled before we treat it as dead and rebuild.
/// The callback re-enables instantly on ByTimeout, so anything past ~1s means
/// re-enable failed or the notification never came.
const TAP_DISABLED_GRACE: Duration = Duration::from_secs(1);
/// Failed recovery cycles before Cooldown.
const MAX_RECOVERY_CYCLES: u32 = 3;
const COOLDOWN_DURATION: Duration = Duration::from_secs(30);
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Create + immediately invalidate a throwaway tap: the functional permission
/// probe.
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
    /// Tap installed and delivering events.
    Running,
    /// Something looks wrong (probe NULL, tap disabled, user-input disable);
    /// the probe discriminates revocation from transient death.
    Suspicion { rebuilt: bool },
    /// Recovery budget exhausted; doing nothing but probing.
    Cooldown { until: Instant },
}

pub(crate) struct Supervisor {
    shutdown: Arc<AtomicBool>,
    shared: Arc<TapShared>,
    join: Option<JoinHandle<()>>,
}

impl Supervisor {
    pub(crate) fn spawn(
        tap: TapThread,
        shared: Arc<TapShared>,
        broadcast: Arc<StatusBroadcast>,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown2 = shutdown.clone();
        let shared2 = shared.clone();
        let join = thread::Builder::new()
            .name("keyvent-supervisor".into())
            .spawn(move || supervise(tap, shared, broadcast, shutdown2))
            .expect("spawn keyvent-supervisor thread");
        Self {
            shutdown,
            shared: shared2,
            join: Some(join),
        }
    }

    pub(crate) fn shutdown(mut self) {
        self.shutdown.store(true, SeqCst);
        // Free system input immediately, even if the tap thread is stuck inside
        // a user callback and can never process the Shutdown command.
        self.shared.force_invalidate();
        if let Some(join) = self.join.take() {
            crate::platform::join_with_timeout(join, SHUTDOWN_JOIN_TIMEOUT, "supervisor");
        }
    }
}

fn supervise(
    tap: TapThread,
    shared: Arc<TapShared>,
    broadcast: Arc<StatusBroadcast>,
    shutdown: Arc<AtomicBool>,
) {
    let mut state = State::Silent;
    let mut probe_due = Instant::now(); // probe immediately on start
    let mut cycles = 0u32;
    // When the tap was first seen continuously disabled (None while enabled).
    let mut disabled_since: Option<Instant> = None;

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
                    disabled_since = None;
                    state = State::Running;
                    broadcast.publish(Status::Running);
                    continue;
                }
                broadcast.publish(Status::Silent);
            }

            State::Running => {
                // Watchdog-disabled detection: the callback re-enables instantly,
                // so a continuously disabled tap means re-enable failed or the
                // notification never arrived.
                let tap_dead = match shared.tap_is_enabled() {
                    Some(false) => {
                        let since = disabled_since.get_or_insert(now);
                        since.elapsed() >= TAP_DISABLED_GRACE
                    }
                    _ => {
                        disabled_since = None;
                        false
                    }
                };

                if shared.take_user_input_disabled() {
                    log::warn!("keyvent: tap disabled by user input; entering suspicion");
                    state = State::Suspicion { rebuilt: false };
                    probe_due = now; // discriminate immediately
                } else if probed == Some(false) {
                    // Ground truth says we cannot filter right now (revocation
                    // or stale permission). Suspicion re-probes to confirm
                    // before tearing down; a transient NULL heals as a rebuild.
                    log::warn!("keyvent: probe NULL while running; entering suspicion");
                    state = State::Suspicion { rebuilt: false };
                    probe_due = now;
                } else if tap_dead {
                    log::warn!("keyvent: tap continuously disabled; entering suspicion");
                    state = State::Suspicion { rebuilt: false };
                    probe_due = now;
                }
            }

            State::Suspicion { rebuilt } => {
                match probed {
                    Some(false) => {
                        // Revocation confirmed: tear down immediately. force_invalidate
                        // works even if the tap thread is stuck in a user callback.
                        log::warn!("keyvent: permission revoked (probe NULL); tearing down tap");
                        shared.force_invalidate();
                        tap.teardown();
                        cycles = 0;
                        disabled_since = None;
                        state = State::Silent;
                        broadcast.publish(Status::Silent);
                    }
                    Some(true) => {
                        if !*rebuilt {
                            // Transient death (disabled tap, sleep/wake, inert tap):
                            // rebuild once. install() returning true proves the new
                            // tap exists and is enabled; whether events actually flow
                            // is not directly observable, so we trust the rebuild and
                            // rely on the next probe cycle to catch a failure here.
                            log::info!("keyvent: probe healthy but tap unhealthy; rebuilding");
                            tap.teardown();
                            if tap.install() {
                                cycles = 0;
                                disabled_since = None;
                                state = State::Running;
                                broadcast.publish(Status::Running);
                            } else {
                                cycles += 1;
                                if cycles >= MAX_RECOVERY_CYCLES {
                                    state = State::Cooldown {
                                        until: now + COOLDOWN_DURATION,
                                    };
                                    broadcast.publish(Status::Cooldown);
                                }
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
                    disabled_since = None;
                    state = State::Silent;
                    broadcast.publish(Status::Silent);
                }
            }
        }
    }

    tap.shutdown();
    log::debug!("keyvent: supervisor exited");
}
