# Development

## Layout

| Path | What |
|---|---|
| `firmware/` | esp32-pixelcast (Rust, esp-hal, embassy) |
| `firmware/core/` | its hardware-free part: own crate, so it runs as host tests |
| `firmware/web/` | the web pages, compressed into the firmware by `build.rs` |
| `firmware/patches/` | `git am` series on embassy-net-wiznet (W5500 receive path) |
| `bootloader/` | second-stage bootloader (ESP-IDF v6.1, C): rollback, BOOT button, starts the updater |
| `updater/` | in-place updater: runs from RAM and rewrites the running slot (Rust) |
| `dualboot/` | firmwares for the other slot, one directory each ([`dualboot/README.md`](../dualboot/README.md)) |
| `dualboot/wled/` | WLED: `upstream` (pinned commit, submodule pointer), `patches/`, `build.sh` |
| `partitions.csv` | the flash layout shared by all of the above |
| `tools/` | `build.sh` (all images), `cargo.sh` (cargo for the firmware), `test.sh` (host tests), `refresh-patches.sh`, `artnet_*.py` (test senders) |
| `docs/` | `api.md` (web pages and HTTP API), this file |

`build/` holds fetched sources and intermediate builds, `dist/` the images.
Neither is committed, and neither are any images: everything is built from
source.

## Building

`tools/build.sh` with no arguments builds every image into `dist/`; the steps
are listed at its top. `tools/cargo.sh` runs cargo in `firmware/` and first
builds what the firmware embeds:

- **updater:** `updater/` with the same toolchain, then esptool (about a
  second when nothing changed).
- **bootloader:** only when `bootloader/`, `partitions.csv` or the ESP-IDF
  version changed. With a local ESP-IDF v6.1 (`idf.py` on `PATH`, or
  `IDF_PATH`), else in the `espressif/idf:v6.1` image through podman or
  docker, the one CI uses (`IDF_CONTAINER=1` prefers it). The build is
  reproducible: local and CI bytes match.

`tools/build.sh check` runs the consistency checks CI runs (pins, versions
shared by firmware and updater, no committed images).

## Pins

- **embassy-net-wiznet:** the release in `firmware/Cargo.lock`.
  `firmware/Cargo.toml` names the plain crates.io release; `tools/build.sh`
  and `tools/cargo.sh` inject the patched copy from `build/deps/` and put the
  committed lock back. A plain `cargo build` stops with a note; plain
  `cargo update` is fine.
- **dualboot/\<name\>:** the commit `dualboot/<name>/upstream` points to
  (WLED: upstream `main`). Only a pointer (`update = none`: `git clone
  --recursive` skips it); the build fetches that commit into
  `build/dualboot/<name>/src`. The pins live in the git index, so building
  needs a git clone, not a source archive.
- **ESP-IDF:** `IDF_VERSION` in `tools/build.sh` (also the container tag).
- **esp-rom-sys:** the updater pins (`=`) the version the firmware gets
  through esp-hal; `tools/build.sh check` fails when they differ.

The scripts select the `esp` toolchain (`RUSTUP_TOOLCHAIN`). There is no
`rust-toolchain.toml`, so Dependabot can resolve the crates with stock Rust;
for an editor run `rustup override set esp` in `firmware/` and `updater/`.

## Updating upstream

Dependabot proposes crate, GitHub Actions and dual-boot firmware (WLED)
updates weekly; CI builds each proposal with the patches applied. A red
`firmware` or `wled` job on such a pull request usually means a patch no
longer applies. Then, on the pull request branch:

```
tools/refresh-patches.sh wled                  # or: embassy-net-wiznet
# on a conflict: fix the files in build/dualboot/wled/src (or
# build/deps/embassy-net-wiznet), git add them, git am --continue, then
tools/refresh-patches.sh wled --export
tools/build.sh wled                            # or: deps firmware
git add dualboot/wled/patches && git commit    # or: firmware/patches
```

Moving a pin by hand: `tools/refresh-patches.sh wled latest` (or a
commit), `tools/refresh-patches.sh embassy-net-wiznet 0.4.0`.

## CI and releases

`.github/workflows/build.yml` builds pushes to `main`, pull requests, tags
and a daily schedule. A tag `v*` publishes a release; `main` updates the
`nightly` pre-release in place.
