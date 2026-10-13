#!/bin/bash
# Prints the DIT-01 version of this checkout without the leading "v": <SERIES>.<n>, n with three
# digits, "-dev" when the working tree has uncommitted changes (rule in VERSION). The one source
# of the number: the Rust screens (crates/dit-ui-core/build.rs), the image, its boot menu and
# manifest all take it from here.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
series="$(sed -n 's/^SERIES=//p' "$ROOT_DIR/VERSION")"
first="$(sed -n 's/^FIRST=//p' "$ROOT_DIR/VERSION")"
[[ "$series" =~ ^[0-9]+\.[0-9]+$ && "$first" =~ ^[0-9]+$ ]] \
    || { printf 'VERSION is malformed\n' >&2; exit 2; }

if ! count="$(git -C "$ROOT_DIR" rev-list --count HEAD 2>/dev/null)"; then
    # Not a git checkout (a source copy): the series is all that is known.
    printf '%s.unknown\n' "$series"
    exit 0
fi
dirty=0
[[ -z "$(git -C "$ROOT_DIR" status --porcelain --untracked-files=no 2>/dev/null)" ]] || dirty=1
n=$((count - first + dirty))
if (( n < 0 )); then
    # A checkout from before the series began.
    printf '%s.000-pre\n' "$series"
elif (( dirty )); then
    printf '%s.%03d-dev\n' "$series" "$n"
else
    printf '%s.%03d\n' "$series" "$n"
fi
