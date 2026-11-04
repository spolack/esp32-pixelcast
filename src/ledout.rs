//! LED output: assembles Art-Net universes into frames and streams them out
//! over I2S1 in 8-bit parallel mode (see `led.rs` for the signal itself).
//!
//! ## Frames
//! The Art-Net task hands every DMX packet to [`on_dmx`], which copies it
//! into the receive frame (wire order, brightness applied). A frame is shown
//! - on ArtSync, once a sender uses it (then only on ArtSync, until none
//!   came for 2 s),
//! - otherwise as soon as every mapped universe has arrived,
//! - or 30 ms after its first universe, if some never come.
//!
//! The LED task copies the receive frame into its own output buffer and
//! streams it; a frame that completes while one is still being sent
//! replaces any earlier waiting one (frames are dropped, never queued).
//!
//! ## Streaming
//! The encoder fills a 16 KB DMA ring while it drains (~5 ms of slack at
//! 3.3 MHz). If the ring ever runs dry mid-frame, the line goes low long
//! enough for the LEDs to latch a partial frame: that is counted as an
//! underrun. Flash writes (settings save, firmware update) block the CPU
//! for tens of ms and will cause one; nothing else should.

use core::cell::RefCell;

use embassy_futures::select::{Either3, select, select3};
use embassy_sync::{
    blocking_mutex::{Mutex, raw::CriticalSectionRawMutex},
    signal::Signal,
};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::{
    dma::DmaTxStreamBuf,
    gpio::NoPin,
    i2s::parallel::{I2sParallel, TxEightBits},
    peripherals::{DMA_I2S1, GPIO12, GPIO14, GPIO26, GPIO27, I2S1},
    time::Rate,
};
use log::{info, warn};

use crate::{
    frame,
    led::{self, Encoder, LANES, Mapping},
    settings, status,
};

// ── Shared LED memory ───────────────────────────────────────────────────────
//
// Four frame copies (the receive frame and the LED core's three input slots)
// share one pool; each copy holds every output's bytes at `Layout::offsets`.
// What fits is the pool's limit split into four (see `led::effective`). When
// the setup Wi-Fi starts, the limit drops to `POOL_WIFI_BYTES` and the rest of
// the pool goes to the heap (until the next restart).

/// The whole pool (in the reclaimed RAM, which it fills).
pub const POOL_BYTES: usize = 65 * 1024;
/// What the LEDs keep once the setup Wi-Fi runs (1596 RGBW LEDs).
pub const POOL_WIFI_BYTES: usize = POOL_BYTES - WIFI_HEAP_BYTES;
/// What the setup Wi-Fi takes from the pool for the heap: its driver needs
/// ~47 KB (measured), the heap has ~17 KB free without it.
pub const WIFI_HEAP_BYTES: usize = 40 * 1024;
const COPIES: usize = 1 + NSLOTS;

#[esp_hal::ram(reclaimed)]
static mut POOL: core::mem::MaybeUninit<[u32; POOL_BYTES / 4]> = core::mem::MaybeUninit::uninit();

#[inline(always)]
fn pool() -> *mut u8 {
    core::ptr::addr_of_mut!(POOL).cast()
}

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
static POOL_LIMIT: AtomicUsize = AtomicUsize::new(POOL_BYTES);
/// Bytes the current layout uses (all copies), and the request it answers.
static POOL_USED: AtomicUsize = AtomicUsize::new(0);
static LAYOUT_REQ: AtomicU32 = AtomicU32::new(0);
static LAYOUT_DONE: AtomicU32 = AtomicU32::new(0);
static WIFI_CAPPED: AtomicBool = AtomicBool::new(false);
/// LEDs per output that fit (what is actually driven).
static EFFECTIVE: [AtomicU16; LANES] = [const { AtomicU16::new(0) }; LANES];

