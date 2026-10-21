#!/usr/bin/env bash
# ci-ok's verdict: did every job the plan asked for run and succeed?
#
#   NEEDS='<toJSON(needs)>' CODE=<true|false> PROFILE=<quick|full> \
#       bash scripts/ci-verdict.sh
#
# CODE is the `changes` job's answer to "is this more than documentation?";
# PROFILE is scripts/ci-plan.sh's answer for the event. A skip is accepted
# only where one of them asked for it:
#
#   changes, fmt, semver    always success
#   test (the OS matrix)    success, or skipped when CODE is false
#   test-release, oracle    success, or skipped when CODE is false or
#                           PROFILE is quick
#
# Everything else fails: a failure or cancellation anywhere, a skip nobody
# asked for, a job missing from NEEDS, and a CODE or PROFILE that is empty
# or not one of its two values. A plan that failed to compute is not a
# quick plan.
set -uo pipefail

REQUIRED=(changes test test-release oracle fmt semver blk-probe blk-probe-package)

bad=()
case "${PROFILE-}" in
    quick | full) ;;
    *) bad+=("plan: profile '${PROFILE-}' is not quick or full") ;;
esac
case "${CODE-}" in
    true | false) ;;
    *) bad+=("plan: code '${CODE-}' is not true or false") ;;
esac

if ! results="$(printf '%s' "${NEEDS-}" | jq -er 'to_entries[] | "\(.key)=\(.value.result)"' 2>/dev/null)"; then
    bad+=("needs: not a JSON object of job results")
    results=
fi

for job in "${REQUIRED[@]}"; do
    result="$(printf '%s\n' "$results" | sed -n "s/^$job=//p")"
    echo "$job: ${result:-missing}"
    case "$result" in
        success) continue ;;
        "") bad+=("$job: missing from needs"); continue ;;
        skipped)
            case "$job" in
                test | blk-probe | blk-probe-package) [ "${CODE-}" = false ] && continue ;;
                test-release | oracle)
                    { [ "${CODE-}" = false ] || [ "${PROFILE-}" = quick ]; } && continue ;;
            esac ;;
    esac
    bad+=("$job: $result")
done

echo "profile: ${PROFILE-} code changed: ${CODE-}"
if [ "${#bad[@]}" -gt 0 ]; then
    printf '::error::not green: %s\n' "${bad[@]}"
    exit 1
fi
echo "every job the plan asked for succeeded"
