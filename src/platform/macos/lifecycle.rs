//! When to install, check and tear down the tap; free of threads and OS calls, so tests run it on a fake clock.
//!
//! Only tap_create tells whether we may filter keyboard events: in-process permission queries cache stale answers.
//! A refused tap_create leaks a Mach port pair inside the OS, so asking must be rare while the answer is likely no.
//!
//! The policy:
//! - Running checks once per second, and tears the tap down if the check fails.
//! - Silent tries to install only for a reason, one `SETTLE` after it: startup, a notification, leaving Running, or a failure unrelated to the permission.
//! - After a refusal, nothing is tried again until a notification arrives.
//!
//! Leaving Running always earns one retry, because a grant notification arriving in Running is ignored.
//! So leaks scale with permission changes, never with uptime.

use std::time::{Duration, Instant};

use crate::Status;

use super::stream::InstallOutcome;

const PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// Wait before any install attempt.
/// tccd notifies before WindowServer applies a change (at most 47ms later in tests), so a shorter wait could miss a grant.
/// Each notification restarts the wait, so a burst or a flood yields at most one attempt.
const SETTLE: Duration = Duration::from_secs(1);

pub(super) trait Platform {
    /// Denied leaks a Mach port pair.
    fn install(&mut self) -> InstallOutcome;
    /// A refusal leaks a Mach port pair.
    fn probe(&mut self) -> bool;
    fn tap_enabled(&self) -> bool;
    /// Must work even while the tap thread is stuck in a user callback.
    fn teardown(&mut self);
    fn publish(&mut self, status: Status);
}

#[derive(Debug)]
enum State {
    Silent {
        /// None: wait for a notification, with no wakeups at all.
        attempt_at: Option<Instant>,
    },
    Running {
        next_check: Instant,
    },
}

#[derive(Debug)]
pub(super) struct Lifecycle {
    state: State,
}

