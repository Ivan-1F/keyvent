# keyvent

Global keyboard events and hotkeys for macOS and Windows.

- **Hotkeys**: register `Super+K`, a lone `Fn`, or only the right `Alt`.
- **Recording**: let users set a hotkey by pressing it, like the shortcut field in an app's settings.
- **Raw events**: see every key event system-wide, and suppress any press.
- **Permissions handled**: on macOS, events start flowing once the Accessibility permission is granted and stop cleanly when it is revoked.

## Install

```toml
[dependencies]
keyvent = "0.2"
```

## Hotkeys

```rust,no_run
use keyvent::{HotkeyEvent, Suppression};

let events = keyvent::on("Super+Shift+K".parse().unwrap(), Suppression::Swallow);
for event in events.iter() {
    match event {
        HotkeyEvent::Down => println!("down"),
        HotkeyEvent::Up { held_for } => println!("up after {held_for:?}"),
    }
}
```

`Swallow` hides the hotkey's keys from other apps; `Pass` only observes them.
Dropping `events` unregisters the hotkey, and `pause()` and `resume()` suspend it without unregistering.

To stop listening from another thread, take the guard first:

```rust,no_run
let mut events = keyvent::on("F5".parse().unwrap(), keyvent::Suppression::Pass);
let guard = events.guard().unwrap();
let listener = std::thread::spawn(move || {
    for event in events.iter() {
        println!("{event:?}");
    }
});

// Unregisters the hotkey and ends the loop above.
drop(guard);
listener.join().unwrap();
```

### Hotkey strings

A hotkey is any of the modifiers `Fn`, `Ctrl`, `Alt`, `Shift` and `Super`, joined by `+`, then at most one key.
`Super` is Command on macOS and the Windows key on Windows.
Parsing ignores case, and `to_string()` gives the canonical form back.

| String | Fires on |
|---|---|
| `Super+K` | Super and K, with no other modifier held |
| `F5` | F5 alone |
| `Alt` | either Alt key alone |
| `AltRight` | the right Alt key alone |
| `Fn` | the Fn (Globe) key alone, macOS only |
| `Super+Shift` | Super and Shift together, with no other key |

The modifiers must match exactly: `Super+K` does not fire on `Super+Shift+K`.
Keys are named `A` to `Z`, `0` to `9`, `F1` to `F20`, `Space`, `Return`, `Tab`, `Escape`, `Delete`, `ArrowUp` and the other arrows, `CapsLock`, punctuation such as `Minus`, `Comma` and `Slash`, and side-specific modifiers such as `ShiftLeft` and `AltRight`.

## Recording a hotkey

```rust,no_run
use keyvent::{RecordOutcome, RecorderEvent};

for event in keyvent::record().iter() {
    match event {
        RecorderEvent::Candidate(hotkey) => println!("so far: {hotkey}"),
        RecorderEvent::Finished(RecordOutcome::Recorded(hotkey)) => println!("recorded {hotkey}"),
        RecorderEvent::Finished(RecordOutcome::Cancelled) => println!("cancelled"),
    }
}
```

`Candidate` reports the keys held so far, for a live preview; exactly one `Finished` follows, and then the loop ends.

- The recording finishes as `Recorded` when every key is released.
- Pressing Esc alone finishes it as `Cancelled`, so a bare `Escape` cannot be recorded, though `Ctrl+Escape` can.
- Dropping the handle cancels from code instead; the stream then simply ends, with no `Finished`.
- A lone modifier records that side only (`AltRight`); pressing both sides records either side (`Alt`).
- While recording, keyboard input is hidden from other apps and registered hotkeys are paused.

## Raw events

```rust,no_run
let guard = keyvent::start(|event| {
    println!("{:?} {:?} {}", event.kind, event.key, event.modifiers);
    // Return true to suppress the event system-wide.
    false
});
// Dropping the guard stops the callback.
```

The callback runs on the event thread for every keystroke, so it must return quickly; send slow work to your own thread.
Only presses can be suppressed: a release always follows what its press did, so no key is ever left stuck.

## Status

Every handle (`Guard`, `HotkeyEvents` and `RecorderEvents`) reports whether events are flowing:

```rust,no_run
use keyvent::{Status, Suppression};

let events = keyvent::on("Super+K".parse().unwrap(), Suppression::Swallow);
if events.status() == Status::Silent {
    println!("Allow this app in System Settings > Privacy & Security > Accessibility.");
}
for status in events.status_receiver() {
    println!("now {status:?}");
}
```

The status is already known when a handle is returned, and follows a permission change within about a second.
The first handle starts a shared background engine and blocks until its first check completes, usually under a millisecond; in async code, create it inside `spawn_blocking`.
The engine stops when the last handle is dropped.

## Platform notes

### macOS

- The process needs the Accessibility permission. Until it is granted, the status is `Silent`.
- macOS prompts for it only the first time the app asks. Once the user declines, it never prompts again, so show your own guidance while the status is `Silent`.
- The grant is tied to the app's code signature. Sign with a stable identity, such as Apple Development or Developer ID: with ad-hoc signing, every rebuild silently drops the grant.
- Run from a terminal, as with `cargo run`, the permission belongs to the terminal app.
- Suppression does not stop the physical Caps Lock key from switching input sources or toggling the lock.
- Swallowing a lone `Fn` hotkey also suppresses the system "Press 🌐 key to" action.

### Windows

- No permission is needed.
- The Fn key is handled by the keyboard itself and is never reported.

## Example

`cargo run --example record_hotkey` records a hotkey, registers it, and prints its activations.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
