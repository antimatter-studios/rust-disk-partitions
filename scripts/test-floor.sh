#!/usr/bin/env bash
# test-floor.sh TIER FLOOR  — the tier ran at least FLOOR tests
#
# THE FAILURE A BUDGET CANNOT SEE, and the failure this repository keeps
# meeting: an ABSENCE. scripts/tier.sh fails a tier that PRINTS more than it
# is allowed to; nothing fails a tier that printed almost nothing because it
# ran almost nothing. `cargo test` exits 0 when a filter selects nothing, when
# a target list comes out empty, and when no test binary was built at all --
# so a tier whose selection stopped matching is a green line saying `0 passed`.
# Nothing in a pass/fail gate can see that, because there is no failure to
# see. Only a count can.
#
# The number is MEASURED, like the budgets, and it only ever goes up: a floor
# lowered to make a run pass is a floor that has stopped measuring anything.
# Raise one with the run that measured the new count.
#
# It reads the tier's log (tmp/logs/<TIER>.log, written by tier.sh) rather than
# a pipe, so it cannot swallow the suite's own verdict -- a
# `cargo test | test-floor.sh` would report this script's exit status and
# discard the one that matters.
set -euo pipefail

[ $# -eq 2 ] || { echo "usage: test-floor.sh TIER FLOOR" >&2; exit 2; }
TIER="$1"
FLOOR="$2"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG="$REPO/tmp/logs/$TIER.log"

if [ ! -f "$LOG" ]; then
    echo "test-floor.sh: $LOG is missing -- the $TIER tier did not run." >&2
    exit 1
fi

# `test result: ok. 37 passed; 0 failed; ...`, one line per test binary.
# Only the `ok.` lines are summed: a `FAILED` line carries a passed count too,
# and a floor that counted it would report a failing tier as having done its
# work.
ran="$(awk '/^test result: ok\./ { sum += $4 } END { print sum + 0 }' "$LOG")"
if [ "$ran" -lt "$FLOOR" ]; then
    echo "test-floor.sh: the $TIER tier executed $ran tests; the floor is $FLOOR." >&2
    echo "               A tier that runs fewer tests than it used to has stopped" >&2
    echo "               early rather than passed. Read $LOG." >&2
    exit 1
fi
printf '%s\n' "$TIER: $ran tests executed (floor $FLOOR)"
