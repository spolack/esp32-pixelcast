//! In-place updater (see `core/src/inplace.rs` for the whole picture).
//!
//! Started by the bootloader with the slot's flash offset as argument. Runs
//! from RAM only, so it can rewrite the slot it was loaded from. Brings the
//! W5500 up in its hardware TCP mode at the address the firmware handed
//! over, takes `POST /update` (with the token from the record), writes the
//! image from the start of the slot, marks the record "written" and
//! restarts; the bootloader then checks the image and selects it.
//!
//! Gives up (erases the record, leaves a note, restarts into the old
//! firmware) when no upload arrives within 10 minutes and the slot is still
//! untouched. Once it started writing there is no way back: it waits for
//! the next upload; BOOT within 2 s of a power-on starts the other slot.
#![no_std]
#![no_main]

// Links IDF's patched flash routines and the ROM address scripts.
use esp_rom_sys as _;

#[path = "../../core/src/inplace.rs"]
#[allow(dead_code)]
mod inplace;

use core::ptr::{read_volatile, write_volatile};
use inplace::{Record, State};

// ── ROM ─────────────────────────────────────────────────────────────────────

unsafe extern "C" {
    fn ets_printf(fmt: *const u8, ...) -> i32;
    fn ets_delay_us(us: u32);
    fn esp_rom_spiflash_erase_sector(sector: u32) -> i32;
    fn esp_rom_spiflash_read(src: u32, dst: *mut u32, len: u32) -> i32;
    fn esp_rom_spiflash_write(dst: u32, src: *const u32, len: u32) -> i32;
    fn esp_rom_spiflash_unlock() -> i32;
    fn Cache_Read_Disable_rom(cpu: u32);
}

macro_rules! log {
    ($fmt:literal $(, $a:expr)*) => {
        unsafe { ets_printf(concat!("updater: ", $fmt, "\n\0").as_ptr() $(, $a)*) }
    };
}

fn reg_w(addr: u32, v: u32) {
    unsafe { write_volatile(addr as *mut u32, v) }
}
fn reg_r(addr: u32) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}
fn delay_us(us: u32) {
    unsafe { ets_delay_us(us) }
}
/// Full system reset (RTC_CNTL_SW_SYS_RST), as the firmware restarts.
fn reset() -> ! {
    delay_us(20_000); // let the UART drain
    reg_w(0x3FF4_8000, reg_r(0x3FF4_8000) | (1 << 31));
    loop {}
}

// ── Watchdog: the bootloader armed the RTC one (9 s); feed it ───────────────

const RTC_WDT_FEED: u32 = 0x3FF4_80A0;
const RTC_WDT_WPROTECT: u32 = 0x3FF4_80A4;
const WDT_KEY: u32 = 0x50D8_3AA1;

fn feed() {
    reg_w(RTC_WDT_WPROTECT, WDT_KEY);
    reg_w(RTC_WDT_FEED, 1 << 31);
    reg_w(RTC_WDT_WPROTECT, 0);
}

// ── Flash (ROM routines; nothing runs from flash here) ──────────────────────

const SECTOR: u32 = 0x1000;

#[repr(C, align(4))]
struct Sector([u8; SECTOR as usize]);

fn flash_read(addr: u32, buf: &mut [u8]) -> bool {
    // The ROM wants word-aligned lengths and buffers.
    let mut words = [0u32; 16];
    let mut done = 0;
    while done < buf.len() {
        let n = (buf.len() - done).min(64);
        let len = (n as u32 + 3) & !3;
        if unsafe { esp_rom_spiflash_read(addr + done as u32, words.as_mut_ptr(), len) } != 0 {
            return false;
        }
        for i in 0..n {
            buf[done + i] = (words[i / 4] >> ((i % 4) * 8)) as u8;
        }
        done += n;
    }
    true
}

