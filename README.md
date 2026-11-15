# keyvent

Global keyboard event stream for macOS, with automatic Accessibility permission lifecycle management.

## Install

```toml
[dependencies]
keyvent = "0.1"
```

## Usage

```rust
// Returns once the initial status is known, within milliseconds.
let guard = keyvent::start(|event| {
    // Return true to suppress the event system-wide.
    event.key == keyvent::Key::F5
});
// Dropping the guard stops the callback, dropping the last guard stops everything cleanly.
```

## Platform Caveats

### macOS

The process needs the Accessibility permission (System Settings > Privacy & Security > Accessibility).
Without it the stream stays silent; it starts automatically once permission is granted, and stops cleanly on revocation.
