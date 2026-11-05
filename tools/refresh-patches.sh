#!/usr/bin/env bash
# Moves a patch series onto another upstream base and writes it back, so the
# series applies cleanly again (fresh context and offsets).
#
#   tools/refresh-patches.sh <name> [<commit> | latest]   dualboot/<name>, e.g. wled
#   tools/refresh-patches.sh embassy-net-wiznet [<version>]
#   tools/refresh-patches.sh <name | embassy-net-wiznet> --export
#
# Without a target the series is refreshed against the current pin. With
# one, the pin moves too: the dualboot/<name>/upstream pointer (staged),
# embassy-net-wiznet's version in firmware/Cargo.lock (cargo update --precise).
#
# The series is applied with git am -3. If a patch conflicts, the script
# stops with the tree in build/dualboot/<name>/src or
# build/deps/embassy-net-wiznet: fix the files there, `git add` them,
# `git am --continue` (repeat), then run this script with --export to write
# the series. Check the result with tools/build.sh <name> (or deps firmware)
# before committing.
set -euo pipefail
# shellcheck source=build.sh
. "$(dirname "${BASH_SOURCE[0]}")/build.sh"

usage() { sed -n '2,18p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2; }

# export_series <tree> <patch dir>: base (tag pxc-base)..HEAD as numbered
# patches, the way they are committed.
export_series() {
    local tree="$1" dir="$2"
    [ -d "$tree/.git/rebase-apply" ] && die "git am is still running in $tree: finish it first (git am --continue)"
    [ -d "$tree/.git/rebase-merge" ] && die "a rebase is still running in $tree: finish it first (git rebase --continue)"
    git -C "$tree" rev-parse -q --verify pxc-base >/dev/null || die "$tree has no pxc-base tag"
    mkdir -p "$dir"
    rm -f "$dir"/*.patch
    git -C "$tree" format-patch --quiet --no-signature --zero-commit -o "$dir" pxc-base..HEAD
    echo "wrote:"
    ls "$dir"
    git -C "$ROOT" status --short -- "$dir"
}

# am_series <tree> <patch dir> <name>: like apply_series, but stops with advice.
am_series() {
    local tree="$1" dir="$2" name="$3" patches
    mapfile -t patches < <(series_patches "$dir")
    [ ${#patches[@]} -gt 0 ] || { echo "no patches in ${dir#"$ROOT"/}"; return 0; }
    if ! git -C "$tree" "${GIT_ID[@]}" am --quiet --keep-cr -3 "${patches[@]}"; then
        cat >&2 <<EOT

${dir#"$ROOT"/}: a patch does not apply to the new base (see above).
Resolve in $tree:
  edit the conflicting files, git add them, git am --continue   (repeat)
then write the series back:
  tools/refresh-patches.sh $name --export
EOT
        exit 1
    fi
}

refresh_dualboot() {
    local name="$1" target="${2:-}" src="$BUILD/dualboot/$1/src" series="$ROOT/dualboot/$1/patches"
    if [ "$target" = "--export" ]; then
        export_series "$src" "$series"
        return
    fi
    [ -d "$src/.git" ] && git -C "$src" am --abort >/dev/null 2>&1 || true
    local was
    was=$(dualboot_pin "$name")
    case "$target" in
        "") target="$was" ;;
        latest)
            dualboot_checkout "$name" "$was"
            local branch
            branch=$(git -C "$ROOT" config -f .gitmodules "submodule.dualboot/$name.branch") ||
                die "dualboot/$name has no branch in .gitmodules"
            git -C "$src" fetch --quiet origin "$branch"
            target=$(git -C "$src" rev-parse FETCH_HEAD)
            ;;
    esac
    dualboot_checkout "$name" "$target"
    target=$(git -C "$src" rev-parse HEAD)
    log "dualboot/$name/patches onto ${target:0:7} (was ${was:0:7})"
    git -C "$ROOT" update-index --add --cacheinfo "160000,$target,dualboot/$name/upstream"
    am_series "$src" "$series" "$name"
    export_series "$src" "$series"
}

refresh_wiznet() {
    local target="${1:-}" dst="$BUILD/deps/embassy-net-wiznet"
    if [ "$target" = "--export" ]; then
        export_series "$dst" "$FW/patches/embassy-net-wiznet"
        return
    fi
    rust_env
    if [ -n "$target" ] && [ "$target" != "$WIZNET_VERSION" ]; then
        log "embassy-net-wiznet $WIZNET_VERSION -> $target in firmware/Cargo.lock"
        (cd "$FW" && cargo update --quiet -p embassy-net-wiznet --precise "$target")
        WIZNET_VERSION=$(lock_field "$FW/Cargo.lock" embassy-net-wiznet version)
        WIZNET_SHA256=$(lock_field "$FW/Cargo.lock" embassy-net-wiznet checksum)
    fi
    # The pristine release as base (step_deps without the series).
    rm -f "$BUILD/deps/embassy-net-wiznet.stamp"
    local series="$FW/patches/embassy-net-wiznet"
    PXC_BASE_ONLY=1 step_deps
    log "firmware/patches/embassy-net-wiznet onto $WIZNET_VERSION"
    am_series "$dst" "$series" embassy-net-wiznet
    export_series "$dst" "$series"
    # Stale stamp: the next build applies the written series afresh.
    rm -f "$BUILD/deps/embassy-net-wiznet.stamp"
}

case "${1:-}" in
    embassy-net-wiznet) shift; refresh_wiznet "$@" ;;
    *)
        is_dualboot "${1:-}" || usage
        refresh_dualboot "$@"
        ;;
esac