/// Erases the sector at `addr` and writes `data` (at most one sector) there.
fn flash_put(addr: u32, data: &Sector, len: usize) -> bool {
    feed();
    if unsafe { esp_rom_spiflash_erase_sector(addr / SECTOR) } != 0 {
        return false;
    }
    let len = ((len as u32) + 3) & !3;
    len == 0 || unsafe { esp_rom_spiflash_write(addr, data.0.as_ptr() as *const u32, len) } == 0
}

static mut BUF: Sector = Sector([0; SECTOR as usize]);

/// The sector buffer. Single-threaded, no interrupts: one user at a time.
#[allow(static_mut_refs)]
fn buf() -> &'static mut Sector {
    unsafe { &mut BUF }
}

/// Writes the record (and keeps the note area empty).
fn put_record(at: u32, r: &Record) -> bool {
    let b = buf();
    b.0.fill(0xFF);
    b.0[..inplace::LEN].copy_from_slice(&r.encode());
    flash_put(at, b, inplace::LEN)
}

/// Erases the record and leaves `note` for the firmware to show.
fn give_up(at: u32, note: &[u8]) -> ! {
    log!("giving up");
    let b = buf();
    b.0.fill(0xFF);
    let n = note.len().min(inplace::NOTE_MAX - 1);
    let off = inplace::NOTE as usize;
    b.0[off..off + n].copy_from_slice(&note[..n]);
    b.0[off + n] = 0;
    flash_put(at, b, off + n + 1);
    reset()
}

// ── SPI3 (VSPI) master: SCK 18, MISO 19, MOSI 23; CS 5 by hand ──────────────

const SPI: u32 = 0x3FF6_5000;
const SPI_CMD: u32 = SPI;
const SPI_CTRL: u32 = SPI + 0x08;
const SPI_CTRL2: u32 = SPI + 0x14;
const SPI_CLOCK: u32 = SPI + 0x18;
const SPI_USER: u32 = SPI + 0x1C;
const SPI_USER1: u32 = SPI + 0x20;
const SPI_USER2: u32 = SPI + 0x24;
const SPI_MOSI_DLEN: u32 = SPI + 0x28;
const SPI_MISO_DLEN: u32 = SPI + 0x2C;
const SPI_PIN: u32 = SPI + 0x34;
const SPI_SLAVE: u32 = SPI + 0x38;
const SPI_W0: u32 = SPI + 0x80;
const SPI_USR: u32 = 1 << 18;
const USR_MISO: u32 = 1 << 28;
const USR_MOSI: u32 = 1 << 27;

const DPORT_PERIP_CLK_EN: u32 = 0x3FF0_00C0;
const DPORT_PERIP_RST_EN: u32 = 0x3FF0_00C4;
const DPORT_SPI3: u32 = 1 << 16;

const GPIO_OUT_W1TS: u32 = 0x3FF4_4008;
const GPIO_OUT_W1TC: u32 = 0x3FF4_400C;
const GPIO_ENABLE_W1TS: u32 = 0x3FF4_4024;
const GPIO_FUNC_IN_SEL: u32 = 0x3FF4_4130;
const GPIO_FUNC_OUT_SEL: u32 = 0x3FF4_4530;
const IO_MUX: u32 = 0x3FF4_9000;

const PIN_SCK: u32 = 18;
const PIN_MISO: u32 = 19;
const PIN_MOSI: u32 = 23;
const PIN_CS: u32 = 5;

fn io_mux_reg(pin: u32) -> u32 {
    IO_MUX
        + match pin {
            5 => 0x6C,
            18 => 0x70,
            19 => 0x74,
            23 => 0x8C,
            _ => 0x6C,
        }
}

/// Pin as a GPIO (IO_MUX function 2) driven through the GPIO matrix.
fn pin_out(pin: u32, signal: u32) {
    reg_w(io_mux_reg(pin), (2 << 12) | (2 << 10)); // GPIO function, drive 2
    reg_w(GPIO_FUNC_OUT_SEL + 4 * pin, signal); // OEN from the enable register
    reg_w(GPIO_ENABLE_W1TS, 1 << pin);
}

