#!/usr/bin/env bash
# Builds the updater and converts it to an ESP image (default: updater.bin
# here, the prebuilt copy the firmware embeds unless PXC_UPDATER_BIN says
# otherwise). Needs the Rust esp toolchain and esptool (pip install esptool,
# or ESP-IDF's, found through IDF_PATH).
set -euo pipefail
cd "$(dirname "$0")"
OUT="${1:-updater.bin}"
export PATH="$HOME/.cargo/bin:$PATH"
[ -f "$HOME/export-esp.sh" ] && . "$HOME/export-esp.sh" >/dev/null 2>&1 || true
cargo build --release
ELF=target/xtensa-esp32-none-elf/release/updater
if python3 -m esptool version >/dev/null 2>&1; then
    ESPTOOL=(python3 -m esptool)
elif command -v esptool.py >/dev/null 2>&1; then
    ESPTOOL=(esptool.py)
elif [ -n "${IDF_PATH:-}" ]; then
    # shellcheck disable=SC1091
    . "$IDF_PATH/export.sh" >/dev/null
    ESPTOOL=(python -m esptool)
else
    echo "esptool not found: pip install esptool, or set IDF_PATH" >&2
    exit 1
fi
mkdir -p "$(dirname "$OUT")"
"${ESPTOOL[@]}" --chip esp32 elf2image --output "$OUT" "$ELF" >/dev/null
echo "$OUT: $(stat -c %s "$OUT") bytes"
