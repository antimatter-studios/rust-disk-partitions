#!/usr/bin/env bash
# Which CI profile an event runs: `quick` or `full`.
#
#   bash scripts/ci-plan.sh <github.event_name>
#
# A pull request runs the quick profile: the three-OS unit matrix, fmt and
# the script tests, and semver. Everything else runs the full profile, which
# adds the release-profile suite and the oracle job (sgdisk, sfdisk, blkid,
# partx, mdadm, LVM). The merge queue is what decides whether a change lands,
# so it is never quick.
#
# AN UNKNOWN EVENT IS FULL. The list of quick events is the short one, and an
# event this script has never heard of -- a trigger added to ci.yml later, a
# new GitHub event -- gets every tier rather than a guess that it is cheap.
# An empty argument is an error, not an unknown event: it means the caller
# lost the event name, and a plan built on nothing is not a plan.
set -uo pipefail

event="${1-}"
if [ -z "$event" ]; then
    echo "ci-plan: no event name given" >&2
    exit 2
fi

case "$event" in
    pull_request) echo quick ;;
    *) echo full ;;
esac
