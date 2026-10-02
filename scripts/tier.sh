#!/usr/bin/env bash
# tier.sh LABEL LOG-NAME MAX-LINES MAX-BYTES -- COMMAND [ARG...]
#
# One test tier, run QUIETLY and under a budget. The whole run goes to
# tmp/logs/<LOG-NAME>.log; a pass prints one verdict line naming that log, and
# a failure prints the verdict, the command's status and the log's path. A run
# that passed but printed more than its budget fails with status 65, which is
# distinguishable from a failing suite.
#
# WHY THE BUDGET IS PART OF THE TASK and not a CI-only check: the reader who
# pays most for a noisy suite is the one running it locally, and a rule that
# only CI enforces is a rule the tree drifts away from between pull requests.
# Before this existed, each green matrix leg replayed 289 `... ok` lines --
# 1,236 of them across the three legs of every green run (#127).
#
# The budgets themselves are in chores.yml, next to the command each one
# bounds, and every one of them was MEASURED -- see the table there. Raise one
# deliberately when a tier grows, with the measurement that justifies it; a
# budget nobody can breach measures nothing, and one lowered to fit is the
# defect it exists to catch.
#
# THE FLOOR IS rust-fs-core's (scripts/core.sh test-floor), not this
# script. A budget fails a tier that PRINTS too much; nothing here fails a
# tier that printed almost nothing because it RAN almost nothing.
#
# VERBOSE. `OUTPUT_BUDGET_VERBOSE=1`, or `--verbose`/`-v` in the chore
# invocation's CLI_ARGS (`chore test -- --verbose`), streams the run as it
# happens as well as logging it. It does NOT lift the budget: the log is the
# same size either way, and a tier that has outgrown its budget should say so
# whether or not anybody was watching.
#
# THE VARIABLE IS `OUTPUT_BUDGET_VERBOSE`. It was `FLTH_VERBOSE` while the
# wrapper lived in the test harness, and a rename like that fails silently --
# `--verbose` simply stops working and nothing errors. Core names the
# replacement on stderr when it sees an `FLTH_*` variable; nothing else does,
# so it is written down here for whoever comes looking for the old name.
#
# A FAILING TIER IS QUIET TOO, from core v0.2.13: it prints
# `<label>: FAILED (exit N) -- <lines> lines in <log>` and no tail. `--tail N`,
# or OUTPUT_BUDGET_FAIL_TAIL=N, brings the tail back for whoever is watching.
# Any assertion about a failing tier has to pass `--tail` explicitly.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# THE WRAPPER IS RUST-FS-CORE'S, COPIED FOR THIS RUN AND DELETED AFTER IT.
#
# `scripts/output-budget.sh` lives in rust-fs-core and nowhere else, and it is
# deliberately NOT committed here. A committed copy is a copy that drifts:
# measured across this family on 2026-09-22 there were three of them, reached
# four different ways, each repository internally consistent and nothing
# comparing them.
#
# THE SIBLING FIRST, THEN WHATEVER CARGO RESOLVED. The sibling comes first so
# a coordinated local change to core's wrapper is exercised here on the next
# run rather than being masked by a registry copy of the last release. A
# checkout with no sibling beside it falls through to cargo, which has already
# resolved am-fs-core and can say where it put it.
#
# FS_CORE_ROOT NAMES CORE OUTRIGHT, and when it is set there is no fallback:
# you have said where core is, so its absence there is an answer rather than a
# reason to go looking. It is also what lets tests/scripts/test-tier-resolver.sh
# drive every refusal below -- a resolver whose failure path nobody executes
# has never been shown to refuse anything, which is the same defect as a test
# that skips.
#
# WHAT IS VERIFIED IS `--version`, NOT A DIGEST. A pinned SHA-256 means every
# comment core adds to the script breaks a consumer until the digest is chased,
# and the same digest in seven repositories is the lockstep this arrangement
# exists to remove. `rust-fs-core-output-budget 1` is the contract.
#
# A WRONG COPY IS FATAL, NOT A REASON TO LOOK ELSEWHERE. Falling through to the
# next candidate would report "core is broken" as "core is missing", which is
# the quieter and more confusing failure.
EXPECTED_API="rust-fs-core-output-budget 1"
CORE_ROOT="${FS_CORE_ROOT:-$REPO/../rust-fs-core}"

verified() {
    [ -f "$1" ] || return 1
    [ "$(bash "$1" --version 2>/dev/null || true)" = "$EXPECTED_API" ]
}

