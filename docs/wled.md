# Dual boot with WLED

This firmware and WLED share one flash layout (`partitions.csv`, WLED's standard
4 MB table with two 1.5 MB app slots; after WLED's filesystem come this
firmware's update record and settings). Each firmware installs into the slot it
is not running from, so:

- **WLED from here:** upload WLED's `.bin` on the settings page (Firmware
  update). It is written to the other slot, checked and started. While it sits
  there, the settings page's Start button or three BOOT clicks within 2 s
  start it again (or BOOT pressed within 2 s of a power-on). A normal update of
  this firmware overwrites it; "Install into this slot" keeps it (see
  `updater/README.md`).
- **Back from WLED:** upload this firmware's `.bin` in WLED's update page
  (Config → Security & Updates → Manual OTA update), or, with the patched WLED
  below, `POST /pixelcast` or three BOOT clicks within 2 s. Stock WLED refuses
  the upload (no WLED metadata) unless "Ignore firmware validation" is ticked;
  the patched one recognises this firmware.

## The patched WLED

Built from upstream WLED (https://github.com/wled/WLED, `main`: WLED 17-dev on
Arduino core 3 / ESP-IDF 5.5, with the AudioReactive usermod) at the commit
pinned in `tools/build.sh`, plus the patch series in `patches/wled/`:

```
tools/build.sh wled               # dist/wled.bin
```

By hand: check out that commit, `git am patches/wled/*.patch`, then
`pio run -e esp32dev_W5500` (needs Node.js for WLED's pages; image:
`.pio/build/esp32dev_W5500/firmware.bin`).

What the patches add:

- **W5500 Ethernet** (SPI: SCLK 18, MISO 19, MOSI 23, CS 5, INT 4; the output
  enable of the board's 74HC245 on GPIO 25 is driven low): Ethernet type "W5500 (SPI)"
  in WLED's settings, using core 3's W5500 driver.
- **The board's build** `esp32dev_W5500`: W5500 by default, no button, relay
  or IR pins, I2C on 21/22 and the four-line display usermod.

The dual boot (wled00/pixelcast.cpp):

- **First configuration from this firmware.** Each time this firmware starts
  WLED it leaves its setup in sector 2 of its `storage` partition: its
  address and its LED setup (core/src/wled.rs). When WLED starts without a
  `/cfg.json` (its first start, or after "Reset WLED"), it writes one from
  that: this firmware's static or Art-Net fallback address as WLED's own
  static address (with DHCP here, WLED stays on DHCP), the LED outputs (pins,
  lengths, LED types, colour orders, direction, dark LEDs at the start, one
  after the other in WLED's LED sequence), the power limit in mA at the strip
  voltage, and the four-line display (128x32 OLED, I2C 21/22) on. Live input
  (Art-Net, sACN) stays at WLED's defaults: turn it on in WLED if wanted.
  From then on WLED's own configuration rules: changes here do not reach it
  until "Reset WLED".
- **Way back** without an upload: `POST /pixelcast` (WLED's settings PIN
  applies) or three BOOT clicks within 2 s. WLED's update page accepts this
  firmware's image (recognised by its app descriptor's project name).
- **Diagnostics** for when WLED is not reachable and its serial port cannot be
  watched: the ESP-IDF log (Ethernet driver), network events and status
  snapshots go to sector 3 of `storage` at 15, 45, 120 and 280 s and before
  switching back. This firmware shows it (Settings → Diagnostics → WLED log,
  or `GET /api/wled-log`); over USB: `espflash read-flash 0x3FF000 0x1000
  diag.bin` (magic `PXCD`, u32 length, text). Each WLED start overwrites it.
- **Static address applied again.** WLED applies a static Ethernet address
  once, when the link comes up; after a power-on that sometimes did not take
  (Ethernet at 0.0.0.0, the address on the Wi-Fi side). While the link is up
  and Ethernet has no address, it is applied again every 2 s.

**Reset WLED** (Settings → Maintenance, `POST /wled-reset`) erases the start
of WLED's filesystem: its settings and presets are gone, and its next start
takes this firmware's current setup again.

Without the patches WLED works too, but uses its own defaults (DHCP:
unreachable on a network without a DHCP server) and only the upload leads
back.

## Moving a board to this layout (over the network)

Boards flashed with the old table (two 1.9 MB slots) move without USB:

1. Download a settings backup (Settings → Backup), just in case.
2. Make sure this firmware runs confirmed from `ota_0` (install it once more
   and confirm if the status page shows `ota_1`).
3. `POST /migrate-layout` (login, `X-Requested-With`). It refuses unless the
   table is exactly the old one, rewrites the 4 KB table sector (settings are
   copied to their new place first) and restarts. `/api/config` then shows
   `"layout": "wled"`. Do not cut the power during that request.
4. Upload WLED's `firmware.bin` on the settings page; check WLED answers at the
   same address, then come back.

The migration also erases the start of WLED's filesystem: a WLED filesystem
from an earlier stock WLED (0xF0000 bytes at the same place) made WLED assert
at start (`lfs_fs_grow_ … block_count >= lfs->block_count`) and restart in a
loop. A board migrated before this fix
that shows it on its serial port: `espflash erase-region 0x310000 0xEB000`.

With USB: `espflash erase-flash`, then `cargo run --release`, then restore
the backup. Never flash WLED itself over USB with PlatformIO afterwards: that
replaces the bootloader and the partition table with WLED's.