fn spi_init() {
    reg_w(DPORT_PERIP_CLK_EN, reg_r(DPORT_PERIP_CLK_EN) | DPORT_SPI3);
    reg_w(DPORT_PERIP_RST_EN, reg_r(DPORT_PERIP_RST_EN) | DPORT_SPI3);
    reg_w(DPORT_PERIP_RST_EN, reg_r(DPORT_PERIP_RST_EN) & !DPORT_SPI3);

    reg_w(GPIO_OUT_W1TS, 1 << PIN_CS);
    pin_out(PIN_CS, 0x100); // plain GPIO output
    pin_out(PIN_SCK, 63); // VSPICLK_OUT
    pin_out(PIN_MOSI, 65); // VSPID_OUT
    reg_w(io_mux_reg(PIN_MISO), (2 << 12) | (1 << 9)); // GPIO function, input
    reg_w(GPIO_FUNC_IN_SEL + 4 * 64, PIN_MISO | (1 << 7)); // VSPIQ_IN from pin 19

    reg_w(SPI_SLAVE, 0); // master
    reg_w(SPI_PIN, 0b111); // no hardware CS; mode 0 (clock idles low)
    reg_w(SPI_CTRL, 0); // MSB first
    reg_w(SPI_CTRL2, 0);
    reg_w(SPI_USER1, 0);
    reg_w(SPI_USER2, 0);
    // 80 MHz APB / 20 = 4 MHz: N = 19, H = 9, L = 19 (a 40 MHz APB halves
    // it). Slow enough that MISO, sampled on the default edge through the
    // GPIO matrix, needs no delay tuning; still ~2 s for a 1 MB image.
    reg_w(SPI_CLOCK, (19 << 12) | (9 << 6) | 19);
}

fn spi_wait() {
    while reg_r(SPI_CMD) & SPI_USR != 0 {}
}

/// Sends up to 64 bytes (CS untouched).
fn spi_tx(data: &[u8]) {
    let mut w = [0u32; 16];
    for (i, b) in data.iter().enumerate() {
        w[i / 4] |= u32::from(*b) << ((i % 4) * 8);
    }
    for (i, v) in w.iter().enumerate().take(data.len().div_ceil(4)) {
        reg_w(SPI_W0 + 4 * i as u32, *v);
    }
    reg_w(SPI_USER, USR_MOSI);
    reg_w(SPI_MOSI_DLEN, data.len() as u32 * 8 - 1);
    reg_w(SPI_CMD, SPI_USR);
    spi_wait();
}

/// Receives up to 64 bytes (CS untouched).
fn spi_rx(data: &mut [u8]) {
    reg_w(SPI_USER, USR_MISO);
    reg_w(SPI_MISO_DLEN, data.len() as u32 * 8 - 1);
    reg_w(SPI_CMD, SPI_USR);
    spi_wait();
    for (i, b) in data.iter_mut().enumerate() {
        *b = (reg_r(SPI_W0 + 4 * (i / 4) as u32) >> ((i % 4) * 8)) as u8;
    }
}

// ── W5500 ───────────────────────────────────────────────────────────────────

const COMMON: u8 = 0;
/// Sockets listening on port 80. Two, so a new connection always finds one
/// listening while the other closes (one alone refused the next request).
const SOCKETS: u8 = 2;

/// The socket the functions below work on.
static mut SOCK: u8 = 0;
fn sel(s: u8) {
    unsafe { SOCK = s }
}
fn sreg() -> u8 {
    unsafe { SOCK * 4 + 1 }
}
fn stx() -> u8 {
    unsafe { SOCK * 4 + 2 }
}
fn srx() -> u8 {
    unsafe { SOCK * 4 + 3 }
}

