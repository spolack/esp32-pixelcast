# esp32-pixelcast

Art-Net 4 / sACN (E1.31) to LED firmware for the ESP32 + W5500, written in
Rust (esp-hal, embassy, bare metal). Four outputs for WS281x/SK6812 (RGB,
RGBW). Dual boot with another firmware, e.g. upstream
[WLED](dualboot/wled/README.md).

Images: [Releases](https://github.com/spolack/esp32-pixelcast/releases)
(`nightly` follows `main`).

## Performance

Measured on the board, unicast, 12 s per run, W5500 SPI at 80 MHz, fast
flash on (settings page; off by default). Tool: [`tools/artnet_stress.py`](tools/artnet_stress.py),
details in [`docs/api.md`](docs/api.md).

| Input | Rate | Loss |
|---|---|---|
| Art-Net, 16 universes | 2700 packets/s (170 fps) | 0 % |
| Art-Net, 4 universes | 3000 packets/s | 0 % |
| sACN, 16 universes | 2080 packets/s (130 fps) | 0 % |
| sACN multicast | 2400 packets/s | 0 % |
| Burst, all universes back to back at wire speed | 36 universes | 0 % |
| | 48 universes | 1.2 % |

Without fast flash: about 1500 (Art-Net) and 1150 (sACN) packets/s.

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
- LED timing presets. Per output: LED count, start universe, colour order,
  brightness, dithering, interpolation, power limit (per output and total,
  in watts), behaviour on signal loss.
- Web interface with login, live view, settings backup; JSON API
  ([`docs/api.md`](docs/api.md)).
- DHCP with Art-Net 2.x.y.z fallback, or static; mDNS; setup Wi-Fi access
  point with captive portal.
- Updates over Ethernet (1 MB in 15–30 s) with trial boot, rollback,
  confirmation and crash log; in-place updates keep the other slot
  ([`updater/README.md`](updater/README.md)).
- Own ESP-IDF bootloader ([`bootloader/`](bootloader/)): rollback, BOOT within 2 s of
  power-on starts the other slot, runs the in-place updater; installable
  over the network.

## Dual boot

The second app slot holds another firmware, started from the settings page
or with three BOOT clicks. Supported: upstream WLED with a short patch series
(W5500, this board, the way back); it starts with this firmware's LED setup
and static address. More firmwares plug in under `dualboot/<name>/`. Details:
[`dualboot/README.md`](dualboot/README.md).

## Building

Needs: Rust with espup (`espup install`), espflash, esptool; ESP-IDF v6.1 or
podman/docker (bootloader); PlatformIO and Node.js 20 (WLED); git, python3,
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
(embassy-net-wiznet: MIT OR Apache-2.0, WLED: EUPL-1.2).
