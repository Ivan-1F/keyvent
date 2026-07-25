//! keyvent soak binary: exercises the LIBRARY (not a private implementation)
//! under repeatable permission-lifecycle scenarios.
//!
//!   keyvent-spike watch [--verbose] [--swallow-key K]
//!   keyvent-spike bench [iterations]
//!
//! `watch` starts `keyvent::start`, logs every event and every status change to
//! stdout and /tmp/keyvent-spike.log, and parks forever. Run it as the signed
//! .app (scripts/package-spike.sh) and drive permission changes from System
//! Settings — expected behavior is E2b: revoke -> Silent within ~2s with zero
//! input impact; re-grant -> Running within ~1s.

use std::env;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;

use keyvent::{Key, KeyEvent, Status};

struct SoakLogger {
    file: Mutex<File>,
    t0: Instant,
}

impl log::Log for SoakLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let line = format!(
            "[{:>9}ms] {:<5} {}",
            self.t0.elapsed().as_millis(),
            record.level(),
            record.args()
        );
        println!("{line}");
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }

    fn flush(&self) {}
}

fn init_logger() {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open("/tmp/keyvent-spike.log")
        .expect("open log file");
    let logger = SoakLogger {
        file: Mutex::new(file),
        t0: Instant::now(),
    };
    log::set_logger(Box::leak(Box::new(logger))).expect("set logger");
    log::set_max_level(log::LevelFilter::Debug);
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Running => "Running",
        Status::Silent => "Silent",
        Status::Cooldown => "Cooldown",
    }
}

fn parse_key(name: &str) -> Option<Key> {
    Some(match name.to_ascii_lowercase().as_str() {
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f20" => Key::F20,
        "fn" | "function" | "globe" => Key::Function,
        "space" => Key::Space,
        "a" => Key::KeyA,
        _ => return None,
    })
}

fn watch(verbose: bool, swallow_key: Option<Key>) -> ! {
    init_logger();
    log::info!("soak watch start: verbose={verbose} swallow_key={swallow_key:?}");

    let guard = keyvent::start(move |event: KeyEvent| {
        if verbose {
            log::info!(
                "event {:?} {:?} mods={:?}{}",
                event.kind,
                event.key,
                event.modifiers,
                if event.is_repeat { " (repeat)" } else { "" }
            );
        }
        swallow_key == Some(event.key)
    });

    guard.on_status_change(|status| {
        log::info!("=== STATUS {} ===", status_name(status));
    });

    log::info!("keyvent started; current status = {}", status_name(guard.status()));
    std::thread::park();
    unreachable!();
}

fn bench(iterations: usize) {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    for _ in 0..1_000 {
        std::hint::black_box(unsafe { AXIsProcessTrusted() });
    }
    let mut v: Vec<u128> = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let t = Instant::now();
        std::hint::black_box(unsafe { AXIsProcessTrusted() });
        v.push(t.elapsed().as_nanos());
    }
    v.sort_unstable();
    let mean = v.iter().sum::<u128>() / v.len() as u128;
    println!(
        "AXIsProcessTrusted: n={iterations} mean={mean}ns p50={}ns p99={}ns max={}ns",
        v[v.len() / 2],
        v[v.len() * 99 / 100],
        v[v.len() - 1]
    );
}

fn usage() -> ! {
    eprintln!("usage:\n  keyvent-spike watch [--verbose] [--swallow-key K]\n  keyvent-spike bench [iterations]");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("watch") => {
            let mut verbose = false;
            let mut swallow_key = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--verbose" => verbose = true,
                    "--swallow-key" => {
                        i += 1;
                        swallow_key = args.get(i).and_then(|k| parse_key(k));
                    }
                    _ => usage(),
                }
                i += 1;
            }
            watch(verbose, swallow_key);
        }
        Some("bench") => {
            let n = args
                .get(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(1_000_000);
            bench(n);
        }
        _ => usage(),
    }
}