impl Lifecycle {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            state: State::Silent {
                attempt_at: Some(now),
            },
        }
    }

    /// When [`advance`](Self::advance) next has work to do; None if only a notification can change anything.
    pub(super) fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Silent { attempt_at } => attempt_at,
            State::Running { next_check } => Some(next_check),
        }
    }

    /// TCC posted an Accessibility change for some app; Running checks on its own and ignores it.
    pub(super) fn notified(&mut self, now: Instant) {
        if let State::Silent { attempt_at } = &mut self.state {
            *attempt_at = Some(now + SETTLE);
        }
    }

    /// Deadlines set here count from `now` even if an install blocks for a while, so they can only come early.
    pub(super) fn advance(&mut self, now: Instant, platform: &mut impl Platform) {
        match self.state {
            State::Silent { attempt_at } if attempt_at.is_some_and(|at| now >= at) => {
                self.state = match platform.install() {
                    InstallOutcome::Installed => {
                        platform.publish(Status::Running);
                        State::Running {
                            next_check: now + PROBE_INTERVAL,
                        }
                    }
                    InstallOutcome::Denied => {
                        log::debug!(
                            "keyvent: no Accessibility permission; waiting for a TCC change"
                        );
                        State::Silent { attempt_at: None }
                    }
                    InstallOutcome::Failed => {
                        log::warn!("keyvent: tap install failed; retrying");
                        State::Silent {
                            attempt_at: Some(now + SETTLE),
                        }
                    }
                };
            }
            State::Running { next_check } if now >= next_check => {
                // A disabled tap is checked first, because that check is free.
                let failure = if !platform.tap_enabled() {
                    Some("tap disabled")
                } else if !platform.probe() {
                    Some("Accessibility permission lost")
                } else {
                    None
                };
                self.state = match failure {
                    None => State::Running {
                        next_check: now + PROBE_INTERVAL,
                    },
                    Some(reason) => {
                        log::warn!("keyvent: {reason}; tearing down tap");
                        platform.teardown();
                        platform.publish(Status::Silent);
                        State::Silent {
                            attempt_at: Some(now + SETTLE),
                        }
                    }
                };
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);
    const HOUR: Duration = Duration::from_secs(3600);

    /// WindowServer, the tap thread and the status broadcast, simulated on a fake clock.
    struct Fake {
        now: Instant,
        /// The TCC record, which notifications announce.
        permission: bool,
        /// When WindowServer starts reflecting `permission`; before that it still reports the opposite.
        effective_at: Instant,
        tap_enabled: bool,
        fail_next_install: bool,
        installed: bool,
        tap_creates: u32,
        refusals: u32,
        status: Status,
    }

    impl Fake {
        fn granted(&self) -> bool {
            if self.now >= self.effective_at {
                self.permission
            } else {
                !self.permission
            }
        }

        fn create(&mut self) -> bool {
            self.tap_creates += 1;
            let granted = self.granted();
            if !granted {
                self.refusals += 1;
            }
            granted
        }
    }

    impl Platform for Fake {
        fn install(&mut self) -> InstallOutcome {
            if std::mem::take(&mut self.fail_next_install) {
                return InstallOutcome::Failed;
            }
            if !self.create() {
                return InstallOutcome::Denied;
            }
            self.installed = true;
            self.tap_enabled = true;
            InstallOutcome::Installed
        }

        fn probe(&mut self) -> bool {
            self.create()
        }

        fn tap_enabled(&self) -> bool {
            self.tap_enabled
        }

        fn teardown(&mut self) {
            self.installed = false;
        }

        fn publish(&mut self, status: Status) {
            self.status = status;
        }
    }

    /// The machine plus its fake world, driven the way the supervisor drives the real one.
    struct Sim {
        machine: Lifecycle,
        os: Fake,
        start: Instant,
    }

    impl Sim {
        /// Start the machine with or without the permission, and run its startup attempt.
        fn start(permission: bool) -> Self {
            let start = Instant::now();
            let mut sim = Self {
                machine: Lifecycle::new(start),
                os: Fake {
                    now: start,
                    permission,
                    effective_at: start,
                    tap_enabled: true,
                    fail_next_install: false,
                    installed: false,
                    tap_creates: 0,
                    refusals: 0,
                    status: Status::Silent,
                },
                start,
            };
            sim.machine.advance(start, &mut sim.os);
            sim
        }

        /// Let `duration` pass, firing every deadline on the way.
        fn wait(&mut self, duration: Duration) {
            let end = self.os.now + duration;
            while let Some(at) = self.machine.deadline().filter(|&at| at <= end) {
                // A deadline in the past fires now, as recv_deadline would.
                self.os.now = self.os.now.max(at);
                self.machine.advance(self.os.now, &mut self.os);
            }
            self.os.now = end;
        }

        fn notify(&mut self) {
            self.machine.notified(self.os.now);
            self.machine.advance(self.os.now, &mut self.os);
        }

        /// Flip the TCC record; tccd notifies, and WindowServer follows after `lag`.
        fn toggle_permission(&mut self, lag: Duration) {
            self.os.permission = !self.os.permission;
            self.os.effective_at = self.os.now + lag;
            self.notify();
        }

        fn elapsed(&self) -> Duration {
            self.os.now - self.start
        }
    }

    #[test]
    fn starts_running_when_granted() {
        let sim = Sim::start(true);
        assert_eq!(sim.os.status, Status::Running);
        assert_eq!(sim.os.refusals, 0);
    }

    #[test]
    fn denied_at_start_never_asks_again_without_a_notification() {
        let mut sim = Sim::start(false);
        assert_eq!(sim.os.status, Status::Silent);
        assert_eq!(sim.machine.deadline(), None);
        sim.wait(HOUR);
        assert_eq!(sim.os.tap_creates, 1);
    }

    #[test]
    fn grant_while_silent_installs_after_settle() {
        let mut sim = Sim::start(false);
        sim.toggle_permission(47 * MS);
        sim.wait(SETTLE - MS);
        assert_eq!(sim.os.status, Status::Silent);
        sim.wait(MS);
        assert_eq!(sim.os.status, Status::Running);
        assert_eq!(sim.os.refusals, 1);
    }

    #[test]
    fn unrelated_notification_costs_one_refusal() {
        let mut sim = Sim::start(false);
        sim.notify();
        sim.wait(HOUR);
        assert_eq!(sim.os.status, Status::Silent);
        assert_eq!(sim.os.refusals, 2);
    }

    #[test]
    fn notification_burst_coalesces_into_one_attempt() {
        let mut sim = Sim::start(false);
        for _ in 0..10 {
            sim.notify();
            sim.wait(10 * MS);
        }
        sim.wait(HOUR);
        assert_eq!(sim.os.tap_creates, 2);
    }

    #[test]
    fn notification_flood_defers_every_attempt() {
        let mut sim = Sim::start(false);
        for _ in 0..1000 {
            sim.notify();
            sim.wait(SETTLE / 2);
        }
        assert_eq!(sim.os.tap_creates, 1);
        sim.wait(SETTLE);
        assert_eq!(sim.os.tap_creates, 2);
    }

    #[test]
    fn running_ignores_notifications() {
        let mut sim = Sim::start(true);
        for _ in 0..100 {
            sim.notify();
        }
        sim.wait(10 * PROBE_INTERVAL);
        // One install plus ten probes, unaffected by the notifications.
        assert_eq!(sim.os.tap_creates, 11);
        assert_eq!(sim.os.refusals, 0);
    }

    #[test]
    fn revocation_tears_down_within_one_probe_interval_then_retries_once() {
        let mut sim = Sim::start(true);
        sim.wait(300 * MS);
        sim.toggle_permission(20 * MS);
        sim.wait(PROBE_INTERVAL);
        assert_eq!(sim.os.status, Status::Silent);
        assert!(!sim.os.installed);
        assert_eq!(sim.os.refusals, 1);
        sim.wait(HOUR);
        assert_eq!(sim.os.refusals, 2);
    }

    /// A grant that races a refused probe must still be picked up.
    #[test]
    fn regrant_racing_a_refused_probe_still_installs() {
        let mut sim = Sim::start(true);
        sim.wait(100 * MS);
        // Revoked, with WindowServer applying it at once; the probe at 1s has not run yet.
        sim.toggle_permission(Duration::ZERO);
        sim.wait(800 * MS);
        // Granted again, but WindowServer lags, so the probe at 1s is still refused.
        sim.toggle_permission(300 * MS);
        sim.wait(200 * MS);
        assert_eq!(sim.os.status, Status::Silent);
        sim.wait(SETTLE);
        assert_eq!(sim.os.status, Status::Running);
    }

    #[test]
    fn grant_arriving_after_teardown_installs() {
        let mut sim = Sim::start(true);
        sim.toggle_permission(Duration::ZERO);
        sim.wait(PROBE_INTERVAL);
        assert_eq!(sim.os.status, Status::Silent);
        sim.toggle_permission(47 * MS);
        sim.wait(SETTLE);
        assert_eq!(sim.os.status, Status::Running);
    }

    #[test]
    fn disabled_tap_is_reinstalled_when_permission_remains() {
        let mut sim = Sim::start(true);
        sim.os.tap_enabled = false;
        sim.wait(PROBE_INTERVAL);
        assert_eq!(sim.os.status, Status::Silent);
        assert!(!sim.os.installed);
        sim.wait(SETTLE);
        assert_eq!(sim.os.status, Status::Running);
        assert_eq!(sim.os.refusals, 0);
    }

    #[test]
    fn failed_install_retries_without_a_notification() {
        let mut sim = Sim::start(false);
        sim.os.permission = true;
        sim.os.fail_next_install = true;
        sim.notify();
        sim.wait(SETTLE);
        assert_eq!(sim.os.status, Status::Silent);
        sim.wait(SETTLE);
        assert_eq!(sim.os.status, Status::Running);
    }

    /// A tiny deterministic generator, so the randomized test needs no dependency.
    struct XorShift(u64);

    impl XorShift {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    /// Let everything settle, then check that the status matches what WindowServer allows.
    fn assert_settled(sim: &mut Sim, seed: u64) {
        sim.os.tap_enabled = true;
        sim.wait(Duration::from_secs(5));
        let expected = if sim.os.granted() {
            Status::Running
        } else {
            Status::Silent
        };
        assert_eq!(
            sim.os.status,
            expected,
            "seed {seed}: liveness at {:?}",
            sim.elapsed()
        );
    }

    /// Random permission changes, notifications, tap disables and quiet periods, checked against three properties.
    /// Liveness: after every quiet period, the status matches what WindowServer allows.
    /// Leak bound: refusals never exceed startup, plus one per notification, plus two per Running stint.
    /// Quiet: with no inputs and no permission, nothing is ever asked.
    #[test]
    fn randomized_histories_keep_the_invariants() {
        for seed in 1..=500u64 {
            let mut rng = XorShift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut sim = Sim::start(rng.below(2) == 0);
            let mut notifications = 0u32;
            let mut stints = u32::from(sim.os.status == Status::Running);
            for _ in 0..300 {
                let was_running = sim.os.status == Status::Running;
                match rng.below(10) {
                    0..=2 if sim.os.now >= sim.os.effective_at => {
                        sim.toggle_permission(Duration::from_millis(rng.below(301)));
                        notifications += 1;
                    }
                    3 => {
                        sim.notify();
                        notifications += 1;
                    }
                    4 => sim.os.tap_enabled = false,
                    5 => sim.os.tap_enabled = true,
                    6 => assert_settled(&mut sim, seed),
                    // Short gaps matter most: they create the races between probes, lags and notifications.
                    7 | 8 => sim.wait(Duration::from_millis(rng.below(400))),
                    _ => sim.wait(Duration::from_millis(rng.below(3000))),
                }
                if !was_running && sim.os.status == Status::Running {
                    stints += 1;
                }
                // Stints are counted as they start, so a stint that ends later is already counted.
                let budget = 1 + notifications + 2 * stints;
                assert!(
                    sim.os.refusals <= budget,
                    "seed {seed}: {} refusals over a budget of {budget} after {:?}",
                    sim.os.refusals,
                    sim.elapsed()
                );
            }

            assert_settled(&mut sim, seed);
            if sim.os.status == Status::Silent {
                let asked = sim.os.tap_creates;
                sim.wait(HOUR);
                assert_eq!(sim.os.tap_creates, asked, "seed {seed}: asked while quiet");
            }
        }
    }
}
