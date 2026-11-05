//! Live state shown on the status page. Tasks update it; the web server
//! reads a copy.

use alloc::string::String;
use core::{
    cell::RefCell,
    net::Ipv4Addr,
    sync::atomic::{AtomicU32, Ordering},
};

use embassy_time::Instant;

use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};

use crate::{artnet::Report, led::Shape};

#[derive(Clone)]
pub struct Status {
    /// A W5500 answered at start-up.
    pub eth_present: bool,
    pub eth_link: bool,
    pub eth_addr: Option<(Ipv4Addr, u8)>,
    /// How the Ethernet address was set: "DHCP", "static", "fallback".
    pub eth_source: &'static str,
    /// Name of the setup Wi-Fi, once it runs.
    pub ap_ssid: Option<String>,
    /// Why the setup Wi-Fi started.
    pub ap_reason: &'static str,
    /// The network Art-Net is received on.
    pub artnet_net: &'static str,
    /// The latest Art-Net statistics window.
    pub artnet: Option<Report>,
    /// LED output: the timing in use and the sample rate measured at start.
    pub led_shape: Option<Shape>,
    pub led_measured_hz: u32,
    /// Frames sent per second, underruns since start, longest frame (µs).
    pub led_fps: u32,
    /// Estimated current per output (mA) and whether the limit cut in.
    /// Estimated power per output (mW).
    pub led_power_mw: [u32; 4],
    pub led_power_limited: bool,
    pub led_underruns: u32,
    pub led_frame_us: u32,
}

static STATUS: Mutex<CriticalSectionRawMutex, RefCell<Status>> = Mutex::new(RefCell::new(Status {
    eth_present: false,
    eth_link: false,
    eth_addr: None,
    eth_source: "",
    ap_ssid: None,
    ap_reason: "",
    artnet_net: "",
    artnet: None,
    led_shape: None,
    led_measured_hz: 0,
    led_fps: 0,
    led_power_mw: [0; 4],
    led_power_limited: false,
    led_underruns: 0,
    led_frame_us: 0,
}));

pub fn update(f: impl FnOnce(&mut Status)) {
    STATUS.lock(|s| f(&mut s.borrow_mut()));
}

pub fn get() -> Status {
    STATUS.lock(|s| s.borrow().clone())
}

/// Reads the status without copying it (no allocation under the lock).
pub fn with<R>(f: impl FnOnce(&Status) -> R) -> R {
    STATUS.lock(|s| f(&s.borrow()))
}

// ── Watched universe ────────────────────────────────────────────────────────
// The live page shows the channel values of one universe. The Art-Net task
// copies packets of that universe here; the page picks it with `?u=`.

/// Time spent handling received Art-Net/sACN packets (parse, merge, copy
/// into the frame), cumulative µs, and how many.
pub static RX_PROC_US: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub static RX_PROC_N: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Of that, time in the LED frame copy (`ledout::on_dmx`).
pub static RX_COPY_US: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Set by `POST /debug/wifi`: start the setup Wi-Fi as if BOOT were held.
pub static AP_REQUEST: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Lowest free heap seen (bytes), sampled once a second.
pub static HEAP_MIN: AtomicU32 = AtomicU32::new(u32::MAX);
/// Universes on which two Art-Net sources are being merged.
pub static MERGING: AtomicU32 = AtomicU32::new(0);

/// The universe the live view (/artnet) watches; u32::MAX = nobody. It
/// lapses 5 s after the last request, so no copy is made on the hot path
/// while no one looks.
pub static WATCH_U: AtomicU32 = AtomicU32::new(u32::MAX);
static WATCH_ASKED_S: AtomicU32 = AtomicU32::new(0);

pub struct Watch {
    pub u: u16,
    pub len: usize,
    pub data: [u8; crate::artnet::DMX_CHANNELS],
    /// When it was last received.
    pub at: Option<Instant>,
}

static WATCH: Mutex<CriticalSectionRawMutex, RefCell<Watch>> = Mutex::new(RefCell::new(Watch {
    u: 0,
    len: 0,
    data: [0; crate::artnet::DMX_CHANNELS],
    at: None,
}));

/// Called for every DMX packet; keeps it if it is the watched universe.
#[esp_hal::ram]
pub fn offer_dmx(universe: u16, data: &[u8]) {
    if u32::from(universe) != WATCH_U.load(Ordering::Relaxed) {
        return;
    }
    if (Instant::now().as_secs() as u32).wrapping_sub(WATCH_ASKED_S.load(Ordering::Relaxed)) > 5 {
        WATCH_U.store(u32::MAX, Ordering::Relaxed);
        return;
    }
    WATCH.lock(|w| {
        let mut w = w.borrow_mut();
        let n = data.len().min(crate::artnet::DMX_CHANNELS);
        w.u = universe;
        w.len = n;
        w.data[..n].copy_from_slice(&data[..n]);
        w.at = Some(Instant::now());
    });
}

/// Selects the watched universe; returns its latest data if any.
pub fn watch(universe: u16, f: impl FnOnce(&Watch)) {
    WATCH_ASKED_S.store(Instant::now().as_secs() as u32, Ordering::Relaxed);
    if WATCH_U.swap(u32::from(universe), Ordering::Relaxed) != u32::from(universe) {
        WATCH.lock(|w| {
            let mut w = w.borrow_mut();
            w.u = universe;
            w.len = 0;
            w.at = None;
        });
    }
    WATCH.lock(|w| f(&w.borrow()));
}
