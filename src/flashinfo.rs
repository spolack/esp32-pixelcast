//! The flash chip and its interface: a read-only probe for the status page,
//! and the optional switch to QIO at 80 MHz (`fast_flash` setting).
//!
//! The bootloader starts the flash at DIO 40 MHz (from its own header, which
//! an update cannot change). The switch only rewrites the cache's SPI
//! registers after boot, so every restart begins at DIO 40 again. It needs the
//! chip's QE bit already set (it is never written here). Safety:
//! - the switch reads 35 KB of flash back and compares it with the reading at
//!   40 MHz; on any difference it returns to DIO 40 at once;
//! - a panic or watchdog reset while running fast turns it off until the next
//!   power-on (marker in RTC memory), so a marginal flash cannot loop;
//! - it runs after the update trial check, so a new image that hangs is still
//!   rolled back.

use core::sync::atomic::{AtomicU32, Ordering};

#[repr(C)]
struct RomSpiflashChip {
    device_id: u32,
    chip_size: u32,
    block_size: u32,
    sector_size: u32,
    page_size: u32,
    status_mask: u32,
}

unsafe extern "C" {
    static mut g_rom_flashchip: RomSpiflashChip;
    static mut g_rom_spiflash_dummy_len_plus: [u8; 4];
    fn Cache_Read_Disable_rom(cpu: u32);
    fn Cache_Flush_rom(cpu: u32);
    fn Cache_Read_Enable_rom(cpu: u32);
    fn esp_rom_spiflash_read_status(chip: *mut RomSpiflashChip, status: *mut u32) -> i32;
    fn esp_rom_spiflash_read_statushigh(chip: *mut RomSpiflashChip, status: *mut u32) -> i32;
}

const SPI0: u32 = 0x3FF4_3000;
const CTRL: u32 = SPI0 + 0x08;
const CLOCK: u32 = SPI0 + 0x18;
const USER1: u32 = SPI0 + 0x20;
const USER2: u32 = SPI0 + 0x24;
const EXT2_SPI1: u32 = 0x3FF4_2000 + 0xF8;
const EXT2_SPI0: u32 = SPI0 + 0xF8;
const FREAD_QIO: u32 = 1 << 24;
const FREAD_DIO: u32 = 1 << 23;
const FREAD_QUAD: u32 = 1 << 20;
const FREAD_DUAL: u32 = 1 << 14;
const FASTRD_MODE: u32 = 1 << 13;
const QE: u32 = 1 << 9;
const IO_MUX_GPIO6: u32 = 0x3FF4_9060; // flash clock pin

/// id, status low, status high, SPI0 CTRL, CLOCK, USER, USER1, IO_MUX clk pin,
/// dummy_len_plus[0..2], status_mask.
static INFO: [AtomicU32; 11] = [const { AtomicU32::new(0) }; 11];

#[inline(always)]
fn reg(a: u32) -> u32 {
    // SAFETY: only called with the SPI0/SPI1/IO_MUX register addresses
    // above, or with flash-mapped data words (`sum`); all are readable.
    unsafe { core::ptr::read_volatile(a as *const u32) }
}

#[inline(always)]
fn set(a: u32, v: u32) {
    // SAFETY: only called with the SPI0 and IO_MUX register addresses above,
    // inside `switch` (cache off, nothing running from flash).
    unsafe { core::ptr::write_volatile(a as *mut u32, v) }
}

/// Reads the values once. Call at start-up while only this core runs.
#[esp_hal::ram]
pub fn probe() {
    let mut v = [0u32; 11];
    // SAFETY: the ROM's flash descriptor and status reads; at start-up only
    // this core runs and the critical section keeps interrupts out.
    critical_section::with(|_| unsafe {
        let chip = &raw mut g_rom_flashchip;
        v[0] = (*chip).device_id;
        let mut s = 0;
        if esp_rom_spiflash_read_status(chip, &mut s) == 0 {
            v[1] = s;
        }
        let mut h = 0;
        if esp_rom_spiflash_read_statushigh(chip, &mut h) == 0 {
            v[2] = h;
        }
        v[3] = reg(SPI0 + 0x08);
        v[4] = reg(SPI0 + 0x18);
        v[5] = reg(SPI0 + 0x1C);
        v[6] = reg(SPI0 + 0x20);
        v[7] = reg(IO_MUX_GPIO6);
        let d = &raw mut g_rom_spiflash_dummy_len_plus;
        v[8] = u32::from((*d)[0]);
        v[9] = u32::from((*d)[1]);
        v[10] = (*chip).status_mask;
    });
    for (a, x) in INFO.iter().zip(v) {
        a.store(x, Ordering::Relaxed);
    }
}

