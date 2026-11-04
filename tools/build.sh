#!/usr/bin/env bash
# Builds everything this repository ships, from pinned upstream sources plus
# the patch series in patches/:
#
#   deps        embassy-net-wiznet 0.3.0 from crates.io + patches/embassy-net-wiznet
#               (build/deps/embassy-net-wiznet, which Cargo.toml points to)
#   bootloader  tools/bootloader with ESP-IDF v6.1   -> dist/bootloader.bin
#   updater     updater/ (in-place updater)          -> dist/updater.bin
#   firmware    esp32-pixelcast, embedding the two above -> dist/esp32-pixelcast.bin
#   wled        WLED (wled/WLED main) at a pinned commit + patches/wled -> dist/wled.bin
#   image       dist/full-4MB.bin: bootloader, partition table, firmware in ota_0,
#               WLED in ota_1 (if built); flash once over USB at 0x0
#   all         everything above, in that order
#
# Usage: tools/build.sh [step...]   (default: all)
#
# Needs: the Rust esp toolchain (espup) and espflash for deps/updater/firmware,
# ESP-IDF v6.1 (idf.py, or IDF_PATH) for bootloader, PlatformIO (pio) and Node.js for
# wled, git, python3 and curl or a populated Cargo cache.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BUILD="$ROOT/build"
DIST="$ROOT/dist"

# ── Pinned upstream sources ──────────────────────────────────────────────────
WIZNET_VERSION=0.3.0
WIZNET_SHA256=336e290ec0eedc19e9d0a948d2f715ea187de79da517a0a24e55553c1a185ab8
WLED_REPO=https://github.com/wled/WLED.git
WLED_COMMIT=e1450a72f62d32e7dc4bc4ed33b8efc4ae6bbc17
WLED_ENV=esp32dev_W5500
IDF_VERSION=v6.1

# Flash layout (partitions.csv).
BOOTLOADER_AT=0x1000
TABLE_AT=0x8000
OTA0_AT=0x10000
OTA1_AT=0x190000
FLASH_SIZE=0x400000

GIT_ID=(-c user.name=build -c user.email=build@localhost)

log() { printf '\n== %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "$1 not found: $2"; }

rust_env() {
    export PATH="$HOME/.cargo/bin:$PATH"
    # espup writes the toolchain environment here.
    [ -f "$HOME/export-esp.sh" ] && . "$HOME/export-esp.sh" >/dev/null 2>&1 || true
    need cargo "install Rust and the esp toolchain (espup install)"
    need espflash "cargo install espflash"
}

# A fresh git checkout of a source tree with the patch series applied on top.
apply_series() { # <dir> <patch dir>
    local dir="$1" series="$2"
    git -C "$dir" "${GIT_ID[@]}" am --quiet --keep-cr "$series"/*.patch
}

step_deps() {
    log "embassy-net-wiznet $WIZNET_VERSION + patches/embassy-net-wiznet"
    local dst="$BUILD/deps/embassy-net-wiznet"
    local stamp="$BUILD/deps/embassy-net-wiznet.stamp"
    local want
    want="$WIZNET_VERSION $(cat "$ROOT"/patches/embassy-net-wiznet/*.patch | sha256sum | cut -c1-16)"
    if [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$want" ] && [ -d "$dst" ]; then
        echo "up to date"
        return
    fi
    mkdir -p "$BUILD/deps"
    local crate="$BUILD/deps/embassy-net-wiznet-$WIZNET_VERSION.crate"
    if [ ! -f "$crate" ]; then
        local cached
        cached=$(ls "${CARGO_HOME:-$HOME/.cargo}"/registry/cache/*/embassy-net-wiznet-"$WIZNET_VERSION".crate 2>/dev/null | head -1 || true)
        if [ -n "$cached" ]; then
            cp "$cached" "$crate"
        else
            need curl "or have the crate in the Cargo cache"
            curl -fsSL -o "$crate" "https://static.crates.io/crates/embassy-net-wiznet/embassy-net-wiznet-$WIZNET_VERSION.crate"
        fi
    fi
    echo "$WIZNET_SHA256  $crate" | sha256sum -c --quiet - || die "checksum mismatch: $crate"
    rm -rf "$dst" "$BUILD/deps/embassy-net-wiznet-$WIZNET_VERSION"
    tar -xzf "$crate" -C "$BUILD/deps"
    mv "$BUILD/deps/embassy-net-wiznet-$WIZNET_VERSION" "$dst"
    git -C "$dst" init --quiet
    git -C "$dst" add -A
    git -C "$dst" "${GIT_ID[@]}" commit --quiet -m "embassy-net-wiznet $WIZNET_VERSION (crates.io)"
    apply_series "$dst" "$ROOT/patches/embassy-net-wiznet"
    echo "$want" > "$stamp"
    git -C "$dst" log --oneline
}

