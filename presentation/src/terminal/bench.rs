//! Per-second drawing counters adapted from herdr-app `src/bench.rs` (Apache-2.0).

use std::{
    fs::OpenOptions,
    io::Write,
    sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static ENABLED: AtomicBool = AtomicBool::new(false);
pub(crate) static WINDOW: AtomicU64 = AtomicU64::new(0);
pub(super) static TERMINAL: AtomicU64 = AtomicU64::new(0);
pub(super) static GRID: AtomicU64 = AtomicU64::new(0);
pub(super) static ROWS: AtomicU64 = AtomicU64::new(0);
pub(super) static DECODED_ROWS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn count(counter: &AtomicU64) {
    if ENABLED.load(Relaxed) {
        counter.fetch_add(1, Relaxed);
    }
}

pub(crate) fn start() {
    let Some(path) = std::env::var_os("POWER_TERMINAL_BENCH") else {
        return;
    };
    let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("Could not open terminal bench file {path:?}. {error}");
            return;
        }
    };
    ENABLED.store(true, Relaxed);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let sample = serde_json::json!({
                "t": t,
                "window": WINDOW.swap(0, Relaxed),
                "terminal": TERMINAL.swap(0, Relaxed),
                "grid": GRID.swap(0, Relaxed),
                "rows": ROWS.swap(0, Relaxed),
                "decoded_rows": DECODED_ROWS.swap(0, Relaxed),
            });
            if let Err(error) = writeln!(file, "{sample}") {
                eprintln!("Could not write terminal bench file {path:?}. {error}");
                ENABLED.store(false, Relaxed);
                break;
            }
        }
    });
}
