# keyvent

Global keyboard event stream for macOS, with automatic Accessibility permission lifecycle management.

## Install

```toml
[dependencies]
keyvent = "0.1"
```

## Usage

```rust
// This will return immediately.
let guard = keyvent::start(|event| {
    // Return true to suppress the event system-wide.
    event.key == keyvent::Key::F5
});
// Dropping the guard stops the callback, dropping the last guard stops everything cleanly.
```

## Platform Caveats

### macOS

The process needs to be granted the Accessibility permission (System Preferences > Privacy & Security > Accessibility)

Without Accessibility permission, the stream simply stays silent; the stream starts automatically once permission is granted, and stops cleanly on revocation.
