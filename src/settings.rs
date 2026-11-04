//! User settings: the ones in effect, and their storage in the `storage`
//! partition. The settings themselves and their form format are in
//! `pixelcast_core::settings` (host-tested).
//!
//! Two 4 KB sectors are written alternately, each with a sequence number
//! and a CRC. Loading picks the newest valid one, so a power cut while saving
//! leaves the previous settings intact.

use alloc::string::String;
use core::{cell::RefCell, fmt::Write, net::Ipv4Addr};

use embassy_sync::{
    blocking_mutex::{Mutex, raw::CriticalSectionRawMutex},
    signal::Signal,
};

use esp_bootloader_esp_idf::partitions::{self, FlashStorage, PARTITION_TABLE_MAX_LEN, PartitionEntry};
use log::{info, warn};

pub use pixelcast_core::settings::*;

const LABEL: &str = "storage";
const MAGIC: [u8; 4] = *b"ANCF";
const HEADER: usize = 16;
const SECTOR: u32 = 4096;
/// Largest stored form string.
pub const MAX_LEN: usize = SECTOR as usize - HEADER;

// ── Settings in effect ──────────────────────────────────────────────────────
// Changes apply live: the web server stores new settings here and signals
// the parts that use them. Nothing restarts the board.

static CURRENT: Mutex<CriticalSectionRawMutex, RefCell<Option<Settings>>> =
    Mutex::new(RefCell::new(None));

/// The Ethernet address settings changed.
pub static ETH_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Name, password or channel of the setup Wi-Fi changed.
pub static AP_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Any LED setting changed.
pub static LED_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Settings changed from the network (ArtAddress): save them to flash once
/// changes stop coming.
pub static SAVE_SOON: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// A copy of the settings in effect.
pub fn current() -> Settings {
    CURRENT.lock(|c| c.borrow().clone().unwrap_or_default())
}

/// Reads the settings in effect without copying them. `f` runs with
/// interrupts off: keep it short (a field or a small string, nothing like
/// `to_form`).
pub fn with<R>(f: impl FnOnce(&Settings) -> R) -> R {
    CURRENT.lock(|c| match c.borrow().as_ref() {
        Some(s) => f(s),
        None => f(&Settings::default()),
    })
}

/// Puts `new` into effect and notifies whoever is affected.
pub fn set_current(new: Settings) {
    let old = CURRENT.lock(|c| c.borrow_mut().replace(new.clone()));
    let Some(old) = old else {
        return;
    };
    let c = Changes::between(&old, &new);
    if c.eth {
        ETH_CHANGED.signal(());
    }
    if c.fast_flash_on {
        crate::flashinfo::retry();
    }
    if c.led {
        LED_CHANGED.signal(());
    }
    if c.ap {
        AP_CHANGED.signal(());
    }
}

/// The Wi-Fi start delay, without copying all settings (polled often).
pub fn ap_after_s() -> u32 {
    with(|s| s.ap_after_s)
}

pub fn ap_boot_min() -> u16 {
    with(|s| s.ap_boot_min)
}

pub fn ap_idle_min() -> u16 {
    with(|s| s.ap_idle_min)
}


// ── Flash ───────────────────────────────────────────────────────────────────

fn partition(flash: &mut FlashStorage<'_>) -> Option<PartitionEntry> {
    let mut buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut buf).ok()?;
    // Matched by label only: `partition_type()` would panic on a foreign table.
    pt.iter().find(|p| p.label_as_str() == LABEL && p.len() >= 2 * SECTOR)
}

/// Reads the record in sector `n`, returning its sequence number and payload.
fn read_record(flash: &mut FlashStorage<'_>, p: PartitionEntry, n: u32, buf: &mut [u8]) -> Option<(u32, usize)> {
    let mut region = p.as_flash_region(flash);
    let mut head = [0u8; HEADER];
    region.read(n * SECTOR, &mut head).ok()?;
    if head[..4] != MAGIC {
        return None;
    }
    let seq = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    let len = u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as usize;
    let crc = u32::from_le_bytes([head[12], head[13], head[14], head[15]]);
    if len > MAX_LEN || len > buf.len() {
        return None;
    }
    region.read(n * SECTOR + HEADER as u32, &mut buf[..len]).ok()?;
    (crc32(&buf[..len]) == crc).then_some((seq, len))
}

/// The newest valid record: (sector, sequence, payload length).
fn newest(flash: &mut FlashStorage<'_>, p: PartitionEntry, buf: &mut [u8]) -> Option<(u32, u32, usize)> {
    let a = read_record(flash, p, 0, buf).map(|(s, l)| (0, s, l));
    let b = read_record(flash, p, 1, buf).map(|(s, l)| (1, s, l));
    let best = match (a, b) {
        (Some(a), Some(b)) => Some(if b.1 > a.1 { b } else { a }),
        (a, b) => a.or(b),
    }?;
    // `buf` holds whichever was read last; reread the winner.
    read_record(flash, p, best.0, buf)?;
    Some(best)
}

