//! `HotkeyManager`: the high-level entry point. Runs its matching engine as
//! one callback in the shared keyvent engine; consumers receive `HotkeyEvent`s
//! over a channel — no user code ever runs on the tap thread.

use std::collections::HashSet;
use std::sync::{Mutex, mpsc};

use crate::{Guard, Status};

use super::engine::{Matcher, Registered};
use super::{Hotkey, HotkeyEvent, HotkeyId, Suppression};

/// A registration change sent to the matcher on the tap thread.
enum Command {
    Register(HotkeyId, Registered),
    Unregister(HotkeyId),
    SetEnabled(bool),
}

/// User-thread registration state: id allocation, the live id set (for
/// `unregister`'s return value), and the enabled flag. The tap thread never
/// touches it.
struct Registry {
    next_id: u32,
    live: HashSet<HotkeyId>,
    enabled: bool,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            next_id: 0,
            live: HashSet::new(),
            enabled: true,
        }
    }
}

/// Registers hotkeys and delivers their activations over a channel.
///
/// Managers share the process-wide keyvent engine (see [`crate::start`]);
/// each manager's registrations and events are routed independently, and
/// [`HotkeyId`]s only need to be unique per manager.
pub struct HotkeyManager {
    commands: mpsc::Sender<Command>,
    registry: Mutex<Registry>,
    rx: mpsc::Receiver<HotkeyEvent>,
    engine: Guard,
}

impl HotkeyManager {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let mut matcher = Matcher::default();
        let engine = crate::start(move |event| {
            // Pending registrations are applied before the match, so they
            // take effect on the very next event — with no locking on the
            // tap thread, per the callback contract.
            while let Ok(command) = cmd_rx.try_recv() {
                match command {
                    Command::Register(id, reg) => matcher.register(id, reg),
                    Command::Unregister(id) => matcher.unregister(id),
                    Command::SetEnabled(enabled) => matcher.set_enabled(enabled),
                }
            }
            let (events, swallow) = matcher.handle(&event);
            for e in events {
                let _ = tx.send(e);
            }
            swallow
        });
        Self {
            commands: cmd_tx,
            registry: Mutex::new(Registry::default()),
            rx,
            engine,
        }
    }

    /// Register a hotkey. Ids are unique per manager and never reused.
    pub fn register(&self, hotkey: Hotkey, suppression: Suppression) -> HotkeyId {
        // The send stays inside the lock so the channel order always matches
        // the live-set order, even with concurrent register/unregister calls.
        let mut registry = self.registry.lock().unwrap();
        let id = HotkeyId(registry.next_id);
        registry.next_id += 1;
        registry.live.insert(id);
        let _ = self.commands.send(Command::Register(
            id,
            Registered {
                hotkey,
                suppression,
            },
        ));
        id
    }

    /// Remove a registration. Returns false if the id is unknown. An active
    /// hotkey that is unregistered simply never fires Up.
    pub fn unregister(&self, id: HotkeyId) -> bool {
        let mut registry = self.registry.lock().unwrap();
        if registry.live.remove(&id) {
            let _ = self.commands.send(Command::Unregister(id));
            true
        } else {
            false
        }
    }

    /// Suspend matching: no new activations start and nothing is swallowed.
    /// A hotkey already active still fires Up when released.
    pub fn disable(&self) {
        self.set_enabled(false);
    }

    /// Resume matching after [`disable`](Self::disable).
    pub fn enable(&self) {
        self.set_enabled(true);
    }

    /// Whether the manager is currently matching.
    pub fn enabled(&self) -> bool {
        self.registry.lock().unwrap().enabled
    }

    fn set_enabled(&self, enabled: bool) {
        // The send stays inside the lock so the channel order always matches
        // the flag order, even with concurrent enable/disable calls.
        let mut registry = self.registry.lock().unwrap();
        registry.enabled = enabled;
        let _ = self.commands.send(Command::SetEnabled(enabled));
    }

    /// Channel of hotkey activations. Use `recv` to block or `try_recv` to
    /// poll.
    pub fn receiver(&self) -> &mpsc::Receiver<HotkeyEvent> {
        &self.rx
    }

    /// Permission lifecycle status of the underlying engine.
    pub fn status(&self) -> Status {
        self.engine.status()
    }

    /// Stream of engine status changes.
    pub fn status_receiver(&self) -> &mpsc::Receiver<Status> {
        self.engine.status_receiver()
    }
}

impl Default for HotkeyManager {
    fn default() -> Self {
        Self::new()
    }
}
