#!/usr/bin/env bash
# scripts/tier.sh finds rust-fs-core's output-budget.sh, or refuses by name.
#
# THE RESOLVER IS THE PART THAT CAN BE WRONG WHILE EVERYTHING LOOKS RIGHT.
# `scripts/output-budget.sh` is rust-fs-core's and is deliberately not
# committed here, so every tier depends on finding somebody else's file at
# run time. The three ways that goes wrong -- absent, present but not core's,
# and present but broken -- all end in a message, and a message nobody ever
# provokes has never been shown to appear.
#
# So each refusal is driven here, with FS_CORE_ROOT pointing at a sandbox
# built for it. FS_CORE_ROOT exists for this: it names core outright and
# turns off the fallbacks, so a test cannot accidentally be answered by the
# real sibling sitting next to the checkout.
#
# NOTHING HERE NEEDS CARGO, A SIBLING, OR A NETWORK, which is why it can live
# in the `fmt` job's glob with the other shell guards: that job clones no
# sibling and the test would otherwise be a skip wearing a different hat.
#
#   bash tests/scripts/test-tier-resolver.sh
#
# THE STRING TESTS ARE `[[ ]]`, NOT `printf | grep -q`. With `pipefail` set,
# `grep -q` exits the moment it matches and printf takes a SIGPIPE, so the
# pipeline's status is 141 rather than 0 -- WHEN IT LOSES THE RACE. Measured
# here: the same check passed on one run and failed on the next with the
# message it was looking for printed in the failure. A flaky guard is worse
# than none, because it teaches its reader to re-run it.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TIER="$REPO/scripts/tier.sh"
fails=0

ok()   { echo "PASS  $*"; }
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

mkdir -p "$REPO/tmp"
SANDBOX="$(mktemp -d "$REPO/tmp/tier-resolver.XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT HUP INT TERM

# A stand-in for core's wrapper: it answers the API version and runs the
# command after `--`, which is all tier.sh depends on. It is NOT a copy of
# the budget logic -- that is core's to test -- so this file cannot drift
# into a second implementation of it.
make_core() {
    local root="$1" version="$2"
    mkdir -p "$root/scripts"
    cat > "$root/scripts/output-budget.sh" <<STUB
#!/usr/bin/env bash
set -uo pipefail
if [ "\${1:-}" = "--version" ]; then echo "$version"; exit 0; fi
log=""
while [ \$# -gt 0 ]; do
    case "\$1" in
        --log) log="\$2"; shift 2 ;;
        --max-lines|--max-bytes|--label) shift 2 ;;
        --) shift; break ;;
        *) shift ;;
    esac
done
mkdir -p "\$(dirname "\$log")"
"\$@" > "\$log" 2>&1
status=\$?
echo "stub: ran with status \$status, log \$log"
exit \$status
STUB
}

# --- 1. FS_CORE_ROOT names a directory with no wrapper in it. --------------
empty="$SANDBOX/empty"
mkdir -p "$empty"
out="$(FS_CORE_ROOT="$empty" bash "$TIER" label log 10 100 -- true 2>&1)"
status=$?
if [ "$status" -eq 0 ]; then
    fail "an FS_CORE_ROOT with no scripts/output-budget.sh was accepted"
elif [[ "$out" != *"has no"* ]]; then
    fail "the refusal does not say the wrapper is absent: $out"
else
    ok "an FS_CORE_ROOT with no wrapper is refused by name"
fi

# --- 2. A wrapper that is not core's is FATAL, not a reason to look on. ----
#
# This is the case worth driving hardest. Falling through to the next
# candidate would report "core is broken" as "core is missing", which is the
# quieter and more confusing failure -- and on a developer's machine the next
# candidate is the real sibling, so the wrong copy would be masked entirely.
wrong="$SANDBOX/wrong"
make_core "$wrong" "some-other-script 9"
out="$(FS_CORE_ROOT="$wrong" bash "$TIER" label log 10 100 -- true 2>&1)"
status=$?
if [ "$status" -eq 0 ]; then
    fail "a wrapper answering the wrong --version was accepted"
elif [[ "$out" != *"--version"* ]]; then
    fail "the refusal does not name the --version contract: $out"
else
    ok "a wrapper that is not core's is refused rather than fallen past"
fi

# --- 3. The contract is the API version, not a digest. --------------------
#
# A comment added in core must not break this repository, which is why no
# SHA-256 is pinned. Same script, extra bytes, still accepted.
digest="$SANDBOX/digest"
make_core "$digest" "rust-fs-core-output-budget 1"
printf '\n# a comment core added after this repository pinned anything\n' \
    >> "$digest/scripts/output-budget.sh"
if FS_CORE_ROOT="$digest" bash "$TIER" label log 10 100 -- true >/dev/null 2>&1; then
    ok "a wrapper that gained a comment is still accepted"
else
    fail "a changed-but-correct wrapper was refused, so something pins its bytes"
fi

# --- 4. A tier exits with the command's own status. -----------------------
#
# The whole point of the redirect-and-capture shape is that the suite's
# status survives it. A wrapper that swallowed it would turn a red suite
# into a green job.
good="$SANDBOX/good"
make_core "$good" "rust-fs-core-output-budget 1"
FS_CORE_ROOT="$good" bash "$TIER" label log 10 100 -- sh -c 'exit 3' >/dev/null 2>&1
status=$?
if [ "$status" -eq 3 ]; then
    ok "a failing command's status is the tier's status"
else
    fail "a command exiting 3 gave the tier status $status"
fi

# --- 5. The command actually runs, and its output goes to the log. --------
FS_CORE_ROOT="$good" bash "$TIER" label resolver-probe 10 100 \
    -- sh -c 'echo the-command-ran' >/dev/null 2>&1
if grep -q "the-command-ran" "$REPO/tmp/logs/resolver-probe.log" 2>/dev/null; then
    ok "the tier's command runs and its output lands in tmp/logs/"
else
    fail "tmp/logs/resolver-probe.log does not hold the command's output"
fi
rm -f "$REPO/tmp/logs/resolver-probe.log" "$REPO/tmp/logs/log.log"

# --- 6. Too few arguments is a usage error, not a run. --------------------
FS_CORE_ROOT="$good" bash "$TIER" label log 10 >/dev/null 2>&1
status=$?
if [ "$status" -eq 2 ]; then
    ok "an incomplete invocation is a usage error"
else
    fail "tier.sh label log 10 gave status $status, expected 2"
fi

# --- 7. The wrapper is not committed here. --------------------------------
#
# The rule the resolver exists to serve: one copy, in core. A committed copy
# is a copy that drifts, and the family had three of them.
if [ -e "$REPO/scripts/output-budget.sh" ]; then
    fail "scripts/output-budget.sh is committed here; it belongs to rust-fs-core"
else
    ok "no copy of output-budget.sh is committed in this repository"
fi

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails check(s) failed" >&2
    exit 1
fi
echo "PASS  tier.sh resolves rust-fs-core's output-budget.sh or refuses by name"
