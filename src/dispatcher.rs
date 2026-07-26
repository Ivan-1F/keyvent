//! All callbacks registered through `start` share one event tap. The
//! dispatcher provides the tap's single callback: it delivers each event to
//! every registered callback and returns whether any of them chose to
//! suppress it.

use crate::KeyEvent;
use crate::tap::BoxedCallback;
use std::sync::Weak;
use std::sync::mpsc;

pub(crate) enum DispatcherCommand {
    Add(BoxedCallback, Weak<()>),
}

pub(crate) struct Dispatcher {
    callbacks: Vec<(Weak<()>, BoxedCallback)>,
    cmd_rx: mpsc::Receiver<DispatcherCommand>,
}

impl Dispatcher {
    pub(crate) fn new() -> (mpsc::Sender<DispatcherCommand>, Self) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        (
            cmd_tx,
            Self {
                callbacks: Vec::new(),
                cmd_rx,
            },
        )
    }

    pub(crate) fn handle(&mut self, event: KeyEvent) -> bool {
        while let Ok(DispatcherCommand::Add(callback, token)) = self.cmd_rx.try_recv() {
            self.callbacks.push((token, callback));
        }
        // A registration lives as long as its guard: prune dead ones.
        self.callbacks.retain(|(token, _)| token.strong_count() > 0);
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

    fn event() -> KeyEvent {
        KeyEvent {
            key: Key::KeyA,
            kind: KeyEventKind::Press,
            modifiers: Modifiers::NONE,
            is_repeat: false,
        }
    }

    fn add(tx: &mpsc::Sender<DispatcherCommand>, callback: BoxedCallback) -> Arc<()> {
        let token = Arc::new(());
        let _ = tx.send(DispatcherCommand::Add(callback, Arc::downgrade(&token)));
        token
    }

    #[test]
    fn dispatcher_ors_suppress_decisions() {
        let (tx, mut d) = Dispatcher::new();
        let _t0 = add(&tx, Box::new(|_| false));
        let _t1 = add(&tx, Box::new(|_| true));
        let _t2 = add(&tx, Box::new(|_| false));
        assert!(d.handle(event()));
    }

    #[test]
    fn dispatcher_prunes_dropped_registrations() {
        let (tx, mut d) = Dispatcher::new();
        let token = add(&tx, Box::new(|_| true));
        assert!(d.handle(event()));
        drop(token);
        assert!(!d.handle(event()));
    }

    #[test]
    fn dispatcher_isolates_callback_panics() {
        let (tx, mut d) = Dispatcher::new();
        let called = Arc::new(AtomicBool::new(false));
        let called2 = called.clone();
        let _t0 = add(&tx, Box::new(|_| panic!("boom")));
        let _t1 = add(
            &tx,
            Box::new(move |_| {
                called2.store(true, SeqCst);
                false
            }),
        );
        // The panic counts as pass-through, and later callbacks still ran.
        assert!(!d.handle(event()));
        assert!(called.load(SeqCst));
    }
}