/// The values as a JSON object (hex strings).
pub fn json() -> alloc::string::String {
    let v: [u32; 11] = core::array::from_fn(|i| INFO[i].load(Ordering::Relaxed));
    alloc::format!(
        "{{\"fast\":\"{}\",\"id\":\"{:06x}\",\"sr\":\"{:x}\",\"srh\":\"{:x}\",\"ctrl\":\"{:08x}\",\"clock\":\"{:08x}\",\"user\":\"{:08x}\",\"user1\":\"{:08x}\",\"clk_pin\":\"{:08x}\",\"dummy_plus\":[{},{}],\"status_mask\":\"{:x}\",\"now_ctrl\":\"{:08x}\",\"now_clock\":\"{:08x}\"}}",
        fast().key(),
        v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8], v[9], v[10],
        reg(CTRL),
        reg(CLOCK)
    )
}

// ── Fast mode ───────────────────────────────────────────────────────────────

/// Kept across panics and watchdog resets (zeroed at power-on).
#[derive(Clone, Copy)]
struct Marker {
    /// The last run had switched to fast mode.
    fast: u32,
    /// A crash happened while fast: stay at DIO 40 until power-on.
    bad: u32,
}
const MAGIC: u32 = 0x464C_5348; // "FLSH"

/// magic, fast, bad.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut MARK: [u32; 3] = [0; 3];

fn mark() -> Marker {
    // SAFETY: plain words in RTC memory; any bit pattern is valid (the
    // magic tells a real marker from leftovers).
    let m = unsafe { core::ptr::read_volatile(&raw const MARK) };
    if m[0] == MAGIC { Marker { fast: m[1], bad: m[2] } } else { Marker { fast: 0, bad: 0 } }
}

fn set_mark(m: Marker) {
    // SAFETY: as in `mark`; written from start-up and the panic handler,
    // which never run at the same time.
    unsafe { core::ptr::write_volatile(&raw mut MARK, [MAGIC, m.fast, m.bad]) }
}

/// What happened at start-up, for the status page.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Fast {
    Off = 0,
    On = 1,
    /// The QE bit is not set: QIO would need a status-register write.
    NoQe = 2,
    /// The read-back at 80 MHz differed; stayed at DIO 40.
    VerifyFailed = 3,
    /// An earlier run crashed while fast; stays off until power-on.
    AfterCrash = 4,
}
static FAST: AtomicU32 = AtomicU32::new(Fast::Off as u32);

impl Fast {
    pub fn key(self) -> &'static str {
        match self {
            Fast::Off => "off",
            Fast::On => "qio80",
            Fast::NoQe => "no_qe",
            Fast::VerifyFailed => "verify_failed",
            Fast::AfterCrash => "after_crash",
        }
    }
}

pub fn fast() -> Fast {
    match FAST.load(Ordering::Relaxed) {
        1 => Fast::On,
        2 => Fast::NoQe,
        3 => Fast::VerifyFailed,
        4 => Fast::AfterCrash,
        _ => Fast::Off,
    }
}

/// Call from the panic handler: a crash while fast disables fast mode.
pub fn note_crash() {
    let mut m = mark();
    if m.fast != 0 {
        m.bad = 1;
        set_mark(m);
    }
}

/// Clears the crash marker so the next start tries again (setting re-saved).
pub fn retry() {
    let mut m = mark();
    m.bad = 0;
    set_mark(m);
}

