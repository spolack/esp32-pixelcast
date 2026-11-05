//! WLED's adapter: recognising its image, its LED setup in the handoff
//! (`pixelcast_core::dualboot::wled`), its factory reset. The WLED side is
//! dualboot/wled/patches.

use esp_bootloader_esp_idf::partitions::{self, FlashStorage, PARTITION_TABLE_MAX_LEN, PartitionEntry};
use log::warn;
use pixelcast_core::dualboot::wled as led;

use super::Adapter;
use crate::ota::{find_after, text};
use crate::settings::SECTOR;

pub const ADAPTER: Adapter = Adapter {
    name: "WLED",
    version,
    led_setup: Some(led_setup),
    reset: Some(reset),
    way_back: "POST /pixelcast there, or three BOOT clicks",
};

/// WLED's version, if `p` holds WLED. Its app descriptor only names its SDK
/// (arduino-lib-builder): WLED 16+ carries a metadata block (magic "WSTS",
/// u32 version, 48-byte version, 48-byte release name); older ones only a
/// string ("... WLED v14.7.2-mdev").
fn version(flash: &mut FlashStorage<'_>, p: PartitionEntry) -> Option<heapless::String<32>> {
    let mut v = heapless::String::new();
    let mut m = [0u8; 4 + 48 + 48];
    let mut old = [0u8; 32];
    if find_after(flash, p, &0x5753_5453u32.to_le_bytes(), &mut m).is_some() {
        let _ = v.push_str(text(&m[4..52]));
        // Its build ("ESP32_W5500"), when it fits.
        let rel = text(&m[52..100]);
        if !rel.is_empty() && v.len() + 1 + rel.len() <= v.capacity() {
            let _ = v.push_str(" ");
            let _ = v.push_str(rel);
        }
        Some(v)
    } else if let Some(n) = find_after(flash, p, b"WLED v", &mut old) {
        let _ = v.push_str(text(&old[..n]));
        Some(v)
    } else {
        None
    }
}

/// This firmware's LED setup in WLED's terms, into the handoff.
fn led_setup(out: &mut [u8]) {
    let leds = crate::settings::with(|s| led::encode(&led::from_settings(s, crate::board::LED_PINS)));
    out[..led::LEN].copy_from_slice(&leds);
}

/// Erases the start of WLED's filesystem (the `spiffs` partition; LittleFS
/// keeps its superblocks in the first two blocks), so WLED formats it on its
/// next start. Its settings and presets are gone.
fn reset(flash: &mut FlashStorage<'_>, _: &crate::core1::Paused) -> Result<&'static str, &'static str> {
    let mut buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut buf).map_err(|_| "Cannot read the partition table.\n")?;
    let fs = pt
        .iter()
        .find(|p| p.label_as_str() == "spiffs" && p.raw_type() == 1) // data (partition_type() panics on unknown subtypes)
        .ok_or("This layout has no WLED filesystem.\n")?;
    if fs.len() < 2 * SECTOR {
        return Err("This layout has no WLED filesystem.\n");
    }
    flash
        .erase(fs.offset(), fs.offset() + 2 * SECTOR)
        .map_err(|_| "Erasing WLED's filesystem failed.\n")?;
    warn!("dualboot: WLED filesystem erased (factory defaults at its next start)");
    Ok("WLED reset to factory defaults: at its next start it takes this firmware's LED setup and address.\n")
}
