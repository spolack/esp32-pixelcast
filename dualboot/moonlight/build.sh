#!/usr/bin/env bash
# Builds MoonLight for this board. Called by tools/build.sh (see ../README.md):
#   build.sh <source> <cache> <image>
# <source>: upstream at the pinned commit with patches/ applied (cleaned
# before every build); <cache>: kept between builds; <image>: where the
# app image goes.
#
# ESP-IDF v6.1 (MoonLight's pin): a local one (idf.py, or IDF_PATH), or else
# the espressif/idf:v6.1 image through podman or docker (IDF_CONTAINER=1
# prefers that; CI uses it).
set -euo pipefail
src="$1" cache="$2" out="$3"
IDF_VERSION=v6.1
# The version in the app descriptor (esp32-pixelcast's update page shows it):
# MoonLight's own (library.json) and the pinned commit; IDF would take
# `git describe` of the patched checkout.
version="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["version"])' "$src/library.json")"
version="$version-$(git -C "$src" rev-parse --short=7 pxc-base 2>/dev/null || git -C "$src" rev-parse --short=7 HEAD)"
ARGS=(-D IDF_TARGET=esp32
      -D "SDKCONFIG_DEFAULTS=sdkconfig.defaults;sdkconfig.defaults.pixelcast"
      -D MM_FIRMWARE_NAME=esp32-pixelcast
      -D "PROJECT_VER=$version")

mkdir -p "$cache/build" "$cache/components"
# IDF reads the defaults only when it makes a new sdkconfig: changed defaults
# (or a cache restored from an older build) would otherwise not count.
rm -f "$cache/build/sdkconfig"
if [ -z "${IDF_CONTAINER:-}" ] && { command -v idf.py >/dev/null 2>&1 || [ -n "${IDF_PATH:-}" ]; }; then
    if ! command -v idf.py >/dev/null 2>&1; then
        # shellcheck disable=SC1091
        . "$IDF_PATH/export.sh" >/dev/null
    fi
    idf.py --version | grep -q "$IDF_VERSION" ||
        echo "warning: ESP-IDF is not $IDF_VERSION ($(idf.py --version))"
    IDF_COMPONENT_CACHE_PATH="$cache/components" \
        idf.py -C "$src/esp32" -B "$cache/build" -D SDKCONFIG="$cache/build/sdkconfig" "${ARGS[@]}" build
else
    rt=$(command -v podman || command -v docker) ||
        { echo "no ESP-IDF $IDF_VERSION: install it and source its export.sh (or set IDF_PATH), or install podman or docker" >&2; exit 1; }
    user=()
    # Rootless podman maps the container's root to this user; docker needs it.
    [ "${rt##*/}" = docker ] && user=(--user "$(id -u):$(id -g)" -e HOME=/tmp)
    echo "in espressif/idf:$IDF_VERSION ($rt)"
    # MoonLight's build reads its git hash: the checkout belongs to another user in the container.
    "$rt" run --rm "${user[@]}" \
        -v "$src:/src:z" -v "$cache:/cache:z" \
        -e IDF_COMPONENT_CACHE_PATH=/cache/components \
        "docker.io/espressif/idf:$IDF_VERSION" \
        bash -c 'git config --global --add safe.directory "*" && idf.py -C /src/esp32 -B /cache/build -D SDKCONFIG=/cache/build/sdkconfig "$@" build' \
        _ "${ARGS[@]}"
fi
cp "$cache/build/MoonLight.bin" "$out"
