//! MoonLight's adapter: recognising its image, its LED setup in the handoff
//! (the record WLED takes, `pixelcast_core::dualboot::wled`), its factory
//! reset. The MoonLight side is dualboot/moonlight/patches.

use esp_bootloader_esp_idf::partitions::{FlashStorage, PartitionEntry};
use log::warn;

use super::Adapter;
use crate::ota::text;

pub const ADAPTER: Adapter = Adapter {
    name: "MoonLight",
    version,
    led_setup: Some(super::wled::led_setup),
    reset: Some(reset),
    way_back: "POST /api/pixelcast there, or three BOOT clicks",
};

/// MoonLight's version, if its app descriptor names the project "MoonLight"
/// (before v7: "projectMM").
fn version(_: &mut FlashStorage<'_>, _: PartitionEntry, desc: &[u8]) -> Option<heapless::String<32>> {
    if desc.len() < 80 || !matches!(text(&desc[48..80]), "MoonLight" | "projectMM") {
        return None;
    }
    let mut v = heapless::String::new();
    let _ = v.push_str(text(&desc[16..48]));
    Some(v)
}

/// Erases the start of the filesystem MoonLight keeps its settings in, so it
/// formats it on its next start and takes the handoff as its first
/// configuration again.
fn reset(flash: &mut FlashStorage<'_>, _: &crate::core1::Paused) -> Result<&'static str, &'static str> {
    super::erase_filesystem(
        flash,
        "This layout has no filesystem for MoonLight.\n",
        "Erasing MoonLight's filesystem failed.\n",
    )?;
    warn!("dualboot: MoonLight filesystem erased (factory defaults at its next start)");
    Ok("MoonLight reset to factory defaults: at its next start it takes this firmware's LED setup and address.\n")
}
