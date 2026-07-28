//! Consumer-stream pairing ledger: guarantees the callback sees a strictly
//! alternating Press/Release sequence per key, and that every release it sees
//! is backed by physical truth.
//!
//! Why this exists: the OS event stream can start mid-hold (tap installed
//! while keys are down) and can lose events (tap disabled windows, permission
//! gaps). Without a ledger the callback would see orphan releases — or, worse,
//! dangling presses that never close. The rules:
//!
//! 1. **Orphan releases are dropped** (consumer side only; the OS pass-through
//!    is a separate concern handled by `suppression`).
//! 2. **A re-press without an intervening release first emits a synthetic
//!    Release** — the stream self-heals through natural use.
//! 3. **Reconcile at recovery and at rebuild** (tap timeout recovery, tap
//!    install): dangling presses whose key is physically up are closed with
//!    synthetic releases; keys still held stay open and close on their real
//!    release. Nothing is closed at teardown: the physical query is only
//!    trustworthy while permission is present, and closing a held key would
//!    fabricate a release the user never made.
//!
//! Synthetic events are only ever Releases completing an already-delivered
//! Press. Presses are never fabricated: fabricating a Press could trigger
//! consumer functionality for an action the user never took.
//!
//! Key-repeat presses bypass the ledger entirely (they are delivered but do
//! not affect pairing).

use std::collections::HashSet;

use crate::platform::Keycode;

/// What the tap layer should do with a converted event on the consumer side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Deliver the event as-is.
    Deliver,
    /// Emit a synthetic Release for this key first, then deliver the event.
    CloseThenDeliver,
    /// Do not deliver this event to the consumer (orphan release).
    Drop,
}

#[derive(Default)]
pub(crate) struct Pairing {
    /// Keys with a Press delivered to the consumer and no Release yet.
    pressed: HashSet<Keycode>,
}

impl Pairing {
    /// Book a press. Repeats neither affect nor consult the ledger.
    pub(crate) fn press(&mut self, code: Keycode, is_repeat: bool) -> Plan {
        if is_repeat {
            return Plan::Deliver;
        }
        if self.pressed.contains(&code) {
            // A release was lost somewhere (disabled window, crash of the
            // event source): close the dangling press, then deliver.
            return Plan::CloseThenDeliver;
        }
        self.pressed.insert(code);
        Plan::Deliver
    }

    /// Book a release. Orphans (no delivered press) are dropped.
    pub(crate) fn release(&mut self, code: Keycode) -> Plan {
        if self.pressed.remove(&code) {
            Plan::Deliver
        } else {
            Plan::Drop
        }
    }

    /// Close dangling presses whose key is physically up; returns their
    /// keycodes for synthetic releases. Keys still held stay open.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn reconcile(&mut self, is_held: impl Fn(Keycode) -> bool) -> Vec<Keycode> {
        let released: Vec<Keycode> = self
            .pressed
            .iter()
            .filter(|code| !is_held(**code))
            .copied()
            .collect();
        for code in &released {
            self.pressed.remove(code);
        }
        released
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_pairing() {
        let mut p = Pairing::default();
        assert_eq!(p.press(36, false), Plan::Deliver);
        assert_eq!(p.release(36), Plan::Deliver);
        // After a clean pair, another release is an orphan.
        assert_eq!(p.release(36), Plan::Drop);
    }

    #[test]
    fn orphan_release_is_dropped() {
        let mut p = Pairing::default();
        // Stream started mid-hold: first event is a release.
        assert_eq!(p.release(49), Plan::Drop);
        // Next real press starts a fresh pair.
        assert_eq!(p.press(49, false), Plan::Deliver);
        assert_eq!(p.release(49), Plan::Deliver);
    }

    #[test]
    fn repress_closes_dangling_press_first() {
        let mut p = Pairing::default();
        assert_eq!(p.press(36, false), Plan::Deliver);
        // Release lost in a disabled window; the user presses again.
        assert_eq!(p.press(36, false), Plan::CloseThenDeliver);
        // Ledger still holds one open press; the real release closes it.
        assert_eq!(p.release(36), Plan::Deliver);
    }

    #[test]
    fn repeats_bypass_ledger() {
        let mut p = Pairing::default();
        assert_eq!(p.press(36, false), Plan::Deliver);
        assert_eq!(p.press(36, true), Plan::Deliver); // repeat: no CloseThenDeliver
        assert_eq!(p.press(36, true), Plan::Deliver);
        assert_eq!(p.release(36), Plan::Deliver);
    }

    #[test]
    fn reconcile_closes_only_physically_released() {
        let mut p = Pairing::default();
        p.press(36, false);
        p.press(56, false);
        let released = p.reconcile(|code| code == 56); // 36 up, 56 held
        assert_eq!(released, vec![36]);
        // 56 stays open and closes naturally later.
        assert_eq!(p.release(56), Plan::Deliver);
    }
}
