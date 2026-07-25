# keyvent

Global keyboard event stream for macOS, with automatic Accessibility permission lifecycle management.

## Motivation

macOS event taps are not create-once resources.
The system disables them when the callback is slow, and stops delivering events when Accessibility permission is revoked at runtime — without reliably notifying the process.
A tap left installed after revocation can block event delivery to the entire session; apps built on the common hotkey libraries have frozen whole machines this way ([example incident](https://github.com/cjpais/Handy/issues/1281)).

The status APIs don't help: `AXIsProcessTrusted()` can return stale results for a running process, and the tap-disabled notification does not reliably fire.
keyvent instead detects permission state through functional probes, tears the tap down proactively on revocation, and rebuilds it when permission returns — all without application code.

## Features

- `Press`/`Release` events for all keys, with side-specific modifier keys and Fn/Globe support.
- Optional per-event suppression, with consistency enforced on the OS side (a suppressed press always suppresses its release, and vice versa for passed events).
- A paired event stream: per key, events strictly alternate `Press`/`Release`, including across permission changes and tap rebuilds.
- A keyboard-only event mask: pointer events never pass through the tap.
- Automatic handling of permission grant, revocation, and re-grant; recovery from tap timeouts, sleep/wake, and secure-input interruptions.

## Install

```toml
[dependencies]
keyvent = "0.1"
```

## Usage

```rust
let guard = keyvent::start(|event| {
    println!("{:?} {:?} {:?}", event.kind, event.key, event.modifiers);
    false // return true to suppress the event
});
```

`start` returns immediately in all cases, including when permission is missing.
Drop the returned `Guard` to stop.

Suppression (returning `true`) is only consulted for presses; releases always follow the press decision.

To observe the permission lifecycle:

```rust
std::thread::spawn(move || {
    while let Ok(status) = guard.status_receiver().recv() {
        // Status::Running | Status::Silent | Status::Cooldown
    }
});
```

## Callback contract

The callback runs on the event-tap thread, on the delivery path of every keystroke in the session.
It must return quickly: no blocking, no waiting on contended locks, no synchronous cross-thread calls.
Move work to another thread (e.g. by sending the event over a channel).

## Permissions

Filtering keyboard events requires the **Accessibility** permission (System Settings → Privacy & Security → Accessibility).
Without it, `start` stays silent; there is no error path to handle.
