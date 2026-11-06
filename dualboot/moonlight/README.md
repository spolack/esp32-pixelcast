# MoonLight

Upstream MoonLight (https://github.com/MoonModules/MoonLight, `main`: 7.0.0-dev
on ESP-IDF v6.1) at the commit `upstream` points to, plus the patch
series in `patches/`:

```
tools/build.sh moonlight          # dist/moonlight.bin
```

By hand: check out that commit, `git am dualboot/moonlight/patches/*.patch`,
then in `esp32/` with ESP-IDF v6.1:
`idf.py -DIDF_TARGET=esp32 -DSDKCONFIG_DEFAULTS="sdkconfig.defaults;sdkconfig.defaults.pixelcast" -DMM_FIRMWARE_NAME=esp32-pixelcast build`
(image: `build/MoonLight.bin`; `build.sh` also sets `PROJECT_VER` to
MoonLight's version and the pinned commit). Without a local ESP-IDF, `build.sh` runs it
in the `espressif/idf:v6.1` image through podman or docker.

Never flash MoonLight over USB with its own tools (web installer, MoonDeck)
on this board: that replaces the bootloader and the partition table with
MoonLight's (a recovery image and one app slot).

## The patches

- **W5500 on the classic ESP32**: a classic ESP32 built without the EMAC
  and with SPI Ethernet gets the W5500 driver, and its RMII pads are no
  longer reported as taken. Its receive task is pinned off the core of the
  LED refill interrupt. It stays at 20 MHz: IDF's driver reads full-duplex,
  which fails at 40 MHz on this board (esp32-pixelcast reaches 80 MHz with
  half-duplex reads).
- **Builds without the certificate bundle**, so HTTPS can be left out.
- **The board's build** (`sdkconfig.defaults.pixelcast`,
  `CONFIG_MM_PIXELCAST`): this repository's partition table, WiFi and
  W5500 Ethernet (SCLK 18, MISO 19, MOSI 23, CS 5, INT 4), GPIO 25 (output
  enable of the 74HCT245) driven low at start. To fit the 1.5 MB slot
  (upstream's classic ESP32 image is 2.1 MB): `-Os`, silent asserts, errors
  only in the serial log, no IPv6, no HTTPS (MoonLight's update from a URL
  works over plain HTTP only; uploading a `.bin` is unchanged), no WiFi
  Enterprise, OWE or SAE on the access point, only the P-256 curve
  (WPA3-Personal's), no mDNS browse or console, the logo at 64 px. WPA2
  and WPA3-Personal stay. About 1.53 MB of 1.57 MB.
  For the network: 1 ms FreeRTOS ticks (upstream's 10 ms quantise frame
  and lock waits and the web server, which the render loop ticks), lwIP on
  core 0 (the LED refill interrupt takes core 1), larger lwIP mailboxes and
  TCP window.

The dual boot (`esp32/main/pixelcast.cpp`; formats: [`../README.md`](../README.md)):

- **Started valid.** This firmware starts MoonLight as "pending verify"
  (the bootloader's rollback), and MoonLight never confirms an image itself:
  without the patch its first restart went back here. It confirms its slot
  once its modules are set up.
- **First configuration from esp32-pixelcast.** On MoonLight's first start
  (no Network or Drivers settings yet, or after "Reset MoonLight") it takes
  the board's Ethernet, esp32-pixelcast's static address (with DHCP there,
  MoonLight stays on DHCP) and its LED outputs (pin, LED type and colour
  order) on one row of lights: one RMT driver when the outputs share a
  fixture and follow each other (it clocks all pins at once), else one per
  output. Direction and dark LEDs at the start are not carried over. And
  **Art-Net in** (MoonLight's NetworkReceive effect, which also takes E1.31
  and DDP) from esp32-pixelcast's first output's universe: RGB, 170 lights
  a universe, all outputs one run. esp32-pixelcast starts each output at
  its own universe; for MoonLight the sender sends one run over all LEDs
  (in LedFx: one Art-Net device with all of them, not one per output). It
  saves that and starts once more on it. From then on MoonLight's settings rule:
  changes here do not reach it until "Reset MoonLight". It reads the same
  LED record as the WLED patches (`firmware/core/src/dualboot/wled.rs`).
- **The OLED** (128x32 on I2C, SDA 21, SCL 22) shows what it shows under
  esp32-pixelcast: the name, the network, Art-Net in and the frame rate.
  The first configuration sets MoonLight's I2C bus to those pins.
- **WiFi** is set up in MoonLight: its access point (`MM-…`) when it has
  no network, or Improv over USB.
- **Way back** without an upload: `POST /api/pixelcast` or three BOOT
  clicks within 2 s. MoonLight's firmware upload accepts esp32-pixelcast's
  image (recognised by its app descriptor's project name).

**Reset MoonLight** (Settings → Maintenance, `POST /dualboot-reset`) erases
the start of the filesystem: MoonLight formats it at its next start and
takes esp32-pixelcast's current setup again. WLED keeps its settings in the
same filesystem, so they go too.

MoonLight writes no diagnostics log for this firmware yet (Settings →
Diagnostics stays empty while MoonLight runs).