step_bootloader() {
    log "bootloader (ESP-IDF $IDF_VERSION)"
    if ! command -v idf.py >/dev/null 2>&1; then
        [ -n "${IDF_PATH:-}" ] || die "idf.py not found: install ESP-IDF $IDF_VERSION and source its export.sh (or set IDF_PATH)"
        # shellcheck disable=SC1091
        . "$IDF_PATH/export.sh" >/dev/null
    fi
    idf.py --version | grep -q "$IDF_VERSION" || echo "warning: ESP-IDF is not $IDF_VERSION ($(idf.py --version))"
    idf.py -C "$ROOT/tools/bootloader" -B "$BUILD/bootloader" \
        -D SDKCONFIG="$BUILD/bootloader/sdkconfig" bootloader
    mkdir -p "$DIST"
    cp "$BUILD/bootloader/bootloader/bootloader.bin" "$DIST/bootloader.bin"
    # The BOOT slot switch must be built in (a Kconfig name mismatch once
    # compiled it out without a warning).
    grep -aq "BOOT within" "$DIST/bootloader.bin" ||
        die "bootloader.bin lacks the BOOT slot switch (CONFIG_PXC_BOOT_BUTTON)"
}

step_updater() {
    log "in-place updater"
    rust_env
    "$ROOT/updater/build.sh" "$DIST/updater.bin"
}

step_firmware() {
    log "esp32-pixelcast firmware"
    rust_env
    [ -d "$BUILD/deps/embassy-net-wiznet" ] || step_deps
    # The bootloader and updater it carries: freshly built ones if present,
    # else the prebuilt copies in the repository.
    [ -f "$DIST/bootloader.bin" ] && export PXC_BOOTLOADER_BIN="$DIST/bootloader.bin"
    [ -f "$DIST/updater.bin" ] && export PXC_UPDATER_BIN="$DIST/updater.bin"
    (cd "$ROOT" && cargo build --release)
    mkdir -p "$DIST"
    espflash save-image --chip esp32 \
        "$ROOT/target/xtensa-esp32-none-elf/release/esp32-pixelcast" "$DIST/esp32-pixelcast.bin"
    espflash partition-table "$ROOT/partitions.csv" --to-binary -o "$DIST/partitions.bin"
    cmp -s "$DIST/partitions.bin" "$ROOT/src/partitions-wled.bin" ||
        die "partitions.csv and src/partitions-wled.bin differ (the firmware migrates to the latter)"
}

step_wled() {
    log "WLED ($WLED_REPO @ ${WLED_COMMIT:0:7} + patches/wled)"
    need git ""
    need pio "install PlatformIO (pip install platformio)"
    need npm "install Node.js 20 (WLED bundles its web pages with it)"
    local src="$BUILD/WLED"
    if [ ! -d "$src/.git" ]; then
        git clone --quiet "$WLED_REPO" "$src"
    fi
    if ! git -C "$src" cat-file -e "$WLED_COMMIT^{commit}" 2>/dev/null; then
        git -C "$src" fetch --quiet origin
    fi
    git -C "$src" am --abort >/dev/null 2>&1 || true
    git -C "$src" checkout --quiet --detach "$WLED_COMMIT"
    git -C "$src" reset --quiet --hard
    git -C "$src" clean --quiet -fdx -e .pio
    apply_series "$src" "$ROOT/patches/wled"
    (cd "$src" && pio run -e "$WLED_ENV")
    mkdir -p "$DIST"
    cp "$src/.pio/build/$WLED_ENV/firmware.bin" "$DIST/wled.bin"
}

step_image() {
    log "full flash image"
    for f in bootloader.bin partitions.bin esp32-pixelcast.bin; do
        [ -f "$DIST/$f" ] || die "dist/$f missing: build it first"
    done
    python3 - "$DIST" "$FLASH_SIZE" "$BOOTLOADER_AT" "$TABLE_AT" "$OTA0_AT" "$OTA1_AT" <<'EOF'
import os, sys
dist, size, bl, table, ota0, ota1 = sys.argv[1], *(int(a, 16) for a in sys.argv[2:])
img = bytearray(b"\xff" * size)  # erased flash: otadata empty, so ota_0 boots
def put(at, name, limit):
    data = open(os.path.join(dist, name), "rb").read()
    if len(data) > limit:
        sys.exit(f"{name} is {len(data)} bytes, more than the {limit} it has")
    img[at:at + len(data)] = data
    print(f"  0x{at:06x}  {name} ({len(data)} bytes)")
put(bl, "bootloader.bin", table - bl)
put(table, "partitions.bin", 0x1000)
put(ota0, "esp32-pixelcast.bin", ota1 - ota0)
if os.path.exists(os.path.join(dist, "wled.bin")):
    put(ota1, "wled.bin", ota1 - ota0)
else:
    print("  (no dist/wled.bin: ota_1 stays empty)")
open(os.path.join(dist, "full-4MB.bin"), "wb").write(img)
EOF
    echo "dist/full-4MB.bin: flash once over USB with"
    echo "  espflash write-bin 0x0 dist/full-4MB.bin"
    echo "(this erases the settings; later updates go over the network)"
}

steps=("$@")
[ ${#steps[@]} -eq 0 ] && steps=(all)
for s in "${steps[@]}"; do
    case "$s" in
        all) step_deps; step_bootloader; step_updater; step_firmware; step_wled; step_image ;;
        deps | bootloader | updater | firmware | wled | image) "step_$s" ;;
        *) die "unknown step: $s (deps bootloader updater firmware wled image all)" ;;
    esac
done
