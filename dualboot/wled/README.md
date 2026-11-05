# WLED

Upstream WLED (https://github.com/wled/WLED, `main`: WLED 17-dev on Arduino
core 3 / ESP-IDF 5.5, with the AudioReactive usermod) at the commit
`upstream` points to, plus the patch series in `patches/`:

```
tools/build.sh wled               # dist/wled.bin
```

By hand: check out that commit, `git am dualboot/wled/patches/*.patch`, then
`pio run -e esp32dev_W5500` (needs Node.js for WLED's pages; image:
`.pio/build/esp32dev_W5500/firmware.bin`).

Never flash WLED over USB with PlatformIO on this board: that replaces the
bootloader and the partition table with WLED's.

## The patches

- **W5500 Ethernet** (SPI: SCLK 18, MISO 19, MOSI 23, CS 5, INT 4; the output
  enable of the board's 74HCT245 on GPIO 25 is driven low): Ethernet type
  "W5500 (SPI)" in WLED's settings, using core 3's W5500 driver.
- **The board's build** `esp32dev_W5500`: W5500 by default, no button, relay
  or IR pins, I2C on 21/22 and the four-line display usermod.

The dual boot (wled00/pixelcast.cpp; formats: [`../README.md`](../README.md)):

- **First configuration from esp32-pixelcast.** When WLED starts without a
  `/cfg.json` (its first start, or after "Reset WLED"), it writes one from the
  handoff: esp32-pixelcast's static or Art-Net fallback address as WLED's own
  static address (with DHCP there, WLED stays on DHCP), the LED outputs (pins,
  lengths, LED types, colour orders, direction, dark LEDs at the start, one
  after the other in WLED's LED sequence), the power limit in mA at the strip
  voltage, and the four-line display (128x32 OLED, I2C 21/22) on. Live input
  (Art-Net, sACN) stays at WLED's defaults: turn it on in WLED if wanted.
  From then on WLED's own configuration rules: changes in esp32-pixelcast do
  not reach it until "Reset WLED". The LED record (magic `PXCL`, WLED's LED
  type and colour order codes): `firmware/core/src/dualboot/wled.rs`.
- **Way back** without an upload: `POST /pixelcast` (WLED's settings PIN
  applies) or three BOOT clicks within 2 s. WLED's update page accepts
  esp32-pixelcast's image (recognised by its app descriptor's project name).
  Stock WLED refuses it (no WLED metadata) unless "Ignore firmware
  validation" is ticked.
- **Diagnostics** for when WLED is not reachable and its serial port cannot be
  watched: the ESP-IDF log (Ethernet driver), network events and status
  snapshots go to the log sector at 15, 45, 120 and 280 s and before
  switching back (tag `PXCD`). esp32-pixelcast shows it (Settings →
  Diagnostics, `GET /api/dualboot-log`); over USB: `espflash read-flash
  0x3FF000 0x1000 diag.bin`. Each WLED start overwrites it.
- **Static address applied again.** WLED applies a static Ethernet address
  once, when the link comes up; after a power-on that sometimes did not take
  (Ethernet at 0.0.0.0, the address on the Wi-Fi side). While the link is up
  and Ethernet has no address, it is applied again every 2 s.

**Reset WLED** (Settings → Maintenance, `POST /dualboot-reset`) erases the
start of WLED's filesystem: its settings and presets are gone, and its next
start takes esp32-pixelcast's current setup again.

Without the patches WLED works too, but uses its own defaults (DHCP:
unreachable on a network without a DHCP server) and only the upload leads
back.
