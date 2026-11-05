//! Built-in test patterns: which one runs (the patterns themselves are in
//! `pixelcast_core::testpattern`, host-tested).

use core::cell::Cell;

use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_sync::signal::Signal;

pub use pixelcast_core::testpattern::*;

static CURRENT: Mutex<CriticalSectionRawMutex, Cell<Option<(Pattern, u64)>>> = Mutex::new(Cell::new(None));
/// Wakes the pattern task when the pattern changes.
pub static CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// The running pattern and when it started (seconds since boot).
pub fn current() -> Option<(Pattern, u64)> {
    CURRENT.lock(|c| c.get())
}

pub fn active() -> bool {
    current().is_some()
}

/// Starts `p` (or stops with `None`).
pub fn set(p: Option<Pattern>, now_s: u64) {
    CURRENT.lock(|c| c.set(p.map(|p| (p, now_s))));
    CHANGED.signal(());
}
