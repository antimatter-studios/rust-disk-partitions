#!/usr/bin/env bash
# The tool is staged as `blk.probe`, and nothing named `blk-probe` is built.
#
# Cargo refuses a dot in a target name, so the [[bin]] is `blk_probe` and
# scripts/stage-binary.sh renames it on the way out -- the same split as a
# `mkfs_ext4` target shipped as `mkfs.ext4`. The rename is the part a
# consumer sees, and a consumer that invokes the tool by name fails at run
# time, not at build time, when the two drift. So this builds the real
# binary, stages it the way `chore binary` does, and runs what was staged.
#
#   bash tests/scripts/test-staged-binary-name.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# The workspace root, which holds chores.yml and the target directory (#150).
ROOT="$(cd "$REPO/../.." && pwd)"
cd "$REPO" || exit 1

fails=0
ok()   { echo "ok    $*"; }
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

mkdir -p "$REPO/tmp"
SANDBOX="$(mktemp -d "$REPO/tmp/staged-binary-name.XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT HUP INT TERM

# --- 1. Cargo builds exactly one binary, and its name has no hyphen. -------
#
# Cargo's own answer, not a grep of Cargo.toml: a second [[bin]] or an
# auto-discovered src/bin/*.rs would be a second executable nobody renamed.
bins="$(cargo metadata --no-deps --format-version 1 --offline 2>"$SANDBOX/metadata.err" |
    python3 -c 'import json,sys
for p in json.load(sys.stdin)["packages"]:
    for t in p["targets"]:
        if "bin" in t["kind"]:
            print(t["name"])')"
if [[ "$bins" == "blk_probe" ]]; then
    ok "cargo builds one binary target, blk_probe"
else
    fail "cargo's binary targets are [${bins//$'\n'/, }], expected [blk_probe]"
    cat "$SANDBOX/metadata.err" >&2
fi

# --- 2. The staging script exists and names its output blk.probe. ----------
STAGE="$REPO/scripts/stage-binary.sh"
if [[ ! -f "$STAGE" ]]; then
    fail "scripts/stage-binary.sh is missing; it is what renames the cargo target"
else
    if ! cargo build --locked --quiet --bin blk_probe >"$SANDBOX/build.log" 2>&1; then
        fail "cargo build --bin blk_probe failed:"
        cat "$SANDBOX/build.log" >&2
    elif ! bash "$STAGE" "$SANDBOX/dist" "$(cargo metadata --no-deps --format-version 1 --offline |
        python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/debug/blk_probe" \
        >"$SANDBOX/stage.log" 2>&1; then
        fail "scripts/stage-binary.sh failed:"
        cat "$SANDBOX/stage.log" >&2
    else
        staged="$(cd "$SANDBOX/dist" && ls -A)"
        if [[ "$staged" == "blk.probe" ]]; then
            ok "the staged directory holds blk.probe and nothing else"
        else
            fail "the staged directory holds [${staged//$'\n'/, }], expected [blk.probe]"
        fi
        if [[ -x "$SANDBOX/dist/blk.probe" ]]; then
            ok "blk.probe is executable"
        else
            fail "dist/blk.probe is not an executable file"
        fi

        # --- 3. The staged binary calls itself blk.probe. -------------------
        usage="$("$SANDBOX/dist/blk.probe" --help 2>&1)"
        if [[ "$usage" == "usage: blk.probe <path> "* ]]; then
            ok "--help names the tool blk.probe"
        else
            fail "--help printed: $usage"
        fi
        err="$("$SANDBOX/dist/blk.probe" --no-such-flag 2>&1 >/dev/null)"
        status=$?
        if [[ "$status" -eq 1 && "$err" == "blk.probe: unknown flag: --no-such-flag"* ]]; then
            ok "an error is prefixed blk.probe: and exits 1"
        else
            fail "an unknown flag exited $status and printed: $err"
        fi
    fi
fi

# --- 4. The chore tasks stage and report the same name. --------------------
if grep -qF 'crates/blk-probe/scripts/stage-binary.sh' "$ROOT/chores.yml"; then
    ok "chore binary stages through scripts/stage-binary.sh"
else
    fail "chores.yml's binary task does not call scripts/stage-binary.sh"
fi
if grep -qF 'printf "/blk.probe\n"' "$ROOT/chores.yml"; then
    ok "chore artifact prints a path ending in /blk.probe"
else
    fail "chores.yml's artifact task does not print .../blk.probe"
fi

# --- 5. No executable anywhere in the tree is called blk-probe. ------------
#
# A tracked file with that name, or a task that writes one, is the second
# similarly named tool this rename exists to remove.
if git ls-files | grep -qE '(^|/)blk-probe$'; then
    fail "a tracked file is named blk-probe"
else
    ok "no tracked file is named blk-probe"
fi

if [[ "$fails" -gt 0 ]]; then
    echo "FAIL  $fails check(s) failed" >&2
    exit 1
fi
echo "PASS  the tool is built as blk_probe and staged as blk.probe"
