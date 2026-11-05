#!/usr/bin/env bash
# Host tests for the hardware-free part of the firmware (firmware/core):
# protocols, LED mapping and encoding, merging, mDNS, HTTP parsing, OLED drawing.
set -euo pipefail
cd "$(dirname "$0")/../firmware/core"
export PATH="$HOME/.cargo/bin:$PATH"  # rustup's cargo (for +stable)
# firmware/.cargo passes -nostartfiles for the chip; not for the host.
export RUSTFLAGS=" "
exec cargo +stable test --target "$(rustc +stable -vV | sed -n 's/^host: //p')" "$@"
