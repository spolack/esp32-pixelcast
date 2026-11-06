# Dual boot

The flash has two 1.5 MB app slots (`partitions.csv`, WLED's standard 4 MB
table; after WLED's filesystem come this firmware's update record and
settings). esp32-pixelcast runs in one, another ESP32 firmware can live in
the other. Each firmware installs into the slot it is not running from.

Supported: [WLED](wled/README.md), [MoonLight](moonlight/README.md).

## Using it

- **Install the other firmware:** upload its `.bin` on the settings page
  (Firmware update). It is written to the other slot, checked and started.
- **Start it again:** the settings page's Start button, three BOOT clicks
  within 2 s, or BOOT within 2 s of a power-on.
- **Back here:** upload esp32-pixelcast's `.bin` in the other firmware's
  update page. A firmware with our patches also has a shorter way (WLED:
  `POST /pixelcast`, MoonLight: `POST /api/pixelcast`; both: three BOOT
  clicks).
- A normal update of esp32-pixelcast overwrites the other slot; "Install
  into this slot" keeps it ([`updater/README.md`](../updater/README.md)).

Settings → Maintenance → "Reset <firmware>" (`POST /dualboot-reset`) resets
the other firmware to its factory defaults, if its adapter knows how. The
other firmware's log: Settings → Diagnostics (`GET /api/dualboot-log`).

## What esp32-pixelcast hands over

Each time it starts the other firmware it writes sector 2 of its `storage`
partition (0x3FE000):

| Offset | What |
|---|---|
| 0 | address record, 24 bytes: magic `PXCW`, version 1, mode (0 DHCP, 1 static), prefix, IP, gateway, CRC-32 |
| 0x100 | LED setup in the other firmware's own format (from its adapter), up to 256 bytes |

Sector 3 (0x3FF000) is free for the other firmware's log: 4-byte tag, u32
length, text. Byte layouts: `firmware/core/src/dualboot.rs`.

## Adding a firmware

A firmware for the other slot is a directory `dualboot/<name>/`:

| Path | What |
|---|---|
| `upstream` | submodule pointer (entry `dualboot/<name>` in `.gitmodules` with `url`, `branch`, `update = none`): the pinned commit, never checked out |
| `patches/` | `git am` series on that commit (may be empty or missing: stock upstream) |
| `build.sh` | executable, `build.sh <source> <cache> <image>`: builds the patched source into `<image>` |
| `README.md` | what the patches do |

`tools/build.sh <name>` fetches the pinned commit into
`build/dualboot/<name>/src`, applies the series and calls `build.sh` with
`build/dualboot/<name>/cache` (kept between builds) and `dist/<name>.bin`.
`tools/refresh-patches.sh <name>` moves the series to a new upstream commit,
and Dependabot proposes new commits; `tools/build.sh check` checks the entry.

What else a new firmware touches:

- `.github/workflows/build.yml`: its name in the `dualboot` job's matrix,
  plus any setup its `build.sh` needs (as the `if: matrix.name == 'wled'`
  steps).
- Firmware, optional: an adapter, `firmware/src/dualboot/<name>.rs` with a
  `pub const ADAPTER` (name, version check, LED setup, reset, way back),
  listed in `ADAPTERS` in `firmware/src/dualboot.rs`. Its LED setup format,
  if any, goes in `firmware/core/src/dualboot/<name>.rs` (host-tested).
  Without an adapter the firmware still works: it is started and gets the
  address record; the slot list shows its project name.
- The "Supported" line above and in `README.md`.

The other firmware needs:

- the partition table above (it must not replace it: never flash it over USB
  with its own tools afterwards);
- an update page that accepts esp32-pixelcast's image (an ESP-IDF app image,
  project name `esp32-pixelcast` in its app descriptor), so there is a way
  back.

## Moving a board to this layout (over the network)

Boards flashed with the old table (two 1.9 MB slots) move without USB:

1. Download a settings backup (Settings → Backup), just in case.
2. Make sure this firmware runs confirmed from `ota_0` (install it once more
   and confirm if the status page shows `ota_1`).
3. `POST /migrate-layout` (login, `X-Requested-With`). It refuses unless the
   table is exactly the old one, rewrites the 4 KB table sector (settings are
   copied to their new place first) and restarts. `/api/config` then shows
   `"layout": "dualboot"`. Do not cut the power during that request.
4. Upload the other firmware on the settings page; check it answers at the
   same address, then come back.

The migration also erases the start of WLED's filesystem: a WLED filesystem
from an earlier stock WLED (0xF0000 bytes at the same place) made WLED assert
at start (`lfs_fs_grow_ … block_count >= lfs->block_count`) and restart in a
loop. A board migrated before this fix that shows it on its serial port:
`espflash erase-region 0x310000 0xEB000`.

With USB: `espflash erase-flash`, then `tools/cargo.sh run --release`, then
restore the backup.
