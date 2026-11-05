#!/usr/bin/env bash
# Builds everything this repository ships, from pinned upstream sources plus
# our patch series:
#
#   deps        embassy-net-wiznet (the version in firmware/Cargo.lock) from
#               crates.io + firmware/patches/embassy-net-wiznet
#               -> build/deps/embassy-net-wiznet
#   check       consistency checks (pins, versions shared by firmware and updater)
#   bootloader  bootloader/ with ESP-IDF v6.1        -> dist/bootloader.bin
#   updater     updater/ (in-place updater)          -> dist/updater.bin
#   firmware    firmware/, embedding the two above   -> dist/esp32-pixelcast.bin
#               (builds them first if needed)
#   <name>      a firmware for the other slot, dualboot/<name>/ (e.g. wled):
#               its upstream at the pinned commit + its patches -> dist/<name>.bin
#   dualboot    all of dualboot/
#   image       dist/full-4MB.bin: bootloader, partition table, firmware in ota_0,
#               dist/$DUALBOOT.bin in ota_1 (default wled, if built); flash
#               once over USB at 0x0
#   all         everything above, in that order
#
# Usage: tools/build.sh [step...]   (default: all)
#
# The firmware is built with the patched embassy-net-wiznet injected on the
# command line (cargo_patched), so its Cargo.toml and Cargo.lock name the
# plain crates.io release and resolve without build/ (Dependabot). For a
# manual build or flash use tools/cargo.sh, e.g. tools/cargo.sh run --release.
#
# Needs: the Rust esp toolchain (espup), espflash and esptool for
# deps/updater/firmware; for the bootloader ESP-IDF v6.1 (idf.py, or IDF_PATH)
# or else podman or docker, which run it in the espressif/idf:v6.1 image (as
# CI does; IDF_CONTAINER=1 prefers that); git, python3 and curl or a
# populated Cargo cache; for dualboot/<name> what its build.sh names
# (wled: PlatformIO and Node.js).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FW="$ROOT/firmware"
BUILD="$ROOT/build"
DIST="$ROOT/dist"

