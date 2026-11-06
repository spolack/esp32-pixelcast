# esp32-pixelcast

Art-Net 4 / sACN (E1.31) to LED firmware for the ESP32 + W5500, written in
Rust (esp-hal, embassy, bare metal). Four outputs for WS281x/SK6812 (RGB,
RGBW). Dual boot with another firmware: upstream
[WLED](dualboot/wled/README.md) or [MoonLight](dualboot/moonlight/README.md).

Images: [Releases](https://github.com/spolack/esp32-pixelcast/releases)
(`nightly` follows `main`).

## Performance

Measured on the board, unicast, 12 s per run, W5500 SPI at 80 MHz, fast
flash on (settings page; off by default). Tool: [`tools/artnet_stress.py`](tools/artnet_stress.py),
details in [`docs/api.md`](docs/api.md).

| Input | Rate | Loss |
|---|---|---|
| Art-Net, 16 universes | 2720 packets/s (170 fps) | 0 % |
| Art-Net, 4 universes | 3000 packets/s | 0 % |
| sACN, 16 universes | 2080 packets/s (130 fps) | 0 % |
| sACN multicast | 2400 packets/s | 0 % |
| Burst, all universes of a frame back to back at wire speed, 40 fps | 36 universes | 0 % |
| | 48 universes | 1.2–1.9 % |

Without fast flash: about 1500 (Art-Net) and 1150 (sACN) packets/s. The
board turns fast flash off after an unexpected restart until the next
power-on (`/api/status`: `flash.fast` reads `after_crash`); check it reads
`qio80` before measuring.

With the microphone on (audio universe sent 44 times a second; measured
2026-11-05, alternating runs, 4 × 512 RGBW):

| Input | Audio off | Audio on |
|---|---|---|
| Art-Net, 16 universes at 170 fps (2720 packets/s) | 0 % | 0 % |
| Burst, 36 universes at 40 fps | 0 % | 0.0–0.1 % |
| Burst, 48 universes at 40 fps | 1.9 % | 4.1–4.2 % |

Up to 36 universes the microphone costs nothing measurable; at 48-universe
bursts it adds about 2 percentage points of loss (handling a packet takes
16.0 instead of 15.5 µs, from `/api/status` `spi.proc_us` / `proc_n`).

| Output | |
|---|---|
| 4 × 512 RGBW (16 universes), 844 ns bits | 72 fps, no underruns |
| 500 ns bits | 120 fps |
| Interpolation, dithering and power limit on | same rates, input limit unchanged (2720 packets/s) |

For comparison, the original Arduino firmware for this board states
"over 40 FPS" and 16 universes.

## Hardware

[ArtnetNode-ESP32](https://oshwlab.com/matthijsdethmers/artnetnode-esp32)
carrier: ESP32-DevKitC-32D, W5500, 74HCT245, 128x32 OLED. Tested on V2.2.

| Signal | GPIO |
|---|---|
| LED outputs 1–4 | 12, 14, 27, 26 |
| 74HCT245 output enable (driven low) | 25 |
| Status LED (WS2812) | 16 |
| Microphone (default SPH0645, SEL to GND) DOUT / LRCL / BCLK | 32 / 15 / 33 (configurable) |
| W5500 SCLK / MISO / MOSI / CS / INT | 18 / 19 / 23 / 5 / 4 |
| OLED SDA / SCL | 21 / 22 |
| BOOT button | 0 |

## Getting started

Flash the full image once over USB (erases settings):

```
espflash write-bin 0x0 full-4MB.bin
```

It holds the bootloader, the partition table, this firmware in `ota_0` and
WLED in `ota_1`. The board takes an address by DHCP; without a DHCP server
2.x.y.z/8 from its MAC; without Ethernet it opens the Wi-Fi
`ArtnetNode-XXXX` (password `artnet-setup`, 192.168.4.1).

Settings: `http://<address>/config`, user `admin`, password
`artnet-setup`. Change it. The web interface is plain HTTP: keep the board
on a trusted network.

## Features

- Art-Net 4 and sACN: ArtPoll, ArtSync, HTP/LTP merge of two sources.
- LED timing presets. Per output: LED count, start universe, LED type,
  colour order, reverse, grouping, offset, start channel, power limit in
  watts. For all outputs: brightness, dithering, interpolation, total power
  limit, behaviour on signal loss.
- Web interface with login, live view, settings backup; JSON API
  ([`docs/api.md`](docs/api.md)).
- DHCP with Art-Net 2.x.y.z fallback, or static; mDNS; setup Wi-Fi access
  point with captive portal.
- Microphone as a DMX source: level, beat, beat phase and three bands as
  one Art-Net or sACN universe for a console; I2S, I2S with MCLK (line-in)
  and PDM mics on configurable pins ([`docs/api.md`](docs/api.md)).
- Updates over Ethernet (1 MB in 15–30 s) with trial boot, rollback,
  confirmation and crash log; in-place updates keep the other slot
  ([`updater/README.md`](updater/README.md)).
- Own ESP-IDF bootloader ([`bootloader/`](bootloader/)): rollback, BOOT within 2 s of
  power-on starts the other slot, runs the in-place updater; installable
  over the network.

## Dual boot

The second app slot holds another firmware, started from the settings page
or with three BOOT clicks. Supported: upstream WLED and MoonLight, each with a
short patch series (W5500, this board, the way back); each starts with this
firmware's LED setup and static address. More firmwares plug in under `dualboot/<name>/`. Details:
[`dualboot/README.md`](dualboot/README.md).

## Building

Needs: Rust with espup (`espup install`), espflash, esptool; ESP-IDF v6.1 or
podman/docker (bootloader, MoonLight); PlatformIO and Node.js 20 (WLED); git, python3,
curl.

```
tools/build.sh                 # all images -> dist/
tools/build.sh firmware        # firmware with its bootloader and updater
tools/cargo.sh run --release   # the same, flashed over USB, serial monitor
tools/test.sh                  # host tests
```

Everything is built from source; upstream code is not forked: the build
fetches pinned versions and applies our patch series
(`firmware/patches/`, `dualboot/<name>/patches/`). Layout, pins and
refreshing patches: [`docs/development.md`](docs/development.md).

## Credits

Board, original firmware and the first WLED port with W5500 by Matthijs
Dethmers ([mdethmers](https://github.com/mdethmers)):
[ArtnetNode-ESP32](https://oshwlab.com/matthijsdethmers/artnetnode-esp32)
(open hardware, sold assembled by [LichtBit](https://lichtbit.etsy.com)),
[ESP32-Artnet-Node-receiver](https://github.com/mdethmers/ESP32-Artnet-Node-receiver),
[W5500_WLED](https://github.com/mdethmers/W5500_WLED). Built on
[WLED](https://github.com/wled/WLED), [esp-rs](https://github.com/esp-rs)
and [embassy](https://embassy.dev).

Vibecoded: written with an AI coding assistant, tested on hardware.

## License

GPL-3.0-or-later ([`LICENSE`](LICENSE)). Bootloader parts from ESP-IDF examples:
Apache-2.0. Patch series: the license of the patched project
(embassy-net-wiznet: MIT OR Apache-2.0, WLED: EUPL-1.2, MoonLight: GPL-3.0).
