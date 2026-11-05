#!/usr/bin/env bash
# cargo in firmware/ with the patched embassy-net-wiznet and the bootloader
# and updater the firmware embeds (all built on first use, like
# tools/build.sh deps bootloader updater). Arguments go to cargo, e.g.
#   tools/cargo.sh run --release      # flash over USB, serial monitor
#   tools/cargo.sh clippy --release
# firmware/Cargo.lock is left as committed. Dependency updates (cargo update
# in firmware/) need no patch: run plain cargo for those.
set -euo pipefail
# shellcheck source=build.sh
. "$(dirname "${BASH_SOURCE[0]}")/build.sh"
rust_env
cargo_patched "$@"
