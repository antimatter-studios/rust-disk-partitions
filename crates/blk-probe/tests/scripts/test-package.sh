#!/usr/bin/env bash
# The release tarball holds bin/blk.probe and LICENSE, and the probe in it runs.
#
# The tarball is the contract: a consumer that downloads it unpacks it and
# runs bin/blk.probe by that path. So this builds the real binary, packages it
# with scripts/package.sh exactly as ci.yml and release.yml do, and holds the
# result to scripts/check-package.sh -- and then proves that check FAILS on a
# tarball with a file too many, a missing LICENSE, a checksum that does not
# match, a platform the binary was not built for and a version it does not
# report. A check that cannot fail is indistinguishable from no check.
#
#   bash tests/scripts/test-package.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO" || exit 1

fails=0
ok()   { echo "ok    $*"; }
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

mkdir -p "$REPO/tmp"
SANDBOX="$(mktemp -d "$REPO/tmp/package.XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT HUP INT TERM

PACKAGE="$REPO/scripts/package.sh"
CHECK="$REPO/scripts/check-package.sh"
for s in "$PACKAGE" "$CHECK"; do
    [[ -f "$s" ]] || { echo "FAIL  ${s#"$REPO"/} is missing" >&2; exit 1; }
done

# The platform this host builds for, in the tarball's spelling.
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) PLATFORM=darwin-arm64 ; OTHER=linux-x86_64 ;;
    Linux-x86_64) PLATFORM=linux-x86_64 ; OTHER=darwin-arm64 ;;
    Linux-aarch64) PLATFORM=linux-arm64 ; OTHER=darwin-arm64 ;;
    *) echo "FAIL  no tarball platform for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
# The version the binary reports, which check-package.sh holds the name to.
VERSION="$(grep -m1 '^version = ' "$REPO/Cargo.toml" | sed 's/version = //; s/"//g')"
NAME="rust-blk-probe-$VERSION-$PLATFORM.tar.gz"

if ! cargo build --locked --quiet --bin blk_probe >"$SANDBOX/build.log" 2>&1; then
    echo "FAIL  cargo build --bin blk_probe failed:" >&2
    cat "$SANDBOX/build.log" >&2
    exit 1
fi

# Cargo's own answer for where it put the binary: CARGO_TARGET_DIR or a
# workspace can move it, and a stale target/ from another host is the wrong
# binary with the right name.
TARGET_DIR="$(cargo metadata --no-deps --format-version 1 --offline |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"

# --- 1. The tarball is written, named for its version and platform. --------
if bash "$PACKAGE" "$TARGET_DIR/debug/blk_probe" "$VERSION" "$PLATFORM" "$SANDBOX/dist" \
    >"$SANDBOX/package.log" 2>&1; then
    ok "scripts/package.sh wrote a tarball"
else
    fail "scripts/package.sh failed:"
    cat "$SANDBOX/package.log" >&2
fi
if [[ -f "$SANDBOX/dist/$NAME" && -f "$SANDBOX/dist/$NAME.sha256" ]]; then
    ok "it is $NAME, with a .sha256 beside it"
else
    fail "expected $NAME and $NAME.sha256, found [$(ls -A "$SANDBOX/dist" 2>/dev/null | tr '\n' ' ')]"
fi

# --- 2. The check accepts it, and says what it ran. -------------------------
if bash "$CHECK" "$SANDBOX/dist/$NAME" >"$SANDBOX/check.log" 2>&1; then
    ok "scripts/check-package.sh accepts it"
else
    fail "scripts/check-package.sh refuses the tarball scripts/package.sh wrote:"
    cat "$SANDBOX/check.log" >&2
fi

# --- 3. The check refuses what it exists to refuse. -------------------------
# Each case repackages the good tarball's contents with one thing wrong.
unpacked="$SANDBOX/unpacked"
mkdir -p "$unpacked"
tar -xzf "$SANDBOX/dist/$NAME" -C "$unpacked"

refused() { # <description> <tarball>
    if bash "$CHECK" "$2" >"$SANDBOX/refused.log" 2>&1; then
        fail "scripts/check-package.sh accepted a tarball with $1"
    else
        ok "scripts/check-package.sh refuses a tarball with $1: $(tail -1 "$SANDBOX/refused.log")"
    fi
}

repack() { # <dir> <tarball-name>; writes the tarball and its checksum
    mkdir -p "$SANDBOX/bad"
    (cd "$1" && COPYFILE_DISABLE=1 tar -czf "$SANDBOX/bad/$2" -- *)
    (cd "$SANDBOX/bad" && shasum -a 256 "$2" >"$2.sha256")
    echo "$SANDBOX/bad/$2"
}

extra="$SANDBOX/extra"
cp -R "$unpacked" "$extra"
echo stray >"$extra/README"
refused "a file too many" "$(repack "$extra" "$NAME")"

nolicence="$SANDBOX/nolicence"
cp -R "$unpacked" "$nolicence"
rm "$nolicence/LICENSE"
refused "no LICENSE" "$(repack "$nolicence" "$NAME")"

badsum="$(repack "$unpacked" "$NAME")"
echo "0000000000000000000000000000000000000000000000000000000000000000  $NAME" >"$badsum.sha256"
refused "a checksum that does not match it" "$badsum"

refused "a platform its binary was not built for" \
    "$(repack "$unpacked" "rust-blk-probe-$VERSION-$OTHER.tar.gz")"

refused "a version its binary does not report" \
    "$(repack "$unpacked" "rust-blk-probe-0.0.0-$PLATFORM.tar.gz")"

if (( fails > 0 )); then
    echo "FAIL  $fails check(s) failed" >&2
    exit 1
fi
echo "PASS  the release tarball holds bin/blk.probe and LICENSE, and the probe in it runs"