fn w5500_frame(addr: u16, block: u8, write: bool) -> [u8; 3] {
    [(addr >> 8) as u8, addr as u8, (block << 3) | if write { 4 } else { 0 }]
}

fn w_write(block: u8, addr: u16, data: &[u8]) {
    reg_w(GPIO_OUT_W1TC, 1 << PIN_CS);
    spi_tx(&w5500_frame(addr, block, true));
    for c in data.chunks(64) {
        spi_tx(c);
    }
    reg_w(GPIO_OUT_W1TS, 1 << PIN_CS);
}

fn w_read(block: u8, addr: u16, data: &mut [u8]) {
    reg_w(GPIO_OUT_W1TC, 1 << PIN_CS);
    spi_tx(&w5500_frame(addr, block, false));
    for c in data.chunks_mut(64) {
        spi_rx(c);
    }
    reg_w(GPIO_OUT_W1TS, 1 << PIN_CS);
}

fn w_u8(block: u8, addr: u16) -> u8 {
    let mut b = [0];
    w_read(block, addr, &mut b);
    b[0]
}

/// A 16-bit counter register, read until two reads agree (the chip may
/// update it between the two bytes).
fn w_u16(block: u8, addr: u16) -> u16 {
    let mut last = 0xFFFF_FFFFu32;
    loop {
        let mut b = [0; 2];
        w_read(block, addr, &mut b);
        let v = u32::from(u16::from_be_bytes(b));
        if v == last {
            return v as u16;
        }
        last = v;
    }
}

fn w_set16(block: u8, addr: u16, v: u16) {
    w_write(block, addr, &v.to_be_bytes());
}

const SN_MR: u16 = 0x00;
const SN_CR: u16 = 0x01;
const SN_IR: u16 = 0x02;
const IR_SEND_OK: u8 = 0x10;
const SN_SR: u16 = 0x03;
const SN_PORT: u16 = 0x04;
const SN_RXBUF: u16 = 0x1E;
const SN_TXBUF: u16 = 0x1F;
const SN_TX_FSR: u16 = 0x20;
const SN_TX_WR: u16 = 0x24;
const SN_RX_RSR: u16 = 0x26;
const SN_RX_RD: u16 = 0x28;

const CR_OPEN: u8 = 0x01;
const CR_LISTEN: u8 = 0x02;
const CR_DISCON: u8 = 0x08;
const CR_CLOSE: u8 = 0x10;
const CR_SEND: u8 = 0x20;
const CR_RECV: u8 = 0x40;

const SR_CLOSED: u8 = 0x00;
const SR_INIT: u8 = 0x13;
const SR_CLOSE_WAIT: u8 = 0x1C;
const SR_LISTEN: u8 = 0x14;
const SR_ESTABLISHED: u8 = 0x17;

fn command(cr: u8) {
    w_write(sreg(), SN_CR, &[cr]);
    let mut n = 0;
    while w_u8(sreg(), SN_CR) != 0 && n < 10_000 {
        n += 1;
    }
}

/// Sets the chip up for TCP: soft reset (its reset pin is on EN, so a
/// software restart left it as the firmware had it), address, buffers.
fn w5500_init(r: &Record) -> bool {
    w_write(COMMON, 0x00, &[0x80]);
    let mut n = 0;
    while w_u8(COMMON, 0x00) & 0x80 != 0 {
        n += 1;
        if n > 100_000 {
            return false;
        }
    }
    if w_u8(COMMON, 0x39) != 0x04 {
        return false;
    }
    w_write(COMMON, 0x01, &r.gateway);
    w_write(COMMON, 0x05, &inplace::mask(r.prefix));
    w_write(COMMON, 0x09, &r.mac);
    w_write(COMMON, 0x0F, &r.ip);
    // 8 KB receive and 4 KB transmit buffer each for the two sockets.
    for s in 0..8u8 {
        let block = s * 4 + 1;
        let (rx, tx) = if s < SOCKETS { (8, 4) } else { (0, 0) };
        w_write(block, SN_RXBUF, &[rx]);
        w_write(block, SN_TXBUF, &[tx]);
    }
    true
}

