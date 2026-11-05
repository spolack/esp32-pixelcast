//! Why the board last restarted, for the status page.
//!
//! A record in RTC memory survives panics, watchdog resets and restarts
//! (not a power cycle). The panic handler writes the message and a short
//! backtrace there; planned restarts (web, update, rollback) write their
//! reason first; anything else is read from the chip's reset reason.
//! [`boot`] turns the record into the "last restart" shown on the status
//! page and counts the unexpected ones since power-on.

use core::{
    fmt::Write,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

const MAGIC: u32 = 0x4352_4153; // "CRAS"
/// In place of [`MAGIC`] while the other firmware runs: it may use this
/// RTC memory too, so the rest of the record means nothing on return.
const LEFT: u32 = 0x4c45_4654; // "LEFT"
static LEAVING: AtomicBool = AtomicBool::new(false);

/// Call once the other firmware is selected to start next: the
/// restart into it and the way back are not reported as crashes.
pub fn leaving() {
    LEAVING.store(true, Ordering::Relaxed);
    put(0, LEFT);
}
const MSG: usize = 160;
const PCS: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Kind {
    PowerOn = 0,
    /// A planned restart (the message says which).
    Planned = 1,
    Panic = 2,
    Watchdog = 3,
    /// The supply voltage dipped.
    Brownout = 4,
    /// A reset this firmware did not ask for (other reason code).
    Other = 5,
}

impl Kind {
    fn from(v: u32) -> Kind {
        match v {
            1 => Kind::Planned,
            2 => Kind::Panic,
            3 => Kind::Watchdog,
            4 => Kind::Brownout,
            5 => Kind::Other,
            _ => Kind::PowerOn,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Kind::PowerOn => "power_on",
            Kind::Planned => "planned",
            Kind::Panic => "panic",
            Kind::Watchdog => "watchdog",
            Kind::Brownout => "brownout",
            Kind::Other => "other",
        }
    }
    pub fn unexpected(self) -> bool {
        !matches!(self, Kind::PowerOn | Kind::Planned)
    }
}

/// The record in RTC memory (plain words: allowed as persistent data).
/// 0 magic, 1 pending kind (0 = none), 2 uptime at the event, 3 uptime
/// heartbeat, 4 unexpected restarts since power-on, 5 hardware reset reason,
/// 6..6+PCS backtrace, then the message length and bytes.
const W_PEND: usize = 1;
const W_UP_AT: usize = 2;
const W_UP: usize = 3;
const W_COUNT: usize = 4;
const W_HW: usize = 5;
const W_PC: usize = 6;
const W_LEN: usize = W_PC + PCS;
const W_MSG: usize = W_LEN + 1;
const WORDS: usize = W_MSG + MSG / 4;

#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut REC: [u32; WORDS] = [0; WORDS];

fn get(i: usize) -> u32 {
    // SAFETY: `i < WORDS` at every call site (fixed indexes and loops over
    // the record); plain words in RTC memory, any value is valid. Volatile:
    // the record must reach memory before a reset.
    unsafe { core::ptr::read_volatile((&raw const REC).cast::<u32>().add(i)) }
}
fn put(i: usize, v: u32) {
    // SAFETY: as in `get`. Writers (start-up, planned restarts, the panic
    // handler) do not overlap in practice; a torn record only garbles the
    // message shown.
    unsafe { core::ptr::write_volatile((&raw mut REC).cast::<u32>().add(i), v) }
}

fn set_msg(m: &[u8]) {
    let n = m.len().min(MSG);
    for w in 0..MSG / 4 {
        let mut b = [0u8; 4];
        for (k, x) in b.iter_mut().enumerate() {
            *x = m.get(w * 4 + k).copied().filter(|_| w * 4 + k < n).unwrap_or(0);
        }
        put(W_MSG + w, u32::from_le_bytes(b));
    }
    put(W_LEN, n as u32);
}

fn msg_into(out: &mut [u8; MSG]) -> usize {
    let n = (get(W_LEN) as usize).min(MSG);
    for w in 0..MSG / 4 {
        out[w * 4..w * 4 + 4].copy_from_slice(&get(W_MSG + w).to_le_bytes());
    }
    n
}

fn ensure() {
    if LEAVING.load(Ordering::Relaxed) {
        return;
    }
    if get(0) != MAGIC {
        for i in 0..WORDS {
            put(i, 0);
        }
        put(0, MAGIC);
    }
}

/// Seconds since start; call about once a second so a watchdog reset still
/// says how long the board ran.
pub fn tick(uptime_s: u32) {
    put(W_UP, uptime_s);
}

/// Like [`planned`], unless a more specific reason is already recorded.
pub fn planned_default(why: &str) {
    ensure();
    if get(W_PEND) == 0 {
        planned(why);
    }
}

/// Call right before a planned restart.
pub fn planned(why: &str) {
    ensure();
    set_msg(why.as_bytes());
    put(W_UP_AT, get(W_UP));
    for i in 0..PCS {
        put(W_PC + i, 0);
    }
    put(W_PEND, Kind::Planned as u32);
}

/// Call when this firmware lets the watchdog restart the board on purpose
/// (a stuck core): counts as a watchdog restart, with the reason.
pub fn watchdog(why: &str) {
    ensure();
    set_msg(why.as_bytes());
    put(W_UP_AT, get(W_UP));
    for i in 0..PCS {
        put(W_PC + i, 0);
    }
    put(W_PEND, Kind::Watchdog as u32);
}

/// A fixed buffer that `write!` can fill without the heap.
struct Buf {
    b: [u8; MSG],
    n: usize,
}
impl Write for Buf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &c in s.as_bytes() {
            if self.n < MSG {
                self.b[self.n] = if (0x20..0x7f).contains(&c) { c } else { b'?' };
                self.n += 1;
            }
        }
        Ok(())
    }
}