/// Bytes per frame copy that the outputs may use under `limit`.
pub const fn copy_budget(limit: usize) -> usize {
    limit / COPIES - 4 * LANES // room for the 4-byte alignment of each output
}

/// Where the outputs sit in a frame copy.
#[derive(Clone, Copy)]
struct Layout {
    offsets: [usize; LANES],
    lens: [usize; LANES],
    stride: usize,
}

const NO_LAYOUT: Layout = Layout { offsets: [0; LANES], lens: [0; LANES], stride: 0 };

/// Output `k` of frame copy `copy` (0 = receive frame, 1.. = input slots).
///
/// # Safety
/// The caller must own that copy for the lifetime it picks (see the callers).
#[inline(always)]
unsafe fn lane<'a>(l: &Layout, copy: usize, k: usize) -> &'a mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(pool().add(copy * l.stride + l.offsets[k]), l.lens[k]) }
}

/// Lowers the LED memory for the setup Wi-Fi and returns the freed end of
/// the pool for the heap, once the LED core no longer uses it. `None` if it
/// did not confirm in time (then nothing is handed over).
pub async fn shrink_for_wifi() -> Option<(*mut u8, usize)> {
    POOL_LIMIT.store(POOL_WIFI_BYTES, Ordering::SeqCst);
    WIFI_CAPPED.store(true, Ordering::Relaxed);
    let want = LAYOUT_REQ.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    settings::LED_CHANGED.signal(());
    let start = Instant::now();
    while (LAYOUT_DONE.load(Ordering::SeqCst).wrapping_sub(want) as i32) < 0
        || POOL_USED.load(Ordering::SeqCst) > POOL_WIFI_BYTES
    {
        if start.elapsed() > Duration::from_secs(3) {
            return None;
        }
        Timer::after(Duration::from_millis(10)).await;
    }
    // SAFETY: in bounds of the pool; the LED side keeps below POOL_WIFI_BYTES
    // from now on (the limit never rises again).
    Some((unsafe { pool().add(POOL_WIFI_BYTES) }, POOL_BYTES - POOL_WIFI_BYTES))
}

/// The LED memory for the status page: pool and limit (bytes), used, whether
/// the setup Wi-Fi took its share, LEDs driven per output.
pub fn pool_json(o: &mut crate::json::Obj<'_>) {
    let e: [u16; LANES] = core::array::from_fn(|k| EFFECTIVE[k].load(Ordering::Relaxed));
    o.field("bytes", POOL_BYTES)
        .field("limit", POOL_LIMIT.load(Ordering::Relaxed))
        .field("used", POOL_USED.load(Ordering::Relaxed))
        .field("wifi_capped", WIFI_CAPPED.load(Ordering::Relaxed))
        .field("effective_leds", e);
}

// ── Receive side (called from the Art-Net task) ────────────────────────────

struct RxFrame {
    /// The receive frame is copy 0 of the pool.
    layout: Layout,
    mapping: Option<Mapping>,
    /// When the frame is shown (`pixelcast_core::frame`, tested).
    gate: frame::Gate,
}

static RX: Mutex<CriticalSectionRawMutex, RefCell<RxFrame>> = Mutex::new(RefCell::new(RxFrame {
    layout: NO_LAYOUT,
    mapping: None,
    gate: frame::Gate::new(),
}));

/// Wakes the LED task: a frame is ready.
static FRAME_READY: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Wakes the LED task if `shown`.
fn shown(shown: bool) {
    if shown {
        FRAME_READY.signal(());
    }
}

fn now_us() -> u64 {
    Instant::now().as_micros()
}

