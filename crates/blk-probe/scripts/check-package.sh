#!/usr/bin/env bash
# Refuse a release tarball that is not the one scripts/package.sh describes.
#
# Checked, in order: the .sha256 beside it matches; it holds bin/blk.probe and
# LICENSE and nothing else; the binary is built for the platform the name
# claims; and it runs -- `--version` names the version the tarball's name
# claims, `--help` prints the usage line, and it probes a GPT image sgdisk
# wrote from the committed corpus as a GPT.
#
# ci.yml runs this on every pull request against the tarball it has just
# built, and release.yml runs it before anything is attested or uploaded, so a
# tarball that fails here is never published. tests/scripts/test-package.sh
# proves it refuses what it exists to refuse.
#
# Usage: scripts/check-package.sh <dir>/rust-blk-probe-<version>-<platform>.tar.gz
set -uo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: scripts/check-package.sh <tarball>" >&2
    exit 1
fi
tarball="$1"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
name="$(basename "$tarball")"
dir="$(cd "$(dirname "$tarball")" && pwd)"

refuse() { echo "check-package: $name: $*" >&2; exit 1; }

[[ -f "$tarball" ]] || refuse "no such file"

platform="$(sed -E 's/^rust-blk-probe-.*-([a-z]+-[a-z0-9_]+)\.tar\.gz$/\1/' <<<"$name")"
[[ "$platform" != "$name" ]] || refuse "not named rust-blk-probe-<version>-<platform>.tar.gz"
version="${name#rust-blk-probe-}"
version="${version%-"$platform".tar.gz}"

# --- the checksum ------------------------------------------------------------
[[ -f "$dir/$name.sha256" ]] || refuse "no $name.sha256 beside it"
(cd "$dir" && shasum -a 256 -c "$name.sha256" >/dev/null 2>&1) ||
    refuse "does not match $name.sha256"

# --- the layout: exactly these entries --------------------------------------
# LC_ALL=C: the order is compared as text, and a UTF-8 locale sorts
# `LICENSE` after `bin/` where the C locale sorts it before.
entries="$(tar -tzf "$tarball" | sed 's|^\./||; /^$/d' | LC_ALL=C sort | tr '\n' ' ')"
want="LICENSE bin/ bin/blk.probe "
[[ "$entries" == "$want" ]] || refuse "holds [${entries% }], expected [${want% }]"

work="$(mktemp -d "${TMPDIR:-/tmp}/blk.probe-check.XXXXXX")"
trap 'rm -rf "$work"' EXIT
tar -xzf "$tarball" -C "$work"
probe="$work/bin/blk.probe"
[[ -x "$probe" ]] || refuse "bin/blk.probe is not executable"

# --- the binary is for the platform the name claims -------------------------
command -v file >/dev/null ||
    refuse "file(1) is missing, and it is what reads the binary's platform; install it (apt-get install file)"
kind="$(file -b "$probe")"
case "$platform" in
    darwin-arm64) want_kind='Mach-O.*arm64' ;;
    linux-x86_64) want_kind='ELF.*x86-64' ;;
    linux-arm64) want_kind='ELF.*(aarch64|ARM)' ;;
    *) refuse "no rule for the platform $platform" ;;
esac
grep -Eq "$want_kind" <<<"$kind" || refuse "is named $platform and holds: $kind"

# --- it runs -------------------------------------------------------------------
# Only where it can: a tarball for another platform has already been refused
# above, so reaching here means the binary is this host's.
# The line a package formula's test and an installer read to know they have
# this program at this version (#47).
said="$("$probe" --version 2>&1)" || refuse "bin/blk.probe --version exited $?: $said"
[[ "$said" == "blk.probe (rust-blk-probe) $version" ]] ||
    refuse "bin/blk.probe --version printed [$said], expected [blk.probe (rust-blk-probe) $version]"

usage="$("$probe" --help 2>&1)" || refuse "bin/blk.probe --help exited $?"
[[ "$usage" == "usage: blk.probe "* ]] || refuse "bin/blk.probe --help printed: $usage"

doc="$("$probe" "$here/fuzz/corpus/probe/gpt.img" 2>&1)" ||
    refuse "bin/blk.probe exited $? on the corpus GPT: $doc"
[[ "$doc" == *'"table":"gpt"'* ]] || refuse "bin/blk.probe did not read the corpus GPT as one: $doc"

echo "check-package: $name holds bin/blk.probe and LICENSE, for $platform, and runs"