/// Loads the settings, or the defaults if none are stored.
pub fn load(flash: &mut FlashStorage<'_>) -> Settings {
    let mut s = Settings::default();
    let Some(p) = partition(flash) else {
        warn!("settings: no storage partition; using defaults");
        return s;
    };
    let mut buf = alloc::vec![0u8; MAX_LEN];
    match newest(flash, p, &mut buf) {
        Some((_, seq, len)) => match core::str::from_utf8(&buf[..len]) {
            Ok(form) => {
                let _ = s.apply_form(form, false);
                info!("settings: loaded (version {})", seq);
            }
            Err(_) => warn!("settings: stored data is not text; using defaults"),
        },
        None => info!("settings: none stored; using defaults"),
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveError {
    NoPartition,
    TooLarge,
    Flash,
}

/// Stores `s` in the sector not holding the newest record. `buf` is the
/// shared flash buffer (one sector): saving needs no heap beyond the form
/// string, which matters while the setup Wi-Fi has taken most of it.
pub fn save(flash: &mut FlashStorage<'_>, _: &crate::core1::Paused, buf: &mut [u8; SECTOR as usize], s: &Settings) -> Result<(), SaveError> {
    let p = partition(flash).ok_or(SaveError::NoPartition)?;
    let form = s.to_form();
    if form.len() > MAX_LEN {
        return Err(SaveError::TooLarge);
    }
    let (sector, seq) = match newest(flash, p, buf) {
        Some((n, seq, _)) => (1 - n, seq.wrapping_add(1)),
        None => (0, 1),
    };
    let n = record_into(buf, &form, seq);
    // `write` erases the sector first (read-modify-write per sector).
    p.as_flash_region(flash)
        .write(sector * SECTOR, &buf[..n])
        .map_err(|_| SaveError::Flash)?;
    info!("settings: saved (version {})", seq);
    Ok(())
}

/// A settings record (header and `form`) in `buf`; returns its length.
/// `form` must fit (`MAX_LEN`).
pub fn record_into(buf: &mut [u8; SECTOR as usize], form: &str, seq: u32) -> usize {
    let rec = &mut buf[..HEADER + form.len()];
    rec[..4].copy_from_slice(&MAGIC);
    rec[4..8].copy_from_slice(&seq.to_le_bytes());
    rec[8..12].copy_from_slice(&(form.len() as u32).to_le_bytes());
    rec[12..16].copy_from_slice(&crc32(form.as_bytes()).to_le_bytes());
    rec[HEADER..].copy_from_slice(form.as_bytes());
    rec.len()
}

/// Erases both records: the next start uses the defaults.
pub fn reset(flash: &mut FlashStorage<'_>, _: &crate::core1::Paused) -> Result<(), SaveError> {
    let p = partition(flash).ok_or(SaveError::NoPartition)?;
    p.as_flash_region(flash)
        .erase(0, 2 * SECTOR)
        .map_err(|_| SaveError::Flash)?;
    Ok(())
}

/// CRC-32 (IEEE), bitwise: small and fast enough for one sector.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Writes `ip/prefix` for display.
pub fn cidr(ip: Ipv4Addr, prefix: u8) -> String {
    let mut s = String::new();
    let _ = write!(s, "{ip}/{prefix}");
    s
}

// ── Handoff to WLED ─────────────────────────────────────────────────────────
// Sector 2 of `storage` holds this firmware's address (here) and LED setup
// (core/src/wled.rs, at 0x100), written each time it starts WLED. Our
// patched WLED (docs/wled.md) builds its first configuration from them.
// Address record (little endian), 24 bytes:
//   0 magic "PXCW", 4 version 1, 5 mode (0 = DHCP: WLED keeps its own
//   settings; 1 = static), 6 prefix, 7..8 zero, 8 ip, 12 gateway (0.0.0.0:
//   none), 16..20 zero, 20 CRC-32 of bytes 0..20.

const HANDOFF_OFFSET: u32 = 2 * SECTOR;
pub const HANDOFF_LEN: usize = 24;

/// The handoff record for `addr` (the address this firmware has now, if
/// any): static unless it came from DHCP (then WLED's DHCP gets the same
/// lease for the same MAC).
pub fn handoff_record(addr: Option<(Ipv4Addr, u8)>, from_dhcp: bool, gateway: Option<Ipv4Addr>) -> [u8; HANDOFF_LEN] {
    let mut r = [0u8; HANDOFF_LEN];
    r[..4].copy_from_slice(b"PXCW");
    r[4] = 1;
    if let (Some((ip, prefix)), false) = (addr, from_dhcp) {
        r[5] = 1;
        r[6] = prefix;
        r[8..12].copy_from_slice(&ip.octets());
        r[12..16].copy_from_slice(&gateway.map_or([0; 4], |g| g.octets()));
    }
    let crc = crc32(&r[..20]);
    r[20..24].copy_from_slice(&crc.to_le_bytes());
    r
}

/// WLED's diagnostics log (our WLED patch writes it to sector 3 of
/// `storage`: 4-byte tag, u32 length, text), for `GET /api/wled-log`.
pub fn read_wled_log(flash: &mut FlashStorage<'_>, buf: &mut [u8; SECTOR as usize]) -> Option<String> {
    let p = partition(flash)?;
    if p.len() < 4 * SECTOR {
        return None;
    }
    p.as_flash_region(flash).read(3 * SECTOR, &mut buf[..]).ok()?;
    let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if len == 0 || len > buf.len() - 8 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[8..8 + len]).into_owned())
}

/// Writes the handoff record and WLED's LED setup (core/src/wled.rs) to
/// sector 2 of `storage` (erasing it).
pub fn write_handoff(
    flash: &mut FlashStorage<'_>,
    _: &crate::core1::Paused,
    rec: &[u8; HANDOFF_LEN],
    leds: &[u8; pixelcast_core::wled::LEN],
) -> Result<(), SaveError> {
    let p = partition(flash).ok_or(SaveError::NoPartition)?;
    if p.len() < HANDOFF_OFFSET + SECTOR {
        return Err(SaveError::NoPartition);
    }
    const AT: usize = pixelcast_core::wled::OFFSET as usize;
    let mut both = [0xFFu8; AT + pixelcast_core::wled::LEN];
    both[..HANDOFF_LEN].copy_from_slice(rec);
    both[AT..].copy_from_slice(leds);
    p.as_flash_region(flash).write(HANDOFF_OFFSET, &both).map_err(|_| SaveError::Flash)
}

