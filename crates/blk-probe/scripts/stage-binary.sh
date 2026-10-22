#!/usr/bin/env bash
# Stage the built probe into <out>/blk.probe.
#
#   scripts/stage-binary.sh <out> <built-binary> [<built-binary>...]
#
# THE TOOL IS `blk.probe`; THE CARGO TARGET IS `blk_probe`. Cargo refuses a
# dot in a target name ("invalid character '.' in crate name"), so the
# underscore is a build-system constraint and this is where it stops: the
# name a person types and a consumer invokes is the dotted one, and it is
# never spelled with a hyphen.
#
# One input is copied. Several are one binary per architecture and are
# joined with lipo into a single universal file, which is what `chore binary`
# passes. Either way <out> ends up holding blk.probe and nothing this script
# did not put there.
set -euo pipefail

TOOL=blk.probe

if [[ $# -lt 2 ]]; then
    echo "usage: scripts/stage-binary.sh <out> <built-binary> [<built-binary>...]" >&2
    exit 1
fi
out="$1"
shift
for b in "$@"; do
    [[ -x "$b" ]] || { echo "stage-binary: not an executable: $b" >&2; exit 1; }
done

mkdir -p "$out"
if [[ $# -eq 1 ]]; then
    cp "$1" "$out/$TOOL"
else
    lipo -create "$@" -output "$out/$TOOL"
fi
chmod +x "$out/$TOOL"
echo "$out/$TOOL"
