//! Dual boot: the firmware in the other app slot.
//!
//! Any ESP32 firmware for this layout can live there; this firmware starts
//! it, hands over its address (and, through the firmware's adapter, its LED
//! setup) and shows its log (formats: `pixelcast_core::dualboot`). What is
//! specific to one firmware is its [`Adapter`]: a module next to [`wled`],
//! listed in [`ADAPTERS`]. The repository side is `dualboot/<name>/` (see
//! dualboot/README.md).

use alloc::string::String;
use core::sync::atomic::{AtomicU8, Ordering};

use esp_bootloader_esp_idf::partitions::{self, FlashStorage, PARTITION_TABLE_MAX_LEN, PartitionEntry};
use log::warn;
use pixelcast_core::dualboot as hand;

use crate::core1::Paused;
use crate::settings::{self, EthMode};

mod moonlight;
mod wled;

/// An adapter's version check and reset (see [`Adapter`]). The version
/// check gets the image's app descriptor too (80 bytes, already read).
pub type VersionFn = fn(&mut FlashStorage<'_>, PartitionEntry, &[u8]) -> Option<heapless::String<32>>;
pub type ResetFn = fn(&mut FlashStorage<'_>, &Paused) -> Result<&'static str, &'static str>;

/// What this firmware knows about one other firmware.
pub struct Adapter {
    /// Its name on the update page ("WLED").
    pub name: &'static str,
    /// Its version, if the image in `p` is this firmware.
    pub version: VersionFn,
    /// Writes its LED setup into the handoff (at `hand::SETUP_OFFSET`, up to
    /// `hand::SETUP_MAX` bytes), if it takes one.
    pub led_setup: Option<fn(&mut [u8])>,
    /// Resets it to factory defaults, if this firmware knows how; returns the
    /// message for the settings page.
    pub reset: Option<ResetFn>,
    /// How to come back from it besides uploading this firmware in its update
    /// page ("" if there is no other way).
    pub way_back: &'static str,
}

/// The firmwares with an adapter. A new one: a module like [`wled`] and an
/// entry here.
/// MoonLight first: its check is exact (its app descriptor), WLED's searches
/// the image for text.
pub const ADAPTERS: &[&Adapter] = &[&moonlight::ADAPTER, &wled::ADAPTER];

/// The firmware in the other slot.
#[derive(Clone, Copy)]
pub enum Firmware {
    Known(&'static Adapter),
    /// No adapter: started and handed the address record only.
    Unknown,
}

impl Firmware {
    pub fn adapter(self) -> Option<&'static Adapter> {
        match self {
            Firmware::Known(a) => Some(a),
            Firmware::Unknown => None,
        }
    }
}

/// Name and version of another firmware's image, for the update page.
pub struct Identity {
    pub firmware: Firmware,
    pub name: heapless::String<32>,
    pub version: heapless::String<32>,
}

/// What [`detect`] found at start-up: 0 none, 1 no adapter, 2 + i
/// `ADAPTERS[i]`. Cleared while an upload rewrites the slot.
static PRESENT: AtomicU8 = AtomicU8::new(0);

/// Identifies the image in `p`; `desc` is its app descriptor.
pub fn identify(flash: &mut FlashStorage<'_>, p: PartitionEntry, desc: &[u8]) -> Identity {
    for a in ADAPTERS {
        if let Some(version) = (a.version)(flash, p, desc) {
            let mut name = heapless::String::new();
            let _ = name.push_str(a.name);
            return Identity { firmware: Firmware::Known(a), name, version };
        }
    }
    let mut id = Identity { firmware: Firmware::Unknown, name: heapless::String::new(), version: heapless::String::new() };
    // esp_app_desc_t: version at 16, project name at 48 (32 bytes each).
    let _ = id.name.push_str(crate::ota::text(&desc[48..80]));
    let _ = id.version.push_str(crate::ota::text(&desc[16..48]));
    id
}

/// Looks at the other slot (cheap: header and segments, plus the adapters'
/// checks) and remembers what it holds.
pub fn detect(flash: &mut FlashStorage<'_>) -> Option<Firmware> {
    let found = crate::ota::other_firmware(flash).map(|(p, desc)| identify(flash, p, &desc).firmware);
    let code = match found {
        None => 0,
        Some(Firmware::Unknown) => 1,
        Some(Firmware::Known(a)) => 2 + ADAPTERS.iter().position(|b| core::ptr::eq(*b, a)).unwrap_or(0) as u8,
    };
    PRESENT.store(code, Ordering::Relaxed);
    found
}

/// The firmware [`detect`] found in the other slot, if any.
pub fn present() -> Option<Firmware> {
    match PRESENT.load(Ordering::Relaxed) {
        0 => None,
        1 => Some(Firmware::Unknown),
        n => ADAPTERS.get(usize::from(n - 2)).map(|a| Firmware::Known(a)),
    }
}

/// The other slot is being rewritten: nothing to start there for now.
pub fn forget() {
    PRESENT.store(0, Ordering::Relaxed);
}

/// Writes the handoff for `firmware` (sector 2 of `storage`, erasing it):
/// this firmware's address now, and its LED setup in that firmware's format.
/// A failure only loses the handoff.
pub fn write_handoff(flash: &mut FlashStorage<'_>, _: &Paused, firmware: Firmware) {
    let (addr, source) = crate::status::with(|st| (st.eth_addr, st.eth_source));
    let gateway = settings::with(|s| if s.eth_mode == EthMode::Static { s.gateway } else { None });
    let mut sector = [0xFFu8; hand::SETUP_OFFSET as usize + hand::SETUP_MAX];
    sector[..hand::ADDRESS_LEN].copy_from_slice(&hand::address_record(addr, source == "DHCP", gateway));
    if let Some(setup) = firmware.adapter().and_then(|a| a.led_setup) {
        setup(&mut sector[hand::SETUP_OFFSET as usize..]);
    }
    let written = settings::storage(flash).ok_or("no storage partition").and_then(|p| {
        p.as_flash_region(flash)
            .write(hand::HANDOFF_SECTOR * settings::SECTOR, &sector)
            .map_err(|_| "flash write failed")
    });
    if let Err(e) = written {
        warn!("dualboot: handoff not written: {e}");
    }
    crate::crashlog::leaving();
}

/// The other firmware's log (sector 3 of `storage`), for
/// `GET /api/dualboot-log`.
pub fn read_log(flash: &mut FlashStorage<'_>, buf: &mut [u8; settings::SECTOR as usize]) -> Option<String> {
    let p = settings::storage(flash)?;
    if p.len() < (hand::LOG_SECTOR + 1) * settings::SECTOR {
        return None;
    }
    p.as_flash_region(flash).read(hand::LOG_SECTOR * settings::SECTOR, &mut buf[..]).ok()?;
    hand::log_text(&buf[..]).map(|t| String::from_utf8_lossy(t).into_owned())
}

/// Resets the other firmware to its factory defaults through its adapter;
/// at its next start it takes this firmware's handoff as its first
/// configuration again. Returns the message for the settings page.
pub fn reset(flash: &mut FlashStorage<'_>, paused: &Paused) -> Result<&'static str, &'static str> {
    match present() {
        None => Err("No other firmware in the other slot.\n"),
        Some(f) => match f.adapter().and_then(|a| a.reset) {
            Some(reset) => reset(flash, paused),
            None => Err("No reset known for the firmware in the other slot.\n"),
        },
    }
}

/// Erases the start of the shared filesystem (the `spiffs` partition;
/// LittleFS keeps its superblocks in the first two blocks), so the firmware
/// in the other slot formats it on its next start. WLED and MoonLight both
/// keep their settings there: whatever either stored is gone.
fn erase_filesystem(flash: &mut FlashStorage<'_>, missing: &'static str, failed: &'static str) -> Result<(), &'static str> {
    let mut buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut buf).map_err(|_| "Cannot read the partition table.\n")?;
    let fs = pt
        .iter()
        .find(|p| p.label_as_str() == "spiffs" && p.raw_type() == 1) // data (partition_type() panics on unknown subtypes)
        .ok_or(missing)?;
    if fs.len() < 2 * settings::SECTOR {
        return Err(missing);
    }
    flash.erase(fs.offset(), fs.offset() + 2 * settings::SECTOR).map_err(|_| failed)
}
