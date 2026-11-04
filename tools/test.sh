#!/bin/bash
# Host tests for the hardware-free code (core/): protocols, LED mapping and
# encoding, merging, mDNS, HTTP parsing, OLED drawing.
set -euo pipefail
cd "$(dirname "$0")/../core"
# The repo config passes -nostartfiles for the chip; not for the host.
export RUSTFLAGS=" "
exec cargo +stable test --target "$(rustc +stable -vV | sed -n 's/^host: //p')" "$@"