/// From the panic handler: message, place and a short backtrace.
pub fn panic(info: &core::panic::PanicInfo) {
    ensure();
    let mut b = Buf { b: [0; MSG], n: 0 };
    let _ = write!(b, "{}", info.message());
    if let Some(l) = info.location() {
        let file = l.file().rsplit('/').next().unwrap_or(l.file());
        let _ = write!(b, " ({}:{})", file, l.line());
    }
    set_msg(&b.b[..b.n]);
    let bt = esp_backtrace::Backtrace::capture();
    for i in 0..PCS {
        put(W_PC + i, bt.frames().get(i).map_or(0, |f| f.program_counter() as u32));
    }
    put(W_UP_AT, get(W_UP));
    put(W_PEND, Kind::Panic as u32);
}

/// A planned restart caused by the restart [`boot`] just classified (the
/// rollback after a crash on trial): the new record keeps that one's kind,
/// message and backtrace, after `why`, so the cause survives the rollback.
pub fn planned_keeping_last(why: &str) {
    ensure();
    let kind = Kind::from(LAST_KIND.load(Ordering::Relaxed));
    let (m, n) = critical_section::with(|cs| *LAST_MSG.borrow_ref(cs));
    let mut b = Buf { b: [0; MSG], n: 0 };
    let _ = write!(b, "{} (before: {}", why, kind.key());
    if n > 0 {
        let _ = write!(b, ": {}", core::str::from_utf8(&m[..n]).unwrap_or(""));
    }
    let _ = write!(b, ")");
    set_msg(&b.b[..b.n]);
    put(W_UP_AT, LAST_UP.load(Ordering::Relaxed));
    for (i, pc) in LAST_PC.iter().enumerate() {
        put(W_PC + i, pc.load(Ordering::Relaxed));
    }
    put(W_PEND, Kind::Planned as u32);
}

/// The kind of the restart [`boot`] classified.
pub fn last_kind() -> Kind {
    Kind::from(LAST_KIND.load(Ordering::Relaxed))
}

