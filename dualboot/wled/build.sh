#!/usr/bin/env bash
# Builds WLED for this board. Called by tools/build.sh (see ../README.md):
#   build.sh <source> <cache> <image>
# <source>: upstream at the pinned commit with patches/ applied (cleaned
# before every build); <cache>: kept between builds; <image>: where the
# app image goes.
set -euo pipefail
src="$1" cache="$2" out="$3"
ENV=esp32dev_W5500

command -v pio >/dev/null 2>&1 || { echo "pio not found: pip install platformio" >&2; exit 1; }
# WLED bundles its web pages with Node (npm ci, npm run build).
command -v npm >/dev/null 2>&1 || { echo "npm not found: install Node.js 20" >&2; exit 1; }

# PlatformIO's build tree outside the source, so it survives the cleaning.
export PLATFORMIO_WORKSPACE_DIR="$cache/pio"
(cd "$src" && pio run -e "$ENV")
cp "$PLATFORMIO_WORKSPACE_DIR/build/$ENV/firmware.bin" "$out"
