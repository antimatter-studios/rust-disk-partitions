#!/usr/bin/env bash
# The quick/full CI profile, as a truth table over events and job results.
#
# scripts/ci-plan.sh turns an event into a profile and scripts/ci-verdict.sh
# turns the profile, the changes job's answer and every job's result into
# ci-ok's verdict. Between them they decide what a pull request may skip, so
# both are held here row by row: every event the workflow triggers on, an
# unknown one, and each way a run can be incomplete -- a missing job, a
# missing or invalid plan, a failure, a cancellation, a skip nobody asked for.
#
#   bash tests/scripts/test-ci-plan.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PLAN="$REPO/scripts/ci-plan.sh"
VERDICT="$REPO/scripts/ci-verdict.sh"

command -v jq >/dev/null || { echo "FAIL  jq is not installed; the fmt job installs it" >&2; exit 1; }

fails=0
rows=0
ok() { rows=$((rows + 1)); }
fail() { rows=$((rows + 1)); fails=$((fails + 1)); echo "FAIL  $*" >&2; }

# --- the plan ---------------------------------------------------------------
plan_is() {
    local event="$1" want="$2" got
    got="$(bash "$PLAN" "$event" 2>/dev/null)"
    if [ "$got" = "$want" ]; then ok; else fail "ci-plan $event: got '$got', want '$want'"; fi
}
plan_is pull_request quick
plan_is merge_group full
plan_is workflow_dispatch full
plan_is workflow_call full
plan_is push full
plan_is schedule full
plan_is some_future_event full
if bash "$PLAN" "" >/dev/null 2>&1; then fail "ci-plan with no event succeeded"; else ok; fi

# --- the verdict ------------------------------------------------------------
# needs <changes> <test> <test-release> <oracle> <fmt> <semver> [<blk-probe>
# <blk-probe-package>]: a toJSON(needs) object with those results; "-" leaves
# the job out. The two blk-probe jobs are gated exactly as the test matrix is,
# so when they are not given they take its result.
needs() {
    local names=(changes test test-release oracle fmt semver blk-probe blk-probe-package) results=("$@") i out= sep=
    [ "$#" -eq 6 ] && results+=("$2" "$2")
    for i in 0 1 2 3 4 5 6 7; do
        [ "${results[$i]}" = - ] && continue
        out="$out$sep\"${names[$i]}\":{\"result\":\"${results[$i]}\",\"outputs\":{}}"
        sep=,
    done
    echo "{$out}"
}
verdict() {
    local want="$1" profile="$2" code="$3" n="$4" label="$5"
    if NEEDS="$n" CODE="$code" PROFILE="$profile" bash "$VERDICT" >/dev/null 2>&1; then
        got=pass
    else
        got=fail
    fi
    if [ "$got" = "$want" ]; then ok; else fail "verdict [$label]: got $got, want $want"; fi
}

S=success
# Full runs: every job must succeed; a heavy-tier skip is a failure.
verdict pass full true "$(needs $S $S $S $S $S $S)" "full, all green"
verdict fail full true "$(needs $S $S skipped $S $S $S)" "full, release skipped"
verdict fail full true "$(needs $S $S $S skipped $S $S)" "full, oracle skipped"
verdict fail full true "$(needs $S $S $S failure $S $S)" "full, oracle failed"
verdict fail full true "$(needs $S $S $S cancelled $S $S)" "full, oracle cancelled"
verdict fail full true "$(needs $S skipped $S $S $S $S)" "full, test matrix skipped"
# Quick runs: the heavy tiers may skip, nothing else may.
verdict pass quick true "$(needs $S $S skipped skipped $S $S)" "quick, heavy skipped"
verdict pass quick true "$(needs $S $S $S $S $S $S)" "quick, heavy ran anyway"
verdict fail quick true "$(needs $S skipped skipped skipped $S $S)" "quick, test matrix skipped"
verdict fail quick true "$(needs $S failure skipped skipped $S $S)" "quick, test matrix failed"
verdict fail quick true "$(needs $S $S failure skipped $S $S)" "quick, release failed"
verdict fail quick true "$(needs $S $S skipped cancelled $S $S)" "quick, oracle cancelled"
verdict fail quick true "$(needs $S $S skipped skipped skipped $S)" "quick, fmt skipped"
verdict fail quick true "$(needs $S $S skipped skipped $S failure)" "quick, semver failed"
verdict fail quick true "$(needs failure $S skipped skipped $S $S)" "quick, changes failed"
# Documentation only: the path-gated jobs may skip under either profile.
verdict pass full false "$(needs $S skipped skipped skipped $S $S)" "full, docs only"
verdict pass quick false "$(needs $S skipped skipped skipped $S $S)" "quick, docs only"
verdict fail full false "$(needs $S skipped skipped skipped skipped $S)" "full, docs only, fmt skipped"
# Missing jobs and missing or invalid plans.
# The blk.probe tool's jobs: gated like the test matrix, skipped only for a
# documentation-only change, and never on a quick plan.
verdict fail quick true "$(needs $S $S skipped skipped $S $S skipped $S)" "quick, blk-probe skipped"
verdict fail full true "$(needs $S $S $S $S $S $S $S skipped)" "full, blk-probe package skipped"
verdict fail quick true "$(needs $S $S skipped skipped $S $S failure $S)" "quick, blk-probe failed"
verdict fail full true "$(needs $S $S $S $S $S $S $S cancelled)" "full, blk-probe package cancelled"
verdict pass quick false "$(needs $S skipped skipped skipped $S $S skipped skipped)" "quick, docs only, blk-probe skipped"
verdict fail quick true "$(needs $S $S skipped skipped $S $S - $S)" "quick, blk-probe missing"
verdict fail full true "$(needs $S $S $S $S $S $S $S -)" "full, blk-probe package missing"

verdict fail full true "$(needs $S $S $S - $S $S)" "full, oracle missing"
verdict fail quick true "$(needs $S $S - skipped $S $S)" "quick, release missing"
verdict fail "" true "$(needs $S $S skipped skipped $S $S)" "no profile"
verdict fail medium true "$(needs $S $S skipped skipped $S $S)" "invalid profile"
verdict fail quick "" "$(needs $S $S skipped skipped $S $S)" "no code answer"
verdict fail quick maybe "$(needs $S $S skipped skipped $S $S)" "invalid code answer"
verdict fail full true "" "no needs at all"
verdict fail full true "not json" "needs not JSON"

# A floor, so a table that stopped being read cannot pass by checking nothing.
if [ "$rows" -lt 41 ]; then
    fail "only $rows truth-table rows ran, floor is 41"
fi

if [ "$fails" -gt 0 ]; then
    echo "test-ci-plan.sh: $fails of $rows rows failed" >&2
    exit 1
fi
echo "test-ci-plan.sh: all checks passed ($rows rows)"
