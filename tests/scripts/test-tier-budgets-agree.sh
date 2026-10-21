#!/usr/bin/env bash
# chores.yml and ci.yml run the same tiers, with the same budgets and floors.
#
# THE DUPLICATION IS DELIBERATE. A tier is declared twice -- once in
# chores.yml for the person at the terminal, once in .github/workflows/ci.yml
# for the gate -- because the workflow cannot read chores.yml without
# installing chore on three runner platforms, which this repository does not
# do (the fmt job runs its guards as scripts for the same reason). What is not
# acceptable is the two drifting: a budget raised in chores.yml and not in
# ci.yml is a budget the gate does not enforce, and a floor raised only in CI
# is a floor a developer never meets. So the numbers are repeated and CHECKED,
# not repeated and trusted.
#
# It came with the blk.probe tool (#150) and covers every tier in the
# repository, the library's as well as the tool's.
#
# It also refuses a tier with a budget and no floor, `semver` alone excepted. Those two answer
# different questions -- "did it print too much" and "did it run anything at
# all" -- and a tier with only the first can go green having executed nothing.
#
#   bash tests/scripts/test-tier-budgets-agree.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok()   { printf 'ok    %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1" >&2; fails=$(( fails + 1 )); }

# The comparison itself, over any pair of files, so the self-test below can
# drive it with trees that are known to disagree.
compare() {
    python3 - "$1" "$2" <<'PY'
import re, sys

TIER = re.compile(
    r'tier\.sh\s+(?P<label>"[^"]*"|\'[^\']*\'|\S+)'
    r'\s+(?P<log>\S+)\s+(?P<lines>\d+)\s+(?P<bytes>\d+)\s+--')
FLOOR = re.compile(r'test-floor\.sh\s+(?:--refuse-ignored\s+)?(?P<log>\S+)\s+(?P<floor>\d+)')

def read(path):
    text = open(path, encoding="utf-8").read()
    tiers, floors = {}, {}
    for m in TIER.finditer(text):
        label = m.group("label").strip('"\'')
        tiers[m.group("log")] = (label, m.group("lines"), m.group("bytes"))
    # A TIER MAY DECLARE MORE THAN ONE FLOOR, and they are compared as a set.
    # The oracle tests are util-linux's and are compiled only on Linux, so the
    # two platforms run different numbers of tests and a single number would
    # have to be the smaller one -- which is a floor the Linux legs cannot
    # breach. The `uname -s` conditional carrying both numbers is the honest
    # spelling, and this is what keeps the two files' copies of it identical.
    for m in FLOOR.finditer(text):
        floors.setdefault(m.group("log"), set()).add(m.group("floor"))
    return tiers, floors

chores, ci = sys.argv[1], sys.argv[2]
(ct, cf), (wt, wf) = read(chores), read(ci)
problems = []

if not ct:
    problems.append(f"{chores} declares no tier at all")
if not wt:
    problems.append(f"{ci} declares no tier at all")

for log in sorted(set(ct) | set(wt)):
    if log not in ct:
        problems.append(f"tier '{log}' runs in {ci} and not in {chores}")
        continue
    if log not in wt:
        problems.append(f"tier '{log}' runs in {chores} and not in {ci}")
        continue
    if ct[log] != wt[log]:
        problems.append(
            f"tier '{log}' disagrees: {chores} has {ct[log]}, {ci} has {wt[log]}")

# THE ONE TIER WITH NO FLOOR, NAMED. `semver` is a single check, not a suite:
# it prints no `test result:` line for a floor to count, and chores.yml says
# so beside it. Anything else without a floor is refused.
NO_FLOOR = {"semver"}

for name, tiers, floors in ((chores, ct, cf), (ci, wt, wf)):
    for log in sorted(tiers):
        if log not in floors and log not in NO_FLOOR:
            problems.append(f"tier '{log}' has a budget and no floor in {name}")
for log in sorted(set(cf) & set(wf)):
    if cf[log] != wf[log]:
        problems.append(
            f"floor for '{log}' disagrees: {chores} has {sorted(cf[log])}, "
            f"{ci} has {sorted(wf[log])}")

for p in problems:
    print(p)
sys.exit(1 if problems else 0)
PY
}

# --- 1. the comparison sees a disagreement it is shown -------------------
mkdir -p "$REPO/tmp"
SANDBOX="$(mktemp -d "$REPO/tmp/tier-budgets.XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT HUP INT TERM

cat > "$SANDBOX/chores" <<'EOF'
- 'bash ../rust-fs-core/scripts/tier.sh "test (debug)" debug 90 6000 -- cargo test'
- 'bash ../rust-fs-core/scripts/test-floor.sh debug 40'
- 'bash ../rust-fs-core/scripts/tier.sh "test (release)" release 90 6000 -- cargo test --release'
- 'bash ../rust-fs-core/scripts/test-floor.sh release 40'
EOF
cat > "$SANDBOX/ci" <<'EOF'
bash ../rust-fs-core/scripts/tier.sh "test (debug)" debug 900 6000 -- cargo test
bash ../rust-fs-core/scripts/test-floor.sh debug 40
bash ../rust-fs-core/scripts/tier.sh "test (oracle)" oracle 90 6000 -- cargo test --test oracle
EOF
seen="$(compare "$SANDBOX/chores" "$SANDBOX/ci")"
for want in "tier 'debug' disagrees" "tier 'release' runs in" "and no floor"; do
    grep -qF "$want" <<<"$seen" \
        && ok "the comparison reports: $want" \
        || fail "the comparison missed '$want'; it said:"$'\n'"$seen"
done
printf 'bash ../rust-fs-core/scripts/tier.sh "t" t 1 2 -- true\nbash ../rust-fs-core/scripts/test-floor.sh t 3\n' \
    > "$SANDBOX/same"
compare "$SANDBOX/same" "$SANDBOX/same" >/dev/null \
    && ok "two files that agree are accepted" \
    || fail "the comparison rejected a file compared with itself"

# --- 2. this repository's two files agree ---------------------------------
seen="$(compare "$REPO/chores.yml" "$REPO/.github/workflows/ci.yml")"
if [ -n "$seen" ]; then
    fail "chores.yml and ci.yml disagree:"$'\n'"$seen"
else
    ok "chores.yml and .github/workflows/ci.yml declare the same tiers"
fi

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails check(s) failed in $(basename "${BASH_SOURCE[0]}")" >&2
    exit 1
fi
echo "PASS  every tier has one budget and the same floors, in both files"
