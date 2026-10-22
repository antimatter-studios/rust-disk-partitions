#!/usr/bin/env bash
# Package a built blk_probe as the release tarball.
#
# THE TARBALL IS THE CONTRACT. A consumer downloads it, unpacks it and runs
# bin/blk.probe by that path, so its layout is fixed and checked -- by
# scripts/check-package.sh, which ci.yml runs on every pull request and
# release.yml runs before anything is attested or uploaded:
#
#   bin/blk.probe   the probe, renamed from the cargo target by stage-binary.sh
#   LICENSE
#
# It is named rust-blk-probe-<version>-<platform>.tar.gz, after the
# repository and not the tool, and a <name>.sha256 is written beside it.
#
# The rename is stage-binary.sh's, not a second copy of it here: cargo refuses
# a dot in a target name, and there is one place that knows that.
#
# Usage: scripts/package.sh <built-binary> <version> <platform> <out-dir>
#   e.g. scripts/package.sh target/release/blk_probe 0.1.0 darwin-arm64 dist
set -euo pipefail

if [[ $# -ne 4 ]]; then
    echo "usage: scripts/package.sh <built-binary> <version> <platform> <out-dir>" >&2
    exit 1
fi
binary="$1"
version="$2"
platform="$3"
out="$4"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
stage="$(mktemp -d "${TMPDIR:-/tmp}/blk.probe-package.XXXXXX")"
trap 'rm -rf "$stage"' EXIT

bash "$here/scripts/stage-binary.sh" "$stage/bin" "$binary" >/dev/null
# The licence is the repository's, at the workspace root (#150).
cp "$here/../../LICENSE" "$stage/LICENSE"

name="rust-blk-probe-$version-$platform.tar.gz"
mkdir -p "$out"
# COPYFILE_DISABLE: macOS's tar otherwise stores extended attributes as
# `._bin` entries beside the real ones, and the layout is exactly two files.
COPYFILE_DISABLE=1 tar -C "$stage" -czf "$out/$name" bin LICENSE
(cd "$out" && shasum -a 256 "$name" >"$name.sha256")
echo "$out/$name"