refuse_wrong() {
    echo "tier.sh: $1/scripts/output-budget.sh is there, but does not answer" >&2
    echo "         --version with '$EXPECTED_API'. That is a broken or far too" >&2
    echo "         old rust-fs-core, not an absent one, so this stops here" >&2
    echo "         rather than quietly looking somewhere else. v0.2.11 is the" >&2
    echo "         first release that ships the script, v0.2.13 the first whose" >&2
    echo "         failing tier is quiet." >&2
    exit 1
}

CORE_SCRIPT=""
if [ -n "${FS_CORE_ROOT:-}" ]; then
    if [ -e "$CORE_ROOT/scripts/output-budget.sh" ]; then
        verified "$CORE_ROOT/scripts/output-budget.sh" || refuse_wrong "$CORE_ROOT"
    else
        echo "tier.sh: FS_CORE_ROOT names $CORE_ROOT, which has no" >&2
        echo "         scripts/output-budget.sh. The wrapper lives in" >&2
        echo "         rust-fs-core and is deliberately not committed here." >&2
        exit 1
    fi
    CORE_SCRIPT="$CORE_ROOT/scripts/output-budget.sh"
elif [ -e "$CORE_ROOT/scripts/output-budget.sh" ]; then
    verified "$CORE_ROOT/scripts/output-budget.sh" || refuse_wrong "$CORE_ROOT"
    CORE_SCRIPT="$CORE_ROOT/scripts/output-budget.sh"
else
    CORE_DIR="$(cargo metadata --format-version 1 --locked --manifest-path "$REPO/Cargo.toml" \
        2>/dev/null | python3 -c '
import json, sys
packages = json.load(sys.stdin)["packages"]
print(next((p["manifest_path"].rsplit("/", 1)[0]
            for p in packages if p["name"] == "am-fs-core"), ""))
' 2>/dev/null)"
    if [ -n "$CORE_DIR" ] && [ -e "$CORE_DIR/scripts/output-budget.sh" ]; then
        verified "$CORE_DIR/scripts/output-budget.sh" || refuse_wrong "$CORE_DIR"
        CORE_SCRIPT="$CORE_DIR/scripts/output-budget.sh"
    fi
fi

if [ -z "$CORE_SCRIPT" ]; then
    echo "tier.sh: no rust-fs-core supplied scripts/output-budget.sh." >&2
    echo "         The wrapper lives in rust-fs-core and is deliberately not" >&2
    echo "         committed here. Looked for the sibling at" >&2
    echo "           $CORE_ROOT/scripts/output-budget.sh" >&2
    echo "         then asked cargo for the am-fs-core package." >&2
    echo "         Clone rust-fs-core beside this checkout, or depend on" >&2
    echo "         am-fs-core v0.2.13 or later." >&2
    exit 1
fi

# COPIED FOR THIS RUN AND DELETED AFTER IT, so a `git pull` in the sibling
# part-way through a long tier cannot change the script underneath it. tmp/ is
# gitignored and is where the tier logs already live.
BUDGET="$REPO/tmp/output-budget.$$.sh"
mkdir -p "$REPO/tmp"
cp "$CORE_SCRIPT" "$BUDGET"
trap 'rm -f "$BUDGET"' EXIT

[ $# -ge 5 ] || { echo "tier.sh: usage: tier.sh LABEL LOG MAX-LINES MAX-BYTES -- CMD..." >&2; exit 2; }
LABEL="$1"; LOG_NAME="$2"; MAX_LINES="$3"; MAX_BYTES="$4"; shift 4
[ "${1:-}" = "--" ] && shift
[ $# -gt 0 ] || { echo "tier.sh: no command" >&2; exit 2; }

# `chore test -- --verbose` arrives as CLI_ARGS. output-budget.sh reads
# OUTPUT_BUDGET_VERBOSE itself, so mapping the flag onto it is all that is
# needed -- and it means the environment variable and the flag cannot disagree.
case " ${CLI_ARGS:-} " in
    *" --verbose "*|*" -v "*) export OUTPUT_BUDGET_VERBOSE=1 ;;
esac

bash "$BUDGET" \
    --log "$REPO/tmp/logs/$LOG_NAME.log" \
    --max-lines "$MAX_LINES" \
    --max-bytes "$MAX_BYTES" \
    --label "$LABEL" \
    -- "$@"