/// Takes one ArtDmx packet. In IRAM: it runs for every packet on the
/// network core, which shares its flash cache with everything else.
#[esp_hal::ram]
pub fn on_dmx(universe: u16, data: &[u8]) {
    RX.lock(|f| {
        let RxFrame { mapping: Some(m), layout, gate } = &mut *f.borrow_mut() else {
            return;
        };
        let hit = m.slots_for(universe).fold(0u64, |h, slot| h | 1 << slot.bit);
        if hit == 0 {
            return;
        }
        let now = now_us();
        shown(gate.before_universe(hit, now));
        for slot in m.slots_for(universe) {
            // SAFETY: copy 0 belongs to the receive frame, and this lock is held.
            led::copy_universe(slot, data, unsafe { lane(layout, 0, slot.lane) });
        }
        shown(gate.after_universe(hit, m.all_bits(), now));
    });
}

/// Takes one ArtSync packet: show what has arrived.
pub fn on_sync() {
    RX.lock(|f| {
        let f = &mut *f.borrow_mut();
        let mapped = f.mapping.is_some();
        shown(f.gate.sync(now_us(), mapped));
    });
}

/// Shows an incomplete frame once its timeout has passed.
fn check_timeout() {
    RX.lock(|f| shown(f.borrow_mut().gate.tick(now_us())));
}

/// Applies the LED settings: fits the outputs into the LED memory, lays out
/// the frame copies and sets up the receive side. Runs on the LED core while
/// no encoder reads the input slots.
fn configure(s: &settings::Settings) -> Layout {
    let req = LAYOUT_REQ.load(Ordering::SeqCst);
    let limit = POOL_LIMIT.load(Ordering::SeqCst);
    let outs = led::effective(&s.outputs, s.led_type, copy_budget(limit));
    let lens: [usize; LANES] = core::array::from_fn(|k| outs[k].wire_bytes(s.led_type));
    let (offsets, stride) = led::layout(&lens);
    let l = Layout { offsets, lens, stride };
    for (e, o) in EFFECTIVE.iter().zip(outs.iter()) {
        e.store(o.leds() as u16, Ordering::Relaxed);
    }
    let mapping = Mapping::new(s.led_type, s.order, &outs);
    RX.lock(|f| {
        let f = &mut *f.borrow_mut();
        f.mapping = Some(mapping);
        f.layout = l;
        f.gate.restart();
        // Start dark; old data may be in the wrong place now. Only the
        // receive copy needs the lock (the rest is zeroed below).
        // SAFETY: within the limit; the receive copy is guarded by this lock.
        unsafe { pool().write_bytes(0, stride.min(limit / COPIES)) };
    });
    // The input slots, outside the lock: up to ~60 KB, and the lock is the
    // one both cores share. SAFETY: within the limit; no encoder reads the
    // slots while the LED core configures.
    let copy = stride.min(limit / COPIES);
    unsafe { pool().add(copy).write_bytes(0, (COPIES - 1) * copy) };
    if outs != s.outputs {
        warn!("led: the outputs do not all fit into the LED memory; driving {:?} LEDs", outs.map(|o| o.leds));
    }
    POOL_USED.store(COPIES * stride, Ordering::SeqCst);
    LAYOUT_DONE.store(req, Ordering::SeqCst);
    l
}

/// The LED colours of each output as received (before brightness and the
/// power limit), sampled to at most `max` points: hex `rrggbb` per point, the
/// white channel of RGBW mixed in. For the live preview.
pub fn preview(max: usize) -> [alloc::string::String; LANES] {
    let s = settings::current();
    core::array::from_fn(|k| {
        let o = &s.outputs[k];
        let leds = usize::from(EFFECTIVE[k].load(Ordering::Relaxed)).min(o.leds());
        let ty = o.ty_or(s.led_type);
        let ch = ty.channels();
        let order = o.order.unwrap_or(s.order).0;
        let step = leds.div_ceil(max.max(1)).max(1);
        let mut hex = alloc::string::String::with_capacity(leds.div_ceil(step) * 6);
        RX.lock(|f| {
            // SAFETY: copy 0 is guarded by this lock.
            let lane: &[u8] = unsafe { lane(&f.borrow().layout, 0, k) };
            for led in (0..leds).step_by(step) {
                let Some(w) = lane.get(led * ch..led * ch + ch) else { break };
                // Wire byte j carries DMX channel order[j].
                let mut rgb = [0u8; 3];
                for (j, &c) in order.iter().enumerate() {
                    rgb[usize::from(c)] = w[j];
                }
                let white = if ch == 4 { w[3] } else { 0 };
                // Table lookups, not `write!`: this runs under the lock both
                // cores take, and the capacity is reserved (no allocation).
                const HEX: &[u8; 16] = b"0123456789abcdef";
                for c in rgb {
                    let v = c.saturating_add(white);
                    hex.push(char::from(HEX[usize::from(v >> 4)]));
                    hex.push(char::from(HEX[usize::from(v & 15)]));
                }
            }
        });
        hex
    })
}