fn listen() {
    command(CR_CLOSE);
    w_write(sreg(), SN_MR, &[0x01]); // TCP
    w_set16(sreg(), SN_PORT, 80);
    command(CR_OPEN);
    command(CR_LISTEN);
}

fn status() -> u8 {
    w_u8(sreg(), SN_SR)
}

/// Received bytes waiting in the chip.
fn rx_waiting() -> usize {
    w_u16(sreg(), SN_RX_RSR) as usize
}

/// Takes up to `out.len()` received bytes; 0 when none wait.
fn recv(out: &mut [u8]) -> usize {
    let n = rx_waiting().min(out.len());
    if n == 0 {
        return 0;
    }
    let ptr = w_u16(sreg(), SN_RX_RD);
    w_read(srx(), ptr, &mut out[..n]);
    w_set16(sreg(), SN_RX_RD, ptr.wrapping_add(n as u16));
    command(CR_RECV);
    n
}

/// Sends all of `data`, 1 KB per SEND, each confirmed by SEND_OK: a SEND
/// issued while the previous one is still going out is ignored, which cut
/// replies short on the board.
fn send(data: &[u8]) -> bool {
    for c in data.chunks(1024) {
        let mut waited = 0;
        while (w_u16(sreg(), SN_TX_FSR) as usize) < c.len() {
            if status() != SR_ESTABLISHED || waited > 2_000 {
                return false;
            }
            delay_us(1000);
            waited += 1;
        }
        let ptr = w_u16(sreg(), SN_TX_WR);
        w_write(stx(), ptr, c);
        w_set16(sreg(), SN_TX_WR, ptr.wrapping_add(c.len() as u16));
        command(CR_SEND);
        let mut waited = 0;
        while w_u8(sreg(), SN_IR) & IR_SEND_OK == 0 {
            if status() != SR_ESTABLISHED || waited > 2_000 {
                return false;
            }
            delay_us(1000);
            waited += 1;
        }
        w_write(sreg(), SN_IR, &[IR_SEND_OK]);
    }
    true
}

fn close() {
    command(CR_DISCON);
    for _ in 0..500 {
        if status() == SR_CLOSED {
            break;
        }
        delay_us(1000);
    }
    command(CR_CLOSE);
}

// ── HTTP ────────────────────────────────────────────────────────────────────

struct Request {
    post_update: bool,
    content_length: Option<usize>,
    token_ok: bool,
    expect_continue: bool,
    head_len: usize,
}

fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t' | b'\r'] = s {
        s = rest;
    }
    s
}

fn parse_usize(s: &[u8]) -> Option<usize> {
    if s.is_empty() || s.len() > 9 {
        return None;
    }
    s.iter().try_fold(0usize, |n, &c| c.is_ascii_digit().then(|| n * 10 + usize::from(c - b'0')))
}