log() { printf '\n== %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "$1 not found: $2"; }

# Field of a package in a Cargo.lock: lock_field <lock> <package> <field>.
lock_field() {
    awk -v n="$2" -v f="$3" '
        $0 == "name = \"" n "\"" { p = 1; next }
        p && /^name = / { p = 0 }
        p && $1 == f { gsub(/"/, "", $3); print $3; exit }' "$1"
}

# A build interrupted inside cargo_patched leaves the committed lock here.
if [ -f "$BUILD/Cargo.lock.saved" ]; then
    mv "$BUILD/Cargo.lock.saved" "$FW/Cargo.lock"
fi

# ── Pinned upstream sources ──────────────────────────────────────────────────
# embassy-net-wiznet: the release in firmware/Cargo.lock (Dependabot bumps it).
WIZNET_VERSION=$(lock_field "$FW/Cargo.lock" embassy-net-wiznet version)
WIZNET_SHA256=$(lock_field "$FW/Cargo.lock" embassy-net-wiznet checksum)
# Firmwares for the other slot: see dualboot_pin.
IDF_VERSION=v6.1
# Which of them full-4MB.bin carries in ota_1.
DUALBOOT=${DUALBOOT:-wled}

# Flash layout (partitions.csv).
BOOTLOADER_AT=0x1000
TABLE_AT=0x8000
OTA0_AT=0x10000
OTA1_AT=0x190000
FLASH_SIZE=0x400000

GIT_ID=(-c user.name=build -c user.email=build@localhost)

rust_env() {
    export PATH="$HOME/.cargo/bin:$PATH"
    # The Xtensa toolchain espup installs. Not in a rust-toolchain.toml: that
    # would make every cargo run here (Dependabot's too) demand it.
    export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-esp}"
    # espup writes the toolchain environment here.
    [ -f "$HOME/export-esp.sh" ] && . "$HOME/export-esp.sh" >/dev/null 2>&1 || true
    need cargo "install Rust and the esp toolchain (espup install)"
    need espflash "cargo install espflash"
}

# The patches of a series in order (none: an empty or missing directory).
series_patches() { # <patch dir>
    compgen -G "$1/*.patch" | sort || true
}

# A fresh git checkout of a source tree with the patch series applied on top.
apply_series() { # <dir> <patch dir>
    local dir="$1" series="$2" patches
    mapfile -t patches < <(series_patches "$series")
    [ ${#patches[@]} -gt 0 ] || { echo "no patches in ${series#"$ROOT"/}"; return 0; }
    if ! git -C "$dir" "${GIT_ID[@]}" am --quiet --keep-cr -3 "${patches[@]}"; then
        git -C "$dir" am --abort >/dev/null 2>&1 || true
        die "${series#"$ROOT"/} does not apply to this base; see tools/refresh-patches.sh"
    fi
}

# cargo in firmware/ with the patched embassy-net-wiznet and the bootloader
# and updater the firmware embeds. Cargo rewrites the lock entry of a patched
# crate; the committed one is put back. (Cargo also warns that the patch "was
# not used": that is the separate resolve for build-std, which has no
# embassy-net-wiznet.)
cargo_patched() {
    # Always: after a lock bump the prepared copy is another version, which
    # Cargo would skip in favour of the unpatched release (up to date: no-op).
    step_deps >/dev/null
    embedded_images
    local rc=0
    mkdir -p "$BUILD"
    cp "$FW/Cargo.lock" "$BUILD/Cargo.lock.saved"
    # Ctrl-C included: put the lock back on the way out.
    trap restore_lock EXIT
    trap 'exit 130' INT TERM
    # --config after the subcommand: external ones (clippy) only see it there.
    local sub="${1:?cargo subcommand}"
    shift
    (cd "$FW" && PXC_WIZNET_PATCHED=1 cargo "$sub" \
        --config "patch.crates-io.embassy-net-wiznet.path=\"$BUILD/deps/embassy-net-wiznet\"" \
        "$@") || rc=$?
    restore_lock
    trap - EXIT INT TERM
    return $rc
}

restore_lock() {
    if [ -f "$BUILD/Cargo.lock.saved" ]; then
        mv "$BUILD/Cargo.lock.saved" "$FW/Cargo.lock"
    fi
}

# Copies a build result to dist/ only when it changed, so the firmware build
# (which embeds it) is not redone for an identical file.
install_if_changed() { # <file> <dist name>
    mkdir -p "$DIST"
    cmp -s "$1" "$DIST/$2" || cp "$1" "$DIST/$2"
}

# The bootloader and updater for the firmware build: PXC_BOOTLOADER_BIN /
# PXC_UPDATER_BIN when set (CI passes the bootloader its own job built),
# else built here into dist/ (the bootloader only when its sources changed).
embedded_images() {
    if [ -n "${PXC_BOOTLOADER_BIN:-}" ]; then
        [ -f "$PXC_BOOTLOADER_BIN" ] || die "PXC_BOOTLOADER_BIN: $PXC_BOOTLOADER_BIN not found"
        PXC_BOOTLOADER_BIN=$(realpath "$PXC_BOOTLOADER_BIN")
    else
        [ -f "$DIST/bootloader.bin" ] && [ "$(cat "$BUILD/bootloader.stamp" 2>/dev/null)" = "$(bootloader_stamp)" ] ||
            step_bootloader
        PXC_BOOTLOADER_BIN="$DIST/bootloader.bin"
    fi
    if [ -n "${PXC_UPDATER_BIN:-}" ]; then
        [ -f "$PXC_UPDATER_BIN" ] || die "PXC_UPDATER_BIN: $PXC_UPDATER_BIN not found"
        PXC_UPDATER_BIN=$(realpath "$PXC_UPDATER_BIN")
    else
        [ -n "${UPDATER_BUILT:-}" ] || step_updater >&2
        PXC_UPDATER_BIN="$DIST/updater.bin"
    fi
    export PXC_BOOTLOADER_BIN PXC_UPDATER_BIN
}

step_deps() {
    log "embassy-net-wiznet $WIZNET_VERSION + firmware/patches/embassy-net-wiznet"
    local dst="$BUILD/deps/embassy-net-wiznet"
    local stamp="$BUILD/deps/embassy-net-wiznet.stamp"
    [ -n "$WIZNET_VERSION" ] && [ -n "$WIZNET_SHA256" ] ||
        die "embassy-net-wiznet missing from firmware/Cargo.lock"
    local want
    want="$WIZNET_VERSION $(series_patches "$FW/patches/embassy-net-wiznet" | xargs -r cat | sha256sum | cut -c1-16)"
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
    git -C "$dst" tag pxc-base
    # tools/refresh-patches.sh only wants the pristine release.
    [ -z "${PXC_BASE_ONLY:-}" ] || return 0
    apply_series "$dst" "$FW/patches/embassy-net-wiznet"
    echo "$want" > "$stamp"
    git -C "$dst" log --oneline
}

# What the bootloader is built from: its sources, the partition table and
# the ESP-IDF version.
bootloader_stamp() {
    (cd "$ROOT" && git ls-files -co --exclude-standard -- bootloader partitions.csv |
        sort | xargs cat | sha256sum | cut -c1-16)
    echo "$IDF_VERSION"
}

step_bootloader() {
    log "bootloader (ESP-IDF $IDF_VERSION)"
    local out
    if [ -z "${IDF_CONTAINER:-}" ] && { command -v idf.py >/dev/null 2>&1 || [ -n "${IDF_PATH:-}" ]; }; then
        if ! command -v idf.py >/dev/null 2>&1; then
            # shellcheck disable=SC1091
            . "$IDF_PATH/export.sh" >/dev/null
        fi
        if ! idf.py --version | grep -q "$IDF_VERSION"; then
            # In CI a mismatch would ship another bootloader than the pin says.
            [ -z "${CI:-}" ] || die "ESP-IDF is not $IDF_VERSION ($(idf.py --version)): update the container in .github/workflows/build.yml"
            echo "warning: ESP-IDF is not $IDF_VERSION ($(idf.py --version))"
        fi
        idf.py -C "$ROOT/bootloader" -B "$BUILD/bootloader" \
            -D SDKCONFIG="$BUILD/bootloader/sdkconfig" bootloader
        out="$BUILD/bootloader/bootloader/bootloader.bin"
    else
        local rt user=()
        rt=$(command -v podman || command -v docker) ||
            die "no ESP-IDF $IDF_VERSION for the bootloader: install it and source its export.sh (or set IDF_PATH), or install podman or docker"
        # Rootless podman maps the container's root to this user; docker needs it.
        [ "${rt##*/}" = docker ] && user=(--user "$(id -u):$(id -g)" -e HOME=/tmp)
        echo "in espressif/idf:$IDF_VERSION ($rt)"
        mkdir -p "$BUILD/bootloader-container"
        "$rt" run --rm "${user[@]}" \
            -v "$ROOT/bootloader:/src/bootloader:ro,z" \
            -v "$ROOT/partitions.csv:/src/partitions.csv:ro,z" \
            -v "$BUILD/bootloader-container:/build:z" \
            "docker.io/espressif/idf:$IDF_VERSION" \
            idf.py -C /src/bootloader -B /build -D SDKCONFIG=/build/sdkconfig bootloader
        out="$BUILD/bootloader-container/bootloader/bootloader.bin"
    fi
    # The BOOT slot switch must be built in (a Kconfig name mismatch once
    # compiled it out without a warning).
    grep -aq "BOOT within" "$out" ||
        die "bootloader.bin lacks the BOOT slot switch (CONFIG_PXC_BOOT_BUTTON)"
    install_if_changed "$out" bootloader.bin
    bootloader_stamp > "$BUILD/bootloader.stamp"
    echo "dist/bootloader.bin: $(wc -c < "$DIST/bootloader.bin") bytes"
}

step_updater() {
    log "in-place updater"
    rust_env
    check_shared_versions
    mkdir -p "$BUILD"
    "$ROOT/updater/build.sh" "$BUILD/updater.bin"
    install_if_changed "$BUILD/updater.bin" updater.bin
    UPDATER_BUILT=1
}

step_firmware() {
    log "esp32-pixelcast firmware"
    rust_env
    check_shared_versions
    cargo_patched build --release
    mkdir -p "$DIST"
    espflash save-image --chip esp32 \
        "$FW/target/xtensa-esp32-none-elf/release/esp32-pixelcast" "$DIST/esp32-pixelcast.bin"
    espflash partition-table "$ROOT/partitions.csv" --to-binary -o "$DIST/partitions.bin"
    cmp -s "$DIST/partitions.bin" "$FW/src/partitions-dualboot.bin" ||
        die "partitions.csv and firmware/src/partitions-dualboot.bin differ (the firmware migrates to the latter)"
}

# ── Firmwares for the other slot (dualboot/<name>/, see dualboot/README.md) ──
# Each has: upstream (a submodule pointer, never checked out: the commit),
# patches/ (git am series on it), build.sh <source> <cache> <image>.

dualboot_names() {
    local d
    for d in "$ROOT"/dualboot/*/; do
        [ -f "$d/build.sh" ] && basename "$d"
    done
    return 0
}

is_dualboot() { [[ -n "$1" && "$1" != */* && -f "$ROOT/dualboot/$1/build.sh" ]]; }

# The commit dualboot/<name>/upstream points to, and its repository.
dualboot_pin() { (git -C "$ROOT" ls-files -s "dualboot/$1/upstream" 2>/dev/null || true) | awk '{print $2}'; }
dualboot_url() { git -C "$ROOT" config -f .gitmodules "submodule.dualboot/$1.url"; }

# build/dualboot/<name>/src at <commit> (default: the pin), clean, tagged
# pxc-base. Fetches only when the commit is not there yet.
dualboot_checkout() { # <name> [<commit or ref>]
    local name="$1" want="${2:-}" src="$BUILD/dualboot/$1/src"
    need git ""
    if [ ! -d "$src/.git" ]; then
        mkdir -p "$src"
        git -C "$src" init --quiet
        git -C "$src" remote add origin "$(dualboot_url "$name")"
    fi
    [ ! -d "$src/.git/rebase-apply" ] ||
        die "build/dualboot/$name/src has a patch series half applied (tools/refresh-patches.sh): finish it with git am --continue and --export, or git -C build/dualboot/$name/src am --abort"
    [ -n "$want" ] || want=$(dualboot_pin "$name")
    [ -n "$want" ] || die "dualboot/$name/upstream is not in the index"
    if ! git -C "$src" cat-file -e "$want^{commit}" 2>/dev/null; then
        # A pinned commit alone (GitHub serves single commits); a ref: all.
        if [[ "$want" =~ ^[0-9a-f]{40}$ ]]; then
            git -C "$src" fetch --quiet --depth 1 origin "$want"
        else
            git -C "$src" fetch --quiet origin
        fi
    fi
    git -C "$src" checkout --quiet --detach "$want"
    git -C "$src" reset --quiet --hard
    git -C "$src" clean --quiet -fdx
    git -C "$src" tag -f pxc-base >/dev/null
}

step_dualboot_one() { # <name>
    local name="$1" pin
    pin=$(dualboot_pin "$name")
    log "$name ($(dualboot_url "$name") @ ${pin:0:7} + dualboot/$name/patches)"
    [ -x "$ROOT/dualboot/$name/build.sh" ] || die "dualboot/$name/build.sh is not executable (chmod +x, git update-index --chmod=+x)"
    dualboot_checkout "$name"
    apply_series "$BUILD/dualboot/$name/src" "$ROOT/dualboot/$name/patches"
    mkdir -p "$BUILD/dualboot/$name/cache" "$DIST"
    "$ROOT/dualboot/$name/build.sh" "$BUILD/dualboot/$name/src" "$BUILD/dualboot/$name/cache" "$DIST/$name.bin"
    echo "dist/$name.bin: $(wc -c < "$DIST/$name.bin") bytes"
}

step_dualboot() {
    local name
    for name in $(dualboot_names); do
        step_dualboot_one "$name"
    done
}

# The updater links the flash routines of esp-rom-sys; it is pinned (=) to
# the version the firmware gets through esp-hal, so both write flash with
# the same code.
check_shared_versions() {
    local fw up
    fw=$(lock_field "$FW/Cargo.lock" esp-rom-sys version)
    up=$(lock_field "$ROOT/updater/Cargo.lock" esp-rom-sys version)
    [ "$fw" = "$up" ] || die "esp-rom-sys: firmware has $fw, updater $up; set the same version (=$fw) in updater/Cargo.toml"
}

# Images are built from source, never committed (the two partition tables
# in firmware/src are data the firmware migrates between).
check_no_binaries() {
    local bins
    bins=$(git -C "$ROOT" ls-files -- '*.bin' ':!firmware/src/partitions-*.bin')
    [ -z "$bins" ] || die "built images are committed (build them instead): $bins"
}

step_check() {
    log "consistency checks"
    check_shared_versions
    check_no_binaries
    [ -n "$WIZNET_VERSION" ] && [ -n "$WIZNET_SHA256" ] || die "embassy-net-wiznet missing from firmware/Cargo.lock"
    local name pins=""
    for name in $(dualboot_names); do
        [ -n "$(dualboot_pin "$name")" ] || die "dualboot/$name/upstream is not in the index"
        [ -n "$(dualboot_url "$name")" ] || die "dualboot/$name has no entry in .gitmodules"
        git -C "$ROOT" config -f .gitmodules "submodule.dualboot/$name.branch" >/dev/null ||
            die "dualboot/$name has no branch in .gitmodules (what Dependabot and refresh-patches.sh latest follow)"
        [ -x "$ROOT/dualboot/$name/build.sh" ] || die "dualboot/$name/build.sh is not executable"
        pins+=", $name $(dualboot_pin "$name" | cut -c1-7)"
    done
    # The CI container builds the bootloader: same ESP-IDF as the pin.
    grep -q "container: espressif/idf:$IDF_VERSION\$" "$ROOT/.github/workflows/build.yml" ||
        die "IDF_VERSION $IDF_VERSION: .github/workflows/build.yml uses another espressif/idf container"
    echo "embassy-net-wiznet $WIZNET_VERSION$pins, esp-rom-sys $(lock_field "$FW/Cargo.lock" esp-rom-sys version)"
}

step_image() {
    log "full flash image"
    for f in bootloader.bin partitions.bin esp32-pixelcast.bin; do
        [ -f "$DIST/$f" ] || die "dist/$f missing: build it first"
    done
    python3 - "$DIST" "$DUALBOOT.bin" "$FLASH_SIZE" "$BOOTLOADER_AT" "$TABLE_AT" "$OTA0_AT" "$OTA1_AT" <<'EOF'
import os, sys
dist, other, size, bl, table, ota0, ota1 = sys.argv[1], sys.argv[2], *(int(a, 16) for a in sys.argv[3:])
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
if os.path.exists(os.path.join(dist, other)):
    put(ota1, other, ota1 - ota0)
else:
    print(f"  (no dist/{other}: ota_1 stays empty)")
open(os.path.join(dist, "full-4MB.bin"), "wb").write(img)
EOF
    echo "dist/full-4MB.bin: flash once over USB with"
    echo "  espflash write-bin 0x0 dist/full-4MB.bin"
    echo "(this erases the settings; later updates go over the network)"
}

main() {
    local steps=("$@") s
    [ ${#steps[@]} -eq 0 ] && steps=(all)
    for s in "${steps[@]}"; do
        case "$s" in
            all) step_check; step_deps; step_bootloader; step_updater; step_firmware; step_dualboot; step_image ;;
            check | deps | bootloader | updater | firmware | dualboot | image) "step_$s" ;;
            *)
                is_dualboot "$s" ||
                    die "unknown step: $s (check deps bootloader updater firmware dualboot $(dualboot_names | tr '\n' ' ')image all)"
                step_dualboot_one "$s"
                ;;
        esac
    done
}

# Sourced by tools/cargo.sh and tools/refresh-patches.sh for its functions.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