/// What [`boot`] found, kept for the status page.
static LAST_KIND: AtomicU32 = AtomicU32::new(0);
static LAST_UP: AtomicU32 = AtomicU32::new(0);
static LAST_HW: AtomicU32 = AtomicU32::new(0);
static COUNT: AtomicU32 = AtomicU32::new(0);
static LAST_PC: [AtomicU32; PCS] = [const { AtomicU32::new(0) }; PCS];
static LAST_MSG: critical_section::Mutex<core::cell::RefCell<([u8; MSG], usize)>> =
    critical_section::Mutex::new(core::cell::RefCell::new(([0; MSG], 0)));

/// Call once at start-up: classifies the restart that just happened.
pub fn boot() {
    use esp_hal::rtc_cntl::SocResetReason as R;
    let hw = esp_hal::system::reset_reason();
    if get(0) == LEFT {
        // Back from the other firmware: a planned restart, the rest is stale.
        for i in 0..WORDS {
            put(i, 0);
        }
        put(0, MAGIC);
        set_msg(b"Back from the other firmware");
        put(W_PEND, Kind::Planned as u32);
    }
    let power_on = matches!(hw, None | Some(R::ChipPowerOn)) || get(0) != MAGIC;
    if power_on {
        for i in 0..WORDS {
            put(i, 0);
        }
        put(0, MAGIC);
    }
    let hw_code = hw.map_or(0, |r| r as u32);
    let kind = match (power_on, get(W_PEND)) {
        (true, _) => Kind::PowerOn,
        (false, p) if p != 0 => Kind::from(p),
        (false, _) => match hw {
            Some(R::CoreMwdt0 | R::CoreMwdt1 | R::CoreRtcWdt | R::CpuMwdt0 | R::Cpu0RtcWdt) => Kind::Watchdog,
            Some(R::SysBrownOut) => Kind::Brownout,
            _ => Kind::Other,
        },
    };
    let up_at = if get(W_PEND) != 0 { get(W_UP_AT) } else { get(W_UP) };
    if kind.unexpected() {
        put(W_COUNT, get(W_COUNT) + 1);
    }
    let mut m = [0u8; MSG];
    let n = if get(W_PEND) != 0 { msg_into(&mut m) } else { 0 };
    critical_section::with(|cs| *LAST_MSG.borrow_ref_mut(cs) = (m, n));
    for i in 0..PCS {
        LAST_PC[i].store(if get(W_PEND) != 0 { get(W_PC + i) } else { 0 }, Ordering::Relaxed);
    }
    LAST_KIND.store(kind as u32, Ordering::Relaxed);
    LAST_UP.store(up_at, Ordering::Relaxed);
    LAST_HW.store(hw_code, Ordering::Relaxed);
    COUNT.store(get(W_COUNT), Ordering::Relaxed);
    put(W_HW, hw_code);
    put(W_PEND, 0);
    put(W_UP, 0);
    if kind.unexpected() {
        log::error!("crashlog: last restart was unexpected ({}) after {} s", kind.key(), up_at);
    }
}

/// The last restart as JSON: kind, uptime before it (s), message, backtrace
/// (hex program counters), hardware reset code, unexpected restarts since
/// power-on.
pub fn json() -> alloc::string::String {
    let kind = Kind::from(LAST_KIND.load(Ordering::Relaxed));
    let (m, n) = critical_section::with(|cs| *LAST_MSG.borrow_ref(cs));
    let msg = core::str::from_utf8(&m[..n]).unwrap_or("");
    crate::json::object(|o| {
        o.field("kind", kind.key())
            .field("uptime_s", LAST_UP.load(Ordering::Relaxed))
            .field("message", msg)
            .arr("backtrace", |a| {
                for pc in LAST_PC.iter().map(|p| p.load(Ordering::Relaxed)).filter(|&p| p != 0) {
                    a.item(alloc::format!("0x{:08x}", pc));
                }
            })
            .field("hw_reason", LAST_HW.load(Ordering::Relaxed))
            .field("unexpected_since_power_on", COUNT.load(Ordering::Relaxed));
    })
}