/// Hex digit pairs to bytes.
fn token_matches(s: &[u8], token: &[u8; 16]) -> bool {
    let hex = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    if s.len() != 32 {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..16 {
        match (hex(s[2 * i]), hex(s[2 * i + 1])) {
            (Some(h), Some(l)) => diff |= ((h << 4) | l) ^ token[i],
            _ => return false,
        }
    }
    diff == 0
}

fn parse(head: &[u8], token: &[u8; 16]) -> Option<Request> {
    let end = head.windows(4).position(|w| w == b"\r\n\r\n")?;
    let mut lines = head[..end].split(|&c| c == b'\n');
    let first = lines.next()?;
    let mut req = Request {
        post_update: first.starts_with(b"POST /update "),
        content_length: None,
        token_ok: false,
        expect_continue: false,
        head_len: end + 4,
    };
    for line in lines {
        let Some(colon) = line.iter().position(|&c| c == b':') else { continue };
        let (name, value) = (trim(&line[..colon]), trim(&line[colon + 1..]));
        if eq_ignore_case(name, b"content-length") {
            req.content_length = parse_usize(value);
        } else if eq_ignore_case(name, b"x-update-token") {
            req.token_ok = token_matches(value, token);
        } else if eq_ignore_case(name, b"expect") {
            req.expect_continue = eq_ignore_case(value, b"100-continue");
        }
    }
    Some(req)
}

fn respond(status: &[u8], body: &[u8]) {
    let mut len = [0u8; 10];
    let mut n = body.len();
    let mut i = len.len();
    loop {
        i -= 1;
        len[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    // One buffer, one send.
    let mut out = [0u8; 512];
    let mut n = 0;
    for part in [
        &b"HTTP/1.1 "[..],
        status,
        b"\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: ",
        &len[i..],
        b"\r\n\r\n",
        body,
    ] {
        let k = part.len().min(out.len() - n);
        out[n..n + k].copy_from_slice(&part[..k]);
        n += k;
    }
    let _ = send(&out[..n]);
}

// ── Main ────────────────────────────────────────────────────────────────────

const IDLE_LIMIT_MS: u32 = 10 * 60 * 1000;

/// The bootloader calls this with the slot's flash offset.
#[unsafe(no_mangle)]
pub extern "C" fn _start(slot: u32) -> ! {
    unsafe extern "C" {
        static mut _bss_start: u32;
        static mut _bss_end: u32;
    }
    unsafe {
        let mut p = &raw mut _bss_start;
        while p < &raw mut _bss_end {
            write_volatile(p, 0);
            p = p.add(1);
        }
    }
    feed();
    log!("started for the slot at 0x%x", slot);
    // Nothing here runs from flash: keep the cache off, so it can never
    // compete with the flash routines for the bus.
    unsafe {
        Cache_Read_Disable_rom(0);
        Cache_Read_Disable_rom(1);
        esp_rom_spiflash_unlock();
    }
    let at = slot + inplace::RECORD;
    let mut raw = [0u8; inplace::LEN];
    let Some(mut rec) = (if flash_read(at, &mut raw) { Record::decode(&raw) } else { None }) else {
        log!("no record");
        reset()
    };

    spi_init();
    if !w5500_init(&rec) {
        log!("no W5500");
        if rec.state == State::Start {
            give_up(at, b"The updater found no W5500 (Ethernet); nothing was changed.");
        }
        delay_us(5_000_000);
        reset();
    }
    log!("listening on %d.%d.%d.%d:80", rec.ip[0] as u32, rec.ip[1] as u32, rec.ip[2] as u32, rec.ip[3] as u32);
    for s in 0..SOCKETS {
        sel(s);
        listen();
    }

    let mut idle_ms = 0u32;
    loop {
        feed();
        // Only upload attempts count as activity: an open status page polls
        // every 2 s and would otherwise keep the updater up for good.
        let mut uploads = false;
        for s in 0..SOCKETS {
            sel(s);
            match status() {
                SR_ESTABLISHED | SR_CLOSE_WAIT => {
                    uploads |= serve(slot, at, &mut rec);
                    close();
                    listen();
                }
                SR_CLOSED | SR_INIT => listen(),
                SR_LISTEN => {}
                // Passing states: a connection mid-handshake
                // (SYN received; resetting it cut uploads off before their
                // first byte) or one still closing. They move on by
                // themselves.
                _ => {}
            }
        }
        if uploads {
            idle_ms = 0;
        } else {
            delay_us(1000);
            idle_ms += 1;
            if rec.state == State::Start && idle_ms > IDLE_LIMIT_MS {
                give_up(at, b"No upload arrived within 10 minutes; nothing was changed.");
            }
        }
    }
}

/// Serves one connection on the selected socket. True for an upload
/// attempt (`POST /update`). Browsers open spare connections and send
/// nothing on them: dropped after 3 s.
fn serve(slot: u32, at: u32, rec: &mut Record) -> bool {
    let mut head = [0u8; 1024];
    let mut fill = 0usize;
    let mut quiet_ms = 0u32;
    let req = loop {
        feed();
        let n = recv(&mut head[fill..]);
        fill += n;
        if n == 0 {
            if status() != SR_ESTABLISHED || quiet_ms > 3000 {
                return false;
            }
            delay_us(1000);
            quiet_ms += 1;
            continue;
        }
        quiet_ms = 0;
        if let Some(req) = parse(&head[..fill], &rec.token) {
            break req;
        }
        if fill == head.len() {
            respond(b"431 Request Header Fields Too Large", b"Header too large\n");
            return false;
        }
    };
    if !req.post_update {
        // Anything else: say what runs here (the page polls for this).
        let body: &[u8] = match rec.state {
            State::Start => b"{\"updater\":true,\"state\":\"waiting\"}",
            _ => b"{\"updater\":true,\"state\":\"interrupted\"}",
        };
        respond(b"200 OK", body);
        return false;
    } else if !req.token_ok {
        respond(b"403 Forbidden", b"Wrong or missing update token.\n");
    } else {
        match req.content_length {
            Some(len) if (4096..=inplace::IMAGE_MAX as usize).contains(&len) => {
                if req.expect_continue {
                    let _ = send(b"HTTP/1.1 100 Continue\r\n\r\n");
                }
                let early = fill - req.head_len;
                let mut pre = [0u8; 1024];
                pre[..early].copy_from_slice(&head[req.head_len..fill]);
                match receive_image(slot, at, rec, len, &pre[..early]) {
                    Ok(()) => {
                        respond(b"200 OK", b"Written. Restarting into the new firmware...\n");
                        close();
                        log!("written; restarting");
                        reset();
                    }
                    Err(msg) => {
                        log!("upload failed");
                        respond(b"400 Bad Request", msg);
                    }
                }
            }
            _ => respond(b"413 Content Too Large", b"Content-Length missing or the image does not fit.\n"),
        }
    }
    true
}

/// Streams `len` bytes (the first `early` already read) into the slot.
fn receive_image(slot: u32, at: u32, rec: &mut Record, len: usize, early: &[u8]) -> Result<(), &'static [u8]> {
    if !early.is_empty() && early[0] != 0xE9 {
        return Err(b"Not an ESP32 firmware image.\n");
    }
    // Once the first sector is erased the old firmware is gone: say so first.
    if rec.state != State::Writing {
        rec.state = State::Writing;
        if !put_record(at, rec) {
            return Err(b"Writing the record failed.\n");
        }
    }
    let b = buf();
    let mut written = 0usize;
    let mut fill = early.len();
    b.0[..fill].copy_from_slice(early);
    let mut quiet_ms = 0u32;
    while written < len {
        let want = (SECTOR as usize).min(len - written);
        while fill < want {
            feed();
            let n = recv(&mut b.0[fill..want]);
            if n == 0 {
                if status() != SR_ESTABLISHED && rx_waiting() == 0 {
                    return Err(b"The connection closed during the upload.\n");
                }
                delay_us(500);
                quiet_ms += 1;
                if quiet_ms > 30_000 {
                    return Err(b"No data for 15 s.\n");
                }
                continue;
            }
            quiet_ms = 0;
            if written == 0 && fill == 0 && b.0[0] != 0xE9 {
                return Err(b"Not an ESP32 firmware image.\n");
            }
            fill += n;
        }
        b.0[want..].fill(0xFF);
        if !flash_put(slot + written as u32, b, want) {
            return Err(b"Writing flash failed.\n");
        }
        written += want;
        fill = 0;
    }
    rec.state = State::Written;
    if !put_record(at, rec) {
        return Err(b"Writing the record failed.\n");
    }
    Ok(())
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log!("panic");
    reset()
}