/// Switches to QIO 80 MHz if `want` and it is safe. Call once at start-up,
/// after [`probe`], with the watchdog running and before the second core
/// starts. `check` are flash-resident data (together more than the 32 KB
/// cache) read back to verify the switch.
pub fn start(want: bool, check: &[&'static [u8]]) {
    use esp_hal::rtc_cntl::SocResetReason as R;
    let mut m = mark();
    let crashed = matches!(
        esp_hal::system::reset_reason(),
        Some(R::CoreMwdt0 | R::CoreMwdt1 | R::CoreRtcWdt | R::CpuMwdt0 | R::Cpu0RtcWdt | R::SysBrownOut)
    );
    if m.fast != 0 && crashed {
        m.bad = 1;
    }
    m.fast = 0;
    set_mark(m);
    let r = if !want {
        Fast::Off
    } else if m.bad != 0 {
        Fast::AfterCrash
    } else if INFO[2].load(Ordering::Relaxed) & QE == 0 {
        Fast::NoQe
    } else {
        let mut parts = [(0u32, 0u32); 4];
        for (p, c) in parts.iter_mut().zip(check) {
            *p = (c.as_ptr() as u32 & !3, (c.len() as u32) / 4);
        }
        if switch(&parts) {
            m.fast = 1;
            set_mark(m);
            Fast::On
        } else {
            Fast::VerifyFailed
        }
    };
    FAST.store(r as u32, Ordering::Relaxed);
}

/// Hash over words at the given (address, word count) pairs, read through
/// the cache. Plain loops, inlined, so all of it runs from IRAM.
#[inline(always)]
fn sum(parts: &[(u32, u32); 4]) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    let mut k = 0;
    while k < 4 {
        let (a, n) = parts[k];
        let mut i = 0;
        while i < n {
            h = (h ^ reg(a + 4 * i)).wrapping_mul(0x0100_0193);
            i += 1;
        }
        k += 1;
    }
    h
}

/// Between `Cache_Read_Disable` and `Cache_Read_Enable` nothing may run from
/// flash: no closures, no calls except ROM, interrupts off.
#[esp_hal::ram]
fn switch(parts: &[(u32, u32); 4]) -> bool {
    // SAFETY: the critical section is released below on every path. The
    // block runs from IRAM with interrupts off, so nothing reads flash while
    // the cache is disabled and the SPI0 registers change; the ROM cache
    // calls and the dummy-length table are the ESP32 ROM's own.
    let cs = unsafe { critical_section::acquire() };
    let ok;
    unsafe {
        let dummy = &raw mut g_rom_spiflash_dummy_len_plus;
        let old = (reg(CTRL), reg(CLOCK), reg(USER1), reg(USER2), reg(IO_MUX_GPIO6), (*dummy)[0]);
        Cache_Read_Disable_rom(0);
        Cache_Flush_rom(0);
        Cache_Read_Enable_rom(0);
        let before = sum(parts);

        Cache_Read_Disable_rom(0);
        while reg(EXT2_SPI1) & 7 != 0 {}
        while reg(EXT2_SPI0) & 7 != 0 {}
        // QIO read (0xEB): 24 address + 8 mode bits on four lines, then
        // 3 dummy cycles + 2 extra at 80 MHz (ESP-IDF bootloader values).
        let mode = FREAD_QIO | FREAD_QUAD | FREAD_DIO | FREAD_DUAL | FASTRD_MODE;
        set(CTRL, (old.0 & !mode) | FREAD_QIO | FASTRD_MODE);
        (*dummy)[0] = 2;
        set(USER1, (old.2 & !((0x3F << 26) | 0xFF)) | (31 << 26) | (3 + 2));
        set(USER2, (old.3 & !0xFFFF) | 0xEB);
        set(IO_MUX_GPIO6, old.4 | (3 << 10)); // clock pin drive 3, as at 80 MHz
        set(CLOCK, 1 << 31); // SPI clock = APB (80 MHz)
        Cache_Flush_rom(0);
        Cache_Read_Enable_rom(0);

        ok = sum(parts) == before && sum(parts) == before;
        if !ok {
            Cache_Read_Disable_rom(0);
            while reg(EXT2_SPI0) & 7 != 0 {}
            set(CLOCK, old.1);
            set(CTRL, old.0);
            set(USER1, old.2);
            set(USER2, old.3);
            set(IO_MUX_GPIO6, old.4);
            (*dummy)[0] = old.5;
            Cache_Flush_rom(0);
            Cache_Read_Enable_rom(0);
        }
        critical_section::release(cs);
    }
    ok
}
