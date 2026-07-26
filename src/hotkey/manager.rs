//! `HotkeyManager`: the high-level entry point. Owns a keyvent engine whose
//! callback is the matching engine; consumers receive `HotkeyEvent`s over a
//! channel — no user code ever runs on the tap thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use crate::{Guard, Status};

use super::engine::{Matcher, Registered};
use super::{Hotkey, HotkeyEvent, HotkeyId, Suppression};

/// Registers hotkeys and delivers their activations over a channel.
///
/// Each manager owns an independent engine (tap thread + supervisor). Create
/// one per application.
pub struct HotkeyManager {
    registered: Arc<Mutex<HashMap<HotkeyId, Registered>>>,
    next_id: AtomicU32,
    rx: mpsc::Receiver<HotkeyEvent>,
    engine: Guard,
}

impl HotkeyManager {
    pub fn new() -> Self {
        let registered = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel();
        let registered2 = registered.clone();
        let mut matcher = Matcher::default();
        let engine = crate::start(move |event| {
            // The match itself is allocation-free and the lock is held only for
            // its duration; registration writes are rare and tiny.
            let (events, swallow) = {
                let guard = registered2.lock().unwrap();
                matcher.handle(&event, &guard)
            };
            for e in events {
                let _ = tx.send(e);
            }
            swallow
        });
        Self {
            registered,
            next_id: AtomicU32::new(0),
            rx,
            engine,
        }
    }

    /// Register a hotkey. Ids are unique per manager and never reused.
    pub fn register(&self, hotkey: Hotkey, suppression: Suppression) -> HotkeyId {
        let id = HotkeyId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.registered
            .lock()
            .unwrap()
            .insert(id, Registered { hotkey, suppression });
        id
    }

    /// Remove a registration. Returns false if the id is unknown. An active
    /// hotkey that is unregistered simply never fires Up.
    pub fn unregister(&self, id: HotkeyId) -> bool {
        self.registered.lock().unwrap().remove(&id).is_some()
    }

    /// Channel of hotkey activations. Use `recv` to block or `try_recv` to poll.
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
