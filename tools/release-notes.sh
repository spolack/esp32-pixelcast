#!/usr/bin/env bash
# Release notes for a commit, on stdout: the commits since the previous v*
# tag, then .github/release-notes.md (which file is what).
#
# Usage: tools/release-notes.sh [ref]   (default: HEAD)
#
# For a commit tagged v* (a release) the range starts at the v* tag before
# it; for any other commit (the nightly) at the latest v* tag below it.
# Without such a tag: all commits. With GITHUB_REPOSITORY set (CI), a compare
# link follows the list. Needs the history and tags (CI: fetch-depth: 0).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ref="${1:-HEAD}"
cd "$ROOT"

sha=$(git rev-parse --verify "$ref^{commit}")
if tag=$(git describe --tags --exact-match --match 'v*' "$sha" 2>/dev/null); then
    base=$(git describe --tags --abbrev=0 --match 'v*' "$sha^" 2>/dev/null || true)
else
    tag=""
    base=$(git describe --tags --abbrev=0 --match 'v*' "$sha" 2>/dev/null || true)
fi

if [ -n "$base" ]; then
    printf '## Changes since %s\n\n' "$base"
    range="$base..$sha"
else
    printf '## Changes\n\n'
    range="$sha"
fi

# Subjects without the "esp32-pixelcast: " prefix, newest first.
changes=$(git log --no-merges --format='- %s (%h)' "$range" | sed 's/^- esp32-pixelcast: /- /')
if [ -n "$changes" ]; then
    printf '%s\n' "$changes"
else
    printf -- '- no changes\n'
fi

if [ -n "$base" ] && [ -n "${GITHUB_REPOSITORY:-}" ]; then
    printf '\nFull diff: %s/%s/compare/%s...%s\n' \
        "${GITHUB_SERVER_URL:-https://github.com}" "$GITHUB_REPOSITORY" "$base" "${tag:-$sha}"
fi

printf '\n## Files\n\n'
cat "$ROOT/.github/release-notes.md"
