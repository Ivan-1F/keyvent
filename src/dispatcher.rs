//! All callbacks registered through `start` share one event tap. The
//! dispatcher provides the tap's single callback: it delivers each event to
//! every registered callback and returns whether any of them chose to
//! suppress it.

use crate::KeyEvent;
use crate::tap::BoxedCallback;
use std::sync::mpsc;

/// A registration change sent to the dispatcher on the tap thread.
pub(crate) enum DispatcherCommand {
    Add(u64, BoxedCallback),
    Remove(u64),
}

/// The tap thread's single callback: fans each event out to every registered
/// callback and ORs the swallow decisions. Commands are applied before the
/// event, so (un)registration takes effect on the very next event — with no
/// locking on the tap thread, per the callback contract.
pub(crate) struct Dispatcher {
    pub(crate) callbacks: Vec<(u64, BoxedCallback)>,
    pub(crate) cmd_rx: mpsc::Receiver<DispatcherCommand>,
}

impl Dispatcher {
    pub(crate) fn handle(&mut self, event: KeyEvent) -> bool {
        while let Ok(command) = self.cmd_rx.try_recv() {
            match command {
                DispatcherCommand::Add(id, callback) => self.callbacks.push((id, callback)),
                DispatcherCommand::Remove(id) => self.callbacks.retain(|(cid, _)| *cid != id),
            }
        }
        let mut suppress = false;
        for (_, callback) in &mut self.callbacks {
            // Each callback is panic-isolated: one panicking callback must not
            // starve the others of this (or any later) event.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(event))) {
                Ok(true) => suppress = true,
                Ok(false) => {}
                Err(_) => log::error!("keyvent: user callback panicked; treated as pass-through"),
            }
        }
        suppress
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Key, KeyEventKind, Modifiers};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::SeqCst;

    fn dispatcher() -> Dispatcher {
        let (_tx, rx) = mpsc::channel();
        Dispatcher {
            callbacks: Vec::new(),
            cmd_rx: rx,
        }
    }

    fn event() -> KeyEvent {
        KeyEvent {
            key: Key::KeyA,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::NONE,
            is_repeat: false,
        }
    }

    #[test]
    fn dispatcher_ors_swallow_decisions() {
        let mut d = dispatcher();
        d.callbacks.push((0, Box::new(|_| false)));
        d.callbacks.push((1, Box::new(|_| true)));
        d.callbacks.push((2, Box::new(|_| false)));
        assert!(d.handle(event()));
    }

    #[test]
    fn dispatcher_applies_commands_before_events() {
        let (tx, rx) = mpsc::channel();
        let mut d = Dispatcher {
            callbacks: Vec::new(),
            cmd_rx: rx,
        };
        tx.send(DispatcherCommand::Add(0, Box::new(|_| true)))
            .unwrap();
        assert!(d.handle(event()));
        tx.send(DispatcherCommand::Remove(0)).unwrap();
        assert!(!d.handle(event()));
    }

    #[test]
    fn dispatcher_isolates_callback_panics() {
        let mut d = dispatcher();
        let called = Arc::new(AtomicBool::new(false));
        let called2 = called.clone();
        d.callbacks.push((0, Box::new(|_| panic!("boom"))));
        d.callbacks.push((
            1,
            Box::new(move |_| {
                called2.store(true, SeqCst);
                false
            }),
        ));
        // The panic counts as pass-through, and later callbacks still ran.
        assert!(!d.handle(event()));
        assert!(called.load(SeqCst));
    }
}