// ── Output side ────────────────────────────────────────────────────────────

/// The peripherals the LED output owns.
pub struct LedHw {
    pub i2s: I2S1<'static>,
    pub dma: DMA_I2S1<'static>,
    // Output 1-4 in the board's channel order (B8..B5 on the V2.x boards,
    // A1..A4 on V3.x).
    pub sig1: GPIO12<'static>,
    pub sig2: GPIO14<'static>,
    pub sig3: GPIO27<'static>,
    pub sig4: GPIO26<'static>,
}

/// The driver and its spare DMA ring between transfers (`send` takes both
/// and the finished transfer hands them back).
struct Out<'d> {
    i2s: Option<I2sParallel<'d, esp_hal::Async>>,
    buf: Option<DmaTxStreamBuf>,
}

/// An empty ring for the next transfer: `DmaTxStreamBuf` keeps its fill
/// position across transfers, so it is split and rebuilt.
fn fresh(buf: DmaTxStreamBuf) -> DmaTxStreamBuf {
    let (d, r) = buf.split();
    DmaTxStreamBuf::new(d, r).unwrap_or_else(|_| unreachable!("the ring was valid before"))
}

static HEARTBEAT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Bumped by the LED task at least every 5 ms (the watchdog checks it).
pub fn heartbeat() -> u32 {
    HEARTBEAT.load(core::sync::atomic::Ordering::Relaxed)
}

