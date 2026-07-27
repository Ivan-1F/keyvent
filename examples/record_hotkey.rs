//! Record a global hotkey, then register it and listen for activations.
//!
//! Run with `cargo run --example record_hotkey`. The process needs the
//! Accessibility permission (System Settings > Privacy & Security >
//! Accessibility); until granted, the stream stays silent and the recording
//! simply starts once permission arrives.

use std::io::{self, Write};

use keyvent::{
    Hotkey, HotkeyEvent, HotkeyManager, HotkeyRecorder, Key, Modifiers, RecorderEvent, Status,
    Suppression,
};

fn main() {
    let (manager, manager_events) = HotkeyManager::new();
    let (recorder, recorder_events) = HotkeyRecorder::new();

    if manager.status() != Status::Running {
        println!("Waiting for Accessibility permission…");
    }
    println!("Record a hotkey (Esc cancels, Delete clears).");

    // Suspend matching so already-registered hotkeys don't fire into the
    // recorder; re-enable once the gesture is over.
    manager.disable();
    recorder.start_recording();

    let hotkey = loop {
        match recorder_events.recv().unwrap() {
            // Live preview while the gesture builds.
            RecorderEvent::Candidate(h) => {
                print!("\r\x1b[K> {hotkey}", hotkey = show(h));
                io::stdout().flush().unwrap();
            }
            RecorderEvent::Recorded(h) => {
                println!("\r\x1b[KRecorded: {hotkey}", hotkey = show(h));
                break h;
            }
            RecorderEvent::Cancelled => {
                println!("\r\x1b[KCancelled.");
                return;
            }
            RecorderEvent::Cleared => {
                println!("\r\x1b[KCleared.");
                return;
            }
        }
    };
    manager.enable();

    manager.register(hotkey, Suppression::Swallow);
    println!(
        "Registered. Press {hotkey} anywhere (Ctrl+C quits).",
        hotkey = show(hotkey)
    );
    while let Ok(event) = manager_events.recv() {
        match event {
            HotkeyEvent::Down(_) => println!("down"),
            HotkeyEvent::Up { held_for, .. } => println!("up, held for {held_for:.2?}"),
        }
    }
}

/// Symbol rendering for the demo (⌃⌥⇧⌘ + key name); apps own their display.
fn show(hotkey: Hotkey) -> String {
    let mut s = String::new();
    let mods = hotkey.modifiers();
    for (bit, symbol) in [
        (Modifiers::CONTROL, "⌃"),
        (Modifiers::ALT, "⌥"),
        (Modifiers::SHIFT, "⇧"),
        (Modifiers::SUPER, "⌘"),
        (Modifiers::FN, "🌐"),
    ] {
        if mods.contains(bit) {
            s.push_str(symbol);
        }
    }
    if let Some(key) = hotkey.key() {
        s.push_str(&key_name(key));
    }
    s
}

fn key_name(key: Key) -> String {
    // Debug gives "KeyK", "F5", "AltRight"; strip the letter-key prefix.
    let debug = format!("{key:?}");
    debug
        .strip_prefix("Key")
        .map(str::to_owned)
        .unwrap_or(debug)
}
