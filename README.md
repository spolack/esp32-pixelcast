# esp32-pixelcast

Firmware for an ESP32 with a W5500 Ethernet chip that turns Art-Net and sACN
into LED data: four outputs for WS281x/SK6812-style strips (RGB or RGBW),
configured and updated from a small built-in web interface. Written in Rust
(esp-hal, embassy), no ESP-IDF at runtime.

It runs on the ArtnetNode-ESP32 carrier board (ESP32-DevKitC-32D, W5500,
74HCT245 level shifter, optional 128x32 OLED). Pins are fixed in
`src/board.rs`; tested on a V2.2 board.

## The idea

A small LED node should do two jobs well. At a show it is a plain Art-Net
or sACN receiver: a lighting desk, Resolume or QLC+ sends pixels over
Ethernet, and every frame arrives on time. The rest of the week the same
bars light a room, play effects or react to music, without a computer
sending data. Those are two different programs, so this board carries both:

- **esp32-pixelcast** for pixel data over the network: a wired-only
  receiver built for steady frame rates, written from scratch in Rust on
  bare metal.
- **[WLED](https://github.com/wled/WLED)** for everything standalone
  (effects, presets, AudioReactive, the WLED apps), upstream WLED with a
  short patch series for the W5500 and this board.

Switching between them takes a button press or one click, and WLED starts
with the LED setup of esp32-pixelcast (pins, lengths, LED types, power
limit), so the strips work in both right away.

The other goal: once flashed over USB, a board never needs USB again.
Updates arrive over the network, start on trial and roll back if they do
not come up; even the firmware that is running can be replaced while WLED
keeps its slot, and the bootloader itself is updated over the network.

Nothing upstream is forked. `tools/build.sh` fetches pinned versions of
WLED, ESP-IDF's bootloader and embassy-net-wiznet and applies the
`git am` patch series in `patches/`, so the changes stay small, readable
and easy to rebase.

## How it was made

Purely vibecoded :) Every line here (firmware, bootloader hook, updater,
WLED patches, build scripts and docs) came out of a conversation with an AI
coding assistant, tested on a real board along the way: Art-Net streams,
update cycles, power cycles, a scope on the status LED pin. It works on the
author's boards; read the code and test with your own setup before you
trust it with a show.

## Credits

The hardware and the starting point come from **Matthijs Dethmers
([mdethmers](https://github.com/mdethmers))**:

- the [ArtnetNode-ESP32](https://oshwlab.com/matthijsdethmers/artnetnode-esp32)
  carrier board, open hardware with schematic, PCB and BOM (also sold
  assembled by [LichtBit](https://lichtbit.etsy.com));
- [ESP32-Artnet-Node-receiver](https://github.com/mdethmers/ESP32-Artnet-Node-receiver),
  the original Arduino firmware for it, the reference for the pin map, the
  OLED and the status LED;
- [W5500_WLED](https://github.com/mdethmers/W5500_WLED), WLED-MM running on
  this board with its W5500, which showed that WLED and the W5500 fit
  together here; the W5500 patch for upstream WLED follows that work.

Thanks also to the [WLED](https://github.com/wled/WLED) and
[MoonModules](https://github.com/MoonModules/WLED-MM) teams, and to the
[esp-rs](https://github.com/esp-rs) and [embassy](https://embassy.dev)
projects this firmware is built on.

## Features

- **Art-Net 4 and sACN (E1.31)** on Ethernet, unicast and broadcast/multicast;
  ArtPoll discovery, ArtSync, merging of two sources (HTP or LTP).
- **Four LED outputs**, driven by I2S in parallel from the second core; per
  output LED count and start universe, colour order, brightness, dithering,
  interpolation, power limit per output and in total, loss behaviour.
- **Web interface** (status, live Art-Net view, settings) with login; JSON API
  documented in `web/API.md`. Settings backup and restore.
- **Network:** DHCP with the Art-Net 2.x.y.z fallback, or static; mDNS name;
  a setup Wi-Fi access point with a captive portal when Ethernet is missing,
  on request, or for a set time after each start.
- **Updates over the network** with a trial boot and rollback, explicit
  confirmation, and a crash log.
- **Two firmware slots:** run this firmware next to another one, for example
  [WLED](docs/wled.md) or an older version, and switch between them from the
  web interface or with the BOOT button. An in-place update replaces the
  running firmware without touching the other slot (`updater/README.md`).
- **Own bootloader** (`tools/bootloader`): app rollback, BOOT within 2 s of a
  power-on starts the other slot, runs the in-place updater. The firmware
  installs it over the network.
- Status LED, OLED status display, test patterns, BOOT button actions.

## Repository layout

| Path | What |
|---|---|
| `src/`, `core/` | the firmware; `core/` holds the hardware-free parts, tested on the host |
| `web/` | the web pages (built into the firmware) and `API.md` |
| `tools/bootloader/` | ESP-IDF v6.1 second-stage bootloader with our hook (C) |
| `updater/` | the RAM-only in-place updater (Rust, no esp-hal) |
| `patches/embassy-net-wiznet/` | `git am` series on embassy-net-wiznet 0.3.0 (W5500 receive path) |
| `patches/wled/` | `git am` series on upstream WLED: W5500, board build, dual boot |
| `tools/build.sh` | builds everything from pinned upstream sources and these patches |
| `docs/` | WLED dual boot |

Upstream code is not copied into this repository: `tools/build.sh` fetches
the pinned versions and applies the patch series.

## Building

Prerequisites:

- Rust with the esp toolchain: `cargo install espup && espup install`
  (writes `~/export-esp.sh`), and `cargo install espflash`
- `esptool` (`pip install esptool`) for the updater image
- ESP-IDF v6.1 for the bootloader (`idf.py` on the PATH or `IDF_PATH` set)
- PlatformIO (`pip install platformio`) and Node.js 20 for WLED
- git, python3, curl

```
tools/build.sh            # everything: deps bootloader updater firmware wled image
tools/build.sh deps firmware   # just the firmware (dist/esp32-pixelcast.bin)
tools/test.sh             # host tests
```

Results land in `dist/`. `tools/build.sh deps` must run once before a plain
`cargo build` works, since Cargo takes the patched embassy-net-wiznet from
`build/deps/`.

## Flashing

First time, over USB (erases the settings):

```
espflash write-bin 0x0 dist/full-4MB.bin
```

The full image holds the bootloader, the partition table, this firmware in
`ota_0` and WLED in `ota_1`. Afterwards everything goes over the network:
open `http://<board address>/config`, user `admin`, password `artnet-setup`
until you set one. A board without a DHCP server takes the Art-Net address
2.x.y.z/8 from its MAC; without Ethernet it opens the setup Wi-Fi
(`ArtnetNode-XXXX`, web interface at 192.168.4.1).

For development, `cargo run --release` flashes the firmware and the partition
table over USB and opens the serial monitor.

## Releases

GitHub Actions builds every push. A tag `v*` publishes the images as a
release; `main` updates the `nightly` pre-release.

## License

GPL-3.0-or-later, see `LICENSE`. The bootloader sources derived from
ESP-IDF's examples keep their Apache-2.0 notice; the patch series carry the
licenses of the projects they patch (embassy-net-wiznet: MIT OR Apache-2.0,
WLED: EUPL-1.2).