fn beat() {
    HEARTBEAT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Ring space worth topping up at once (one DMA chunk, see main).
const CHUNK: usize = 2048;

/// Input frames on the LED core: the newest, the one before (interpolation)
/// and one to receive the next into.
const NSLOTS: usize = 3;

/// Copies the ready frame into slot `slot` if it is newer than `seen`.
///
/// Only the LED task calls this, and only for a slot no live encoder reads.

fn take_frame(slot: usize, seen: &mut u32, l: &Layout) -> bool {
    RX.lock(|f| {
        let f = f.borrow();
        if f.gate.seq() == *seen {
            return false;
        }
        // SAFETY: copy 0 is guarded by this lock; nothing else references
        // slot `slot` right now (see above). Same layout: set by this core.
        unsafe { core::ptr::copy_nonoverlapping(pool(), pool().add((1 + slot) * l.stride), l.stride) };
        *seen = f.gate.seq();
        true
    })
}

/// The bytes of slot `slot` to send per output.
fn slot_lanes(slot: usize, l: &Layout) -> [&'static [u8]; LANES] {
    // SAFETY: the slot is not written while an encoder reads it (see
    // `Inputs::busy`), and the layout only changes between frames.
    core::array::from_fn(|k| &*unsafe { lane(l, 1 + slot, k) })
}

/// What the slots hold and who reads them.
struct Inputs {
    latest: Option<usize>,
    older: Option<usize>,
    /// Encoders (sending or prepared) reading each slot.
    busy: [u8; NSLOTS],
    /// Channel-value sums per output of each slot (power estimate).
    sums: [[u32; LANES]; NSLOTS],
    /// When the newest frame arrived, and the smoothed time between frames.
    arrived: Instant,
    interval_us: u64,
}

impl Inputs {
    fn new() -> Self {
        Self { latest: None, older: None, busy: [0; NSLOTS], sums: [[0; LANES]; NSLOTS], arrived: Instant::now(), interval_us: 20_000 }
    }

    /// Takes a newer input frame if there is one and a slot is free for it.
    fn take(&mut self, seen: &mut u32, lens: &Layout) -> bool {
        let Some(i) = (0..NSLOTS).find(|&i| self.busy[i] == 0 && Some(i) != self.latest && Some(i) != self.older) else {
            return false;
        };
        if !take_frame(i, seen, lens) {
            return false;
        }
        let lanes = slot_lanes(i, lens);
        self.sums[i] = core::array::from_fn(|k| lanes[k].iter().map(|&b| u32::from(b)).sum());
        let now = Instant::now();
        if self.latest.is_some() {
            let dt = (now - self.arrived).as_micros().clamp(2_000, 500_000);
            self.interval_us = (self.interval_us * 3 + dt) / 4;
        }
        self.older = self.latest;
        self.latest = Some(i);
        self.arrived = now;
        true
    }
}

/// The render settings that do not change per frame.
struct RenderCfg {
    brightness: u16,
    interpolate: bool,
    dither: bool,
    fade_after: Option<Duration>,
    power: led::PowerLimit,
}

impl RenderCfg {
    fn new(s: &settings::Settings) -> Self {
        Self {
            brightness: (u32::from(s.brightness.min(100)) * u32::from(led::UNITY) / 100) as u16,
            interpolate: s.interpolate,
            dither: s.dither,
            fade_after: (s.loss == settings::Loss::Fade).then(|| Duration::from_secs(u64::from(s.loss_after_s))),
            power: led::PowerLimit::from_led(
                s.power_mw_per_led,
                core::array::from_fn(|k| s.outputs[k].ty_or(s.led_type).channels()),
                s.power_out_mw,
                s.power_total_mw,
            ),
        }
    }

    /// Fade factor after signal loss (UNITY = none, 0 = black).
    fn fade(&self, inp: &Inputs, now: Instant) -> u16 {
        const FADE: Duration = Duration::from_secs(2);
        match self.fade_after {
            Some(after) if now - inp.arrived > after => {
                let into = (now - inp.arrived - after).as_millis().min(FADE.as_millis());
                (u64::from(led::UNITY) - into * u64::from(led::UNITY) / FADE.as_millis()) as u16
            }
            _ => led::UNITY,
        }
    }

    /// Whether frames must keep coming without new input: blending is
    /// under way, dithering is on, or a fade has not reached black.
    fn dynamic(&self, inp: &Inputs, now: Instant) -> bool {
        if inp.latest.is_none() {
            return false;
        }
        let blending = self.interpolate && inp.older.is_some() && (now - inp.arrived).as_micros() < inp.interval_us;
        let fading = self.fade_after.is_some_and(|a| now - inp.arrived > a) && self.fade(inp, now) > 0;
        // One more frame after the fade reaches 0, so black is shown.
        let just_black = self.fade_after.is_some_and(|a| {
            let since = now - inp.arrived;
            since > a && since < a + Duration::from_millis(2100)
        });
        blending || self.dither || fading || just_black
    }
}

/// The next frame: its ring is pre-filled piece by piece while the current
/// frame is sent, and must be completely full before it starts (the DMA does
/// not pick up data appended behind a partly filled ring once it runs).
struct Prepared {
    ring: DmaTxStreamBuf,
    enc: Encoder<'static>,
    /// Slots its encoder reads (previous, current).
    reads: (usize, usize),
    /// The ring is full (or the frame is shorter than the ring).
    full: bool,
}

impl Prepared {
    /// Encodes up to one chunk more into the ring. Small pieces, so the
    /// current frame's ring is topped up in between (a whole ring at once
    /// took long enough with rendering to let it run dry).
    fn fill_some(&mut self) {
        if self.full {
            return;
        }
        let enc = &mut self.enc;
        let n = self.ring.push_with(|dst| {
            let k = dst.len().min(CHUNK);
            enc.fill(&mut dst[..k])
        });
        if n == 0 || enc.done() {
            self.full = true;
        }
    }

    fn fill_all(&mut self) {
        while !self.full {
            self.fill_some();
        }
    }
}

/// Prepares the next output frame from the newest input (blended with the
/// one before when interpolating), with gains and dithering.
fn prepare(
    ring: DmaTxStreamBuf,
    inp: &mut Inputs,
    cfg: &RenderCfg,
    frame: u32,
    lens: &Layout,
    shape: led::Shape,
    latch_us: u32,
) -> Option<Prepared> {
    let cur = inp.latest?;
    let now = Instant::now();
    let (prev, t) = match inp.older {
        Some(o) if cfg.interpolate => {
            let t = ((now - inp.arrived).as_micros() * u64::from(led::UNITY) / inp.interval_us.max(1)).min(u64::from(led::UNITY));
            (o, t as u16)
        }
        _ => (cur, led::UNITY),
    };
    let sums: [u32; LANES] = core::array::from_fn(|k| inp.sums[prev][k].max(inp.sums[cur][k]));
    let (mut gain, mw) = led::lane_gains(cfg.brightness, &sums, &cfg.power);
    let limited = gain.iter().any(|&g| g < cfg.brightness);
    let fade = cfg.fade(inp, now);
    for g in gain.iter_mut() {
        *g = (u32::from(*g) * u32::from(fade) / u32::from(led::UNITY)) as u16;
    }
    let mw = mw.map(|m| (u64::from(m) * u64::from(fade) / u64::from(led::UNITY)) as u32);
    status::update(|st| {
        st.led_power_mw = mw;
        st.led_power_limited = limited;
    });
    let render = led::Render { t, gain, dither: cfg.dither, frame };
    let enc = Encoder::new(slot_lanes(cur, lens), shape, latch_us).with_render(slot_lanes(prev, lens), render);
    inp.busy[prev] += 1;
    inp.busy[cur] += 1;
    Some(Prepared { ring: fresh(ring), enc, reads: (prev, cur), full: false })
}

fn give_back(spares: &mut [Option<DmaTxStreamBuf>; 2], ring: DmaTxStreamBuf) {
    if spares[0].is_none() {
        spares[0] = Some(ring);
    } else {
        spares[1] = Some(ring);
    }
}

/// Runs the LED output forever. `rings` are the two DMA rings (see main).
pub async fn run(mut hw: LedHw, rings: [DmaTxStreamBuf; 2]) -> ! {
    let [r0, r1] = rings;
    let mut spares: [Option<DmaTxStreamBuf>; 2] = [Some(r0), Some(r1)];
    loop {
        settings::LED_CHANGED.reset();
        let s = settings::current();
        let shape = led::quantize(&s.timing);
        let lens = configure(&s);
        let cfg = RenderCfg::new(&s);

        let i2s = I2sParallel::new(
            hw.i2s.reborrow(),
            hw.dma.reborrow(),
            Rate::from_hz(shape.sample_rate_hz),
            TxEightBits::new(
                hw.sig1.reborrow(),
                hw.sig2.reborrow(),
                hw.sig3.reborrow(),
                hw.sig4.reborrow(),
                NoPin,
                NoPin,
                NoPin,
                NoPin,
            ),
            NoPin,
        )
        .into_async();
        let mut out = Out { i2s: Some(i2s), buf: spares[0].take() };

        // Self-test: time a run of zeros (the lines stay low) to learn the
        // real sample rate.
        // Best of three: start-up work elsewhere (e.g. the Wi-Fi driver
        // starting) can delay the timer polling and make one run look slow.
        let mut measured = 0;
        for _ in 0..3 {
            measured = measured.max(measure_rate(&mut out).await);
        }
        spares[0] = out.buf.take();
        let Some(mut i2s) = out.i2s.take() else {
            unreachable!("measure_rate hands the driver back")
        };
        let (a0, a1, ap) = shape.actual_ns();
        info!(
            "led: {} samples/bit, {} Hz requested, {} Hz measured; pulses {}/{} ns, bit {} ns",
            shape.n, shape.sample_rate_hz, measured, a0, a1, ap
        );
        status::update(|st| {
            st.led_shape = Some(shape);
            st.led_measured_hz = measured;
        });

        let mut seen_seq = RX.lock(|f| f.borrow().gate.seq());
        let mut stats = OutStats::new();
        let mut inp = Inputs::new();
        let mut frame = 0u32;
        let mut next: Option<Prepared> = None;
        let mut last_end: Option<Instant> = None;
        loop {
            // A frame to start: the one prepared during the last transfer,
            // or one made now (new input, or a dynamic render).
            let cur = match next.take() {
                Some(p) => p,
                None => {
                    if !cfg.dynamic(&inp, Instant::now()) {
                        match select3(
                            FRAME_READY.wait(),
                            settings::LED_CHANGED.wait(),
                            Timer::after(Duration::from_millis(5)),
                        )
                        .await
                        {
                            Either3::Second(()) => break,
                            Either3::First(()) | Either3::Third(()) => {}
                        }
                    } else if settings::LED_CHANGED.signaled() {
                        break;
                    }
                    beat();
                    check_timeout();
                    stats.maybe_report();
                    let fresh_input = inp.take(&mut seen_seq, &lens);
                    if !fresh_input && !cfg.dynamic(&inp, Instant::now()) {
                        continue;
                    }
                    let Some(ring) = spares[0].take().or_else(|| spares[1].take()) else {
                        unreachable!("both rings are idle between transfers")
                    };
                    frame = frame.wrapping_add(1);
                    match prepare(ring, &mut inp, &cfg, frame, &lens, shape, s.latch_us) {
                        Some(p) => p,
                        None => {
                            // No input yet; prepare() only fails then.
                            unreachable!("prepare has input after take or in dynamic mode")
                        }
                    }
                }
            };

            let mut cur = cur;
            cur.fill_all();
            // The latch pause must be on the wire in full before the next
            // frame: "done" comes when the DMA has handed over its last data,
            // while the I2S FIFO still holds part of the trailing zeros, and
            // starting the next transfer can cut those short. Back-to-back
            // frames (interpolation, dithering) then had too short a reset,
            // and some LEDs took the wrong 32 bits (ghost LEDs further on).
            if let Some(end) = last_end {
                let need = Duration::from_micros(u64::from(s.latch_us));
                let gone = end.elapsed();
                if gone < need {
                    let wait = need - gone;
                    let until = Instant::now() + wait;
                    while Instant::now() < until {}
                }
            }
            let started = Instant::now();
            let Prepared { ring, mut enc, reads, .. } = cur;
            let mut t = match i2s.send(ring) {
                Ok(t) => t,
                Err((e, back, ring)) => {
                    warn!("led: starting the transfer failed: {:?}", e);
                    i2s = back;
                    give_back(&mut spares, ring);
                    inp.busy[reads.0] -= 1;
                    inp.busy[reads.1] -= 1;
                    continue;
                }
            };
            let mut ok = true;
            loop {
                if !enc.done() {
                    if t.is_done() {
                        ok = false; // the DMA ran dry mid-frame
                        break;
                    }
                    if t.available_bytes() >= CHUNK {
                        t.push_with(|dst| enc.fill(dst));
                        continue;
                    }
                } else if t.is_done() {
                    break;
                }
                // Spare time: get the next frame ready in the idle ring, so
                // it starts the moment this one ends.
                if let Some(n) = next.as_mut()
                    && !n.full
                {
                    n.fill_some();
                    continue;
                }
                if next.is_none() {
                    let fresh_input = inp.take(&mut seen_seq, &lens);
                    if fresh_input || cfg.dynamic(&inp, Instant::now()) {
                        let Some(ring) = spares[0].take().or_else(|| spares[1].take()) else {
                            unreachable!("one ring is idle during a transfer")
                        };
                        frame = frame.wrapping_add(1);
                        match prepare(ring, &mut inp, &cfg, frame, &lens, shape, s.latch_us) {
                            Some(p) => next = Some(p),
                            None => unreachable!("prepare has input here"),
                        }
                        continue;
                    }
                }
                beat();
                check_timeout();
                stats.maybe_report();
                if enc.done() {
                    // Only the tail is left: check again shortly. (Waiting for
                    // the DMA's end interrupt missed it whenever it fired while
                    // this loop was busy, costing up to 5 ms per frame.)
                    Timer::after(Duration::from_micros(100)).await;
                } else {
                    let _ = select(FRAME_READY.wait(), Timer::after(Duration::from_micros(300))).await;
                }
            }
            let (back, ring) = t.wait();
            last_end = Some(Instant::now());
            i2s = back;
            give_back(&mut spares, ring);
            inp.busy[reads.0] -= 1;
            inp.busy[reads.1] -= 1;
            stats.frame(ok, started.elapsed());
            if settings::LED_CHANGED.signaled() {
                break;
            }
        }

        // Settings changed: keep the rings, rebuild the driver with the new
        // timing.
        if let Some(p) = next.take() {
            give_back(&mut spares, p.ring);
        }
        drop(i2s);
        info!("led: settings changed; applying");
    }
}

/// Plays a ring full of zeros and measures how long it takes.
async fn measure_rate(out: &mut Out<'_>) -> u32 {
    let (Some(i2s), Some(buf)) = (out.i2s.take(), out.buf.take()) else {
        return 0;
    };
    let mut buf = fresh(buf);
    let n = buf.push_with(|dst| {
        dst.fill(0);
        dst.len() / 4 * 4
    });
    let started = Instant::now();
    let t = match i2s.send(buf) {
        Ok(t) => t,
        Err((_, i2s, buf)) => {
            out.i2s = Some(i2s);
            out.buf = Some(buf);
            return 0;
        }
    };
    while !t.is_done() {
        Timer::after(Duration::from_micros(200)).await;
    }
    let us = started.elapsed().as_micros().max(1);
    let (i2s, buf) = t.wait();
    out.i2s = Some(i2s);
    out.buf = Some(buf);
    (n as u64 * 1_000_000 / us) as u32
}

/// Output statistics for the status page.
struct OutStats {
    window: Instant,
    frames: u32,
    underruns: u32,
    max_frame_us: u64,
}

impl OutStats {
    fn new() -> Self {
        Self { window: Instant::now(), frames: 0, underruns: 0, max_frame_us: 0 }
    }

    fn frame(&mut self, ok: bool, took: Duration) {
        self.frames += 1;
        if !ok {
            // Logged once per window (`maybe_report`), not here: a log line
            // keeps this core's interrupts off while the UART drains, which
            // can starve the next frames too.
            self.underruns += 1;
        }
        self.max_frame_us = self.max_frame_us.max(took.as_micros());
        self.maybe_report();
    }

    fn maybe_report(&mut self) {
        let el = self.window.elapsed();
        if el < Duration::from_secs(1) {
            return;
        }
        let fps = (u64::from(self.frames) * 1000 / el.as_millis().max(1)) as u32;
        let (u, mf) = (self.underruns, self.max_frame_us as u32);
        if u > 0 {
            warn!("led: {} underrun(s) in the last second (the DMA ran dry mid-frame)", u);
        }
        status::update(|st| {
            st.led_fps = fps;
            st.led_underruns = st.led_underruns.wrapping_add(u);
            st.led_frame_us = mf;
        });
        *self = Self::new();
    }
}

