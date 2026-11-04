//! Keeps the second core (which runs the LED output) out of the way while
//! the first core writes to flash.
//!
//! While flash is written or erased the chip is busy, so any cache miss on
//! either core would read garbage: the other core must not run code or read
//! data from flash meanwhile. esp-storage could
//! halt the core in hardware (`multicore_auto_park`), but that can stop it
//! inside a critical section, and the flash routines take one themselves, so
//! both cores would wait forever. Instead the second core parks itself at a
//! safe point, the way ESP-IDF does it: a small task on its executor waits
//! for a request, masks its interrupts and spins in RAM until the write is
//! done.
//!
//! Every function that writes or erases flash takes a [`Paused`] guard, so
//! the compiler enforces this (the flash driver no longer checks it itself;
//! see `FlashStorage::multicore_ignore` in main). Get one from [`pause`], or
//! from [`before_start`] during start-up.
//!
//! Not reentrant: only one guard may exist at a time. Every caller holds
//! the shared flash lock while it has one, which guarantees that.

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer};
use log::error;

/// The second core has been started (set by main right after starting it;
/// before that there is nothing to park).
static UP: AtomicBool = AtomicBool::new(false);
/// The first core wants the second core parked.
static REQUESTED: AtomicBool = AtomicBool::new(false);
/// The second core is parked (spinning in RAM with interrupts masked).
static PARKED: AtomicBool = AtomicBool::new(false);
static WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Runs on the second core's executor, next to the LED task.
pub async fn pause_task() -> ! {
    loop {
        WAKE.wait().await;
        if REQUESTED.load(Ordering::Acquire) {
            park();
        }
    }
}

/// Spins until the request is withdrawn. Lives in RAM and only touches
/// statics in RAM, so it keeps running while the flash cache is off.
#[esp_hal::ram]
fn park() {
    let mask = xtensa_lx::interrupt::disable();
    PARKED.store(true, Ordering::Release);
    while REQUESTED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    PARKED.store(false, Ordering::Release);
    // SAFETY: restores the mask saved above; not inside a critical section.
    unsafe { xtensa_lx::interrupt::set_mask(mask) };
}

/// Call right after starting the second core.
pub fn mark_started() {
    UP.store(true, Ordering::Release);
}

/// While alive, the second core is parked (or not started yet) and flash
/// may be written.
pub struct Paused(());

impl Drop for Paused {
    fn drop(&mut self) {
        REQUESTED.store(false, Ordering::Release);
        // Wait until the second core has really left `park`, so a pause
        // right after this one cannot see the old PARKED flag. Bounded: it
        // is spinning with interrupts off and leaves within nanoseconds.
        while PARKED.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
    }
}

/// A guard for flash writes during start-up, before the second core runs.
///
/// # Panics
/// If the second core has been started already.
pub fn before_start() -> Paused {
    assert!(!UP.load(Ordering::Acquire), "core1::before_start after the second core started");
    Paused(())
}

/// Parks the second core and returns once it is parked. Before the second
/// core has started there is nothing to park.
pub async fn pause() -> Paused {
    let guard = Paused(());
    if !UP.load(Ordering::Acquire) {
        return guard;
    }
    REQUESTED.store(true, Ordering::Release);
    WAKE.signal(());
    let start = Instant::now();
    while !PARKED.load(Ordering::Acquire) {
        if start.elapsed() > Duration::from_secs(2) {
            // The LED task never yields for this long; something is badly
            // wrong. Writing anyway could crash the second core, so restart.
            error!("core1: the second core did not park; restarting");
            crate::crashlog::watchdog("The LED core did not pause for a flash write");
            Timer::after(Duration::from_millis(100)).await;
            esp_hal::system::software_reset();
        }
        Timer::after(Duration::from_micros(200)).await;
    }
    guard
}
