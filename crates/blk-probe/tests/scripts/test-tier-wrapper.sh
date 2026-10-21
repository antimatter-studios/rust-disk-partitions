#!/usr/bin/env bash
# rust-fs-core's scripts/tier.sh, run in place, keeps every promise chores.yml
# makes on its behalf.
#
# The runner is rust-fs-core's and is NOT committed here, so this guard drives
# the real one, from the ../rust-fs-core checkout at the pinned version, in a
# sandbox -- so a change in core's behaviour shows up here rather than in a CI
# log three repositories away.
#
# What is proven, and why each one is worth a check:
#
#   1. a green tier prints ONE line and puts the transcript in tmp/logs/
#   2. a tier that breaches its line budget exits 65, not 0 and not 1
#   3. a tier that breaches its byte budget exits 65 as well
#   4. a failing command's OWN status is what tier.sh exits with
#   5. a failure prints no tail by default, and OUTPUT_BUDGET_FAIL_TAIL brings
#      one back
#   6. --verbose streams the run AND still enforces the budget
#
# It accumulates failures rather than stopping at the first: a guard that
# stops early answers one question per run, and these checks are independent.
#
#   bash tests/scripts/test-tier-wrapper.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok()   { printf 'ok    %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1" >&2; fails=$(( fails + 1 )); }

# rust-fs-core is beside the repository, two levels above this crate (#150).
TIER="$REPO/../../../rust-fs-core/scripts/tier.sh"
if [ ! -f "$TIER" ]; then
    echo "FAIL  $TIER does not exist: rust-fs-core owns the runner and this" >&2
    echo "      repository keeps no copy of it. Clone it beside the repository." >&2
    exit 1
fi

# --- a sandbox repository with a good core beside it ----------------------
mkdir -p "$REPO/tmp"
SANDBOX="$(mktemp -d "$REPO/tmp/tier-wrapper.XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT HUP INT TERM

mkdir -p "$SANDBOX/repo"
# The runner logs in the repository it is run for; the sandbox sits inside
# this checkout, so it is named outright rather than found by git.
export FS_CORE_CALLER="$SANDBOX/repo"

# A stand-in for `cargo test`: N lines, a cargo-shaped result line, a status.
cat > "$SANDBOX/repo/fake-suite.sh" <<'FAKE'
#!/usr/bin/env bash
# fake-suite.sh LINES PASSED STATUS
for i in $(seq 1 "$1"); do echo "running fixture $i with some words on the line"; done
echo "test result: ok. $2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
exit "$3"
FAKE
chmod +x "$SANDBOX/repo/fake-suite.sh"

SUITE="$SANDBOX/repo/fake-suite.sh"
run() {  # run TIER-ARGS...; captures stdout+stderr in $out and status in $rc
    out="$(cd "$SANDBOX/repo" && "$@" 2>&1)"; rc=$?
}

# --- 1. a green tier is one line, and the transcript is on disk -----------
run bash "$TIER" "test (debug)" debug 50 4000 -- bash "$SUITE" 10 12 0
[ "$rc" -eq 0 ] && [ "$(printf '%s\n' "$out" | wc -l)" -eq 1 ] \
    && grep -q '^test (debug): ok (' <<<"$out" \
    && grep -q 'tmp/logs/debug.log' <<<"$out" \
    && ok "a green tier prints one verdict line naming its log" \
    || fail "a green tier printed status $rc and:"$'\n'"$out"
[ "$(grep -c . "$SANDBOX/repo/tmp/logs/debug.log")" -eq 11 ] \
    && ok "the whole transcript is in tmp/logs/debug.log" \
    || fail "the log does not hold the run: $(wc -l < "$SANDBOX/repo/tmp/logs/debug.log") lines"

# --- 2/3. over budget is 65, on lines and on bytes ------------------------
run bash "$TIER" over-lines over-lines 5 100000 -- bash "$SUITE" 40 12 0
[ "$rc" -eq 65 ] && grep -q 'passed, but printed' <<<"$out" \
    && ok "a tier over its LINE budget exits 65" \
    || fail "over the line budget gave status $rc:"$'\n'"$out"
run bash "$TIER" over-bytes over-bytes 100000 200 -- bash "$SUITE" 40 12 0
[ "$rc" -eq 65 ] && grep -q 'bytes (budget 200)' <<<"$out" \
    && ok "a tier over its BYTE budget exits 65" \
    || fail "over the byte budget gave status $rc:"$'\n'"$out"

# --- 4/5. the command's own status, and the tail ---------------------------
run bash "$TIER" failing failing 50 4000 -- bash "$SUITE" 3 0 7
[ "$rc" -eq 7 ] && grep -q 'failing: FAILED (exit 7)' <<<"$out" \
    && ok "a failing command's own status (7) is the tier's status" \
    || fail "a command exiting 7 gave status $rc:"$'\n'"$out"
grep -q 'running fixture' <<<"$out" \
    && fail "a failure printed the log back by default:"$'\n'"$out" \
    || ok "a failure prints no tail by default"
out="$(cd "$SANDBOX/repo" && OUTPUT_BUDGET_FAIL_TAIL=3 bash "$TIER" failing failing 50 4000 \
    -- bash "$SUITE" 3 0 7 2>&1)"; rc=$?
[ "$rc" -eq 7 ] && grep -q 'last 3 lines' <<<"$out" \
    && ok "OUTPUT_BUDGET_FAIL_TAIL brings the tail back" \
    || fail "OUTPUT_BUDGET_FAIL_TAIL=3 gave status $rc:"$'\n'"$out"

# --- 6. verbose streams, and does not lift the budget ---------------------
out="$(cd "$SANDBOX/repo" && OUTPUT_BUDGET_VERBOSE=1 bash "$TIER" loud loud 5 100000 \
    -- bash "$SUITE" 40 12 0 2>&1)"; rc=$?
grep -q 'running fixture 40' <<<"$out" \
    && ok "--verbose streams the run" \
    || fail "OUTPUT_BUDGET_VERBOSE=1 printed nothing of the run:"$'\n'"$out"
[ "$rc" -eq 65 ] \
    && ok "--verbose does not lift the budget (still 65)" \
    || fail "verbose over budget gave status $rc, not 65"

# --- usage -----------------------------------------------------------------
run bash "$TIER" only three args
[ "$rc" -eq 2 ] && ok "too few arguments is a usage error (2)" \
    || fail "three arguments gave status $rc"

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails check(s) failed in $(basename "${BASH_SOURCE[0]}")" >&2
    exit 1
fi
echo "PASS  rust-fs-core's tier.sh keeps its contract here (the floor is rust-fs-core's, tested there)"
