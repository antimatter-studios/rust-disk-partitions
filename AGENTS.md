# Working in rust-disk-partitions (agent guide)

Pure-Rust partition-table probe, filesystem-magic sniffer and table writer over
any random-access block source, exposing a C ABI, validated against `sgdisk`,
`sfdisk`, `blkid` and `partx` as the oracle. This file is the fast path for an
agent picking up work here, so the workflow does not have to be re-derived each
time. It points at the existing docs rather than duplicating them:

- **README** → `## What it does`, `## Status` (the read side, the write side,
  and what is deliberately not done yet), `## Layout`.
- **`tests/oracle_tools.rs`** → the module docs argue the oracle's design:
  both directions, no loop device, no root, and why a missing tool panics.
- **`.github-guard`** → why one required check and not six, argued at length.

The section between the BEGIN/END markers below is **shared, byte-identical,
with every repository in this family**. Do not edit it here: change the
canonical copy and propagate it, or `chore lint` will fail. Everything after
the END marker is specific to this repository.

<!-- BEGIN SHARED BLOCK: agent-core v7 sha256:1d6c04c827b22c2836e4b9d3977fda083f1ec322d8408a6b2ae5bf72de42b3e5 -->
## Claiming work

Several agents work these repositories at the same time. Before you start on
an issue, claim it, so nobody else spends a session on what you are already
doing. The lock is a **GitHub label**, because labels are shared state that
every agent can read and change without posting comments into the thread.

**Before starting.** Check, claim, then read back:

```sh
gh issue view <N> --json labels                      # holds `claimed`? pick another
gh issue edit <N> --add-label claimed --add-label claim/<session>
gh issue view <N> --json labels                      # read back and confirm
```

`<session>` is your session name — `agent-<random4>-<isodate>`, e.g.
`agent-3f7c-2026-09-22`. Create the `claim/<session>` label if it does not
exist.

**Resolving a race.** Adding a label is not compare-and-swap: two agents can
both add `claimed` and both believe they won. That is what the read-back is
for. If it shows more than one `claim/*` label, the **lexically lowest**
session keeps the issue; every other agent removes its own `claim/*` label and
picks different work. Each racer computes the same answer independently, so no
further coordination is needed.

**When you finish or stop.** Remove both labels — on merge, or the moment you
abandon the work:

```sh
gh issue edit <N> --remove-label claimed --remove-label claim/<session>
```

Delete your `claim/<session>` label from the repository at the end of your
session so they do not accumulate.

**Reclaiming a stale claim.** An agent that dies holding a claim would block an
issue forever. If `claimed` was applied more than 12 hours ago and the holder's
branch has no commits since, any agent may take it: remove the stale `claim/*`,
add your own, and say so in the issue.

**This is a convention, not a fence.** Nothing enforces it. An agent that
ignores it duplicates work; it cannot corrupt anything. Honour it anyway.

## Findings become issues before a tracking issue closes

A review, an investigation, or the work under an umbrella issue turns up
things beyond what it set out to do. File each finding as its own issue --
what was seen, where, and what would show it fixed -- and cite it in the
tracking issue **before** that issue closes. A finding left in a comment, a
pull request description or a transcript is lost when the issue that held
it closes; an open issue is where the next agent looks.

## Work in a worktree

Every working copy is a **git worktree** of an existing checkout, made with
`git worktree add`. Never `git clone` a second, unlinked copy — not for a
branch, a PR, a review, or a sibling you need at another ref:

```sh
git -C <checkout> fetch origin
git -C <checkout> worktree add <path> -b <type>/<name> origin/main   # new work
git -C <checkout> worktree add --detach <path> <tag>                 # a sibling at a pinned ref
git -C <checkout> worktree remove <path>                             # when done
```

A worktree shares the checkout's objects and remotes, and `git worktree list`
shows it to every agent on the machine, so nobody else mistakes it for
abandoned work or loses track of it. An unlinked clone copies all the history
again, is invisible to that list, and gets left behind in `/tmp` long after the
work that made it is merged. Remove your worktree when you finish.

## Skills to use

- **`dev-loop`** — the required loop for any non-trivial change: baseline the
  full suite → change → re-run (no baseline test may regress) → enhance tests →
  vet. Always run it.
- **`commit`** / **`pr`** — for grouping commits and opening pull requests.

Each repository names any further skills of its own below.

## A bug fix starts with a red

**Prove it is broken first** — a failing check or test — *then* fix it, *then*
prove that same check is green, *then* confirm the full baseline still passes.
Never write the fix before you have a red. A fix with no failing test to its
name is a claim, not a result.

**Red and green happen in one pull request, on one branch.** Push the commit
that adds the failing test on its own, and let CI show it red on that pull
request. Then push the fix to the **same branch**, with the test untouched,
until the same pull request is green.

- Never put a fix in a second pull request, stacked or not. A pull request
  that only holds a red commit can never merge, and it blocks every pull
  request built on it.
- Never push the test and the fix together. The red has to be visible in CI,
  not claimed in the description.
- Wait for the red run to finish before pushing the fix: a new push cancels
  the run in progress.

## Nothing skips

A test that cannot run **fails**, naming the task that would provide what it
needed. Never add an early return for a missing fixture, tool or VM: a skipped
test reads exactly like a passing one, and a suite that quietly declines to run
is indistinguishable from a suite that passes.

Where a tier reports skips or ignored tests, that is a gate, not a note.

## Validate against something that is not us

A driver's own readers share its interpretation of the format, so they cannot
catch a misreading: the mistake is baked into the fixture *and* the parser, and
they agree with each other while disagreeing with every real filesystem. Unit
tests over self-built fixtures prove self-consistency, not correctness.

Every structure that is parsed or written gets a cross-validation test against
an **independent oracle** — the platform's own tools, a real kernel, or a third
implementation — before it is considered done. Each repository names its
oracles below.

## Output is budgeted

Test tiers run through `scripts/tier.sh`, which runs the suite **quietly**: the
whole run goes to `tmp/logs/<tier>.log`, a pass prints one verdict line naming
that log, and a failure prints the verdict, the command's status and the log's
path — `--tail N`, or `OUTPUT_BUDGET_FAIL_TAIL=N`, prints the tail for whoever
is watching. **Read the log**: a failing tier names it and does not recite it.
CI keeps the logs as an artifact, so the detail is always retrievable.

The budget caps the log, not merely what is shown, and every number in the
table was measured. A run that passes but prints more than its budget **fails**.

The reader who pays most for a noisy suite is an agent that re-reads its whole
transcript on every step, and so pays for one loud run many times over. If a
tier legitimately grows, raise its row **with the measurement that justifies
it**. Do not silence output to fit, and do not route around `tier.sh`.

## Commits and branches

- Branches are `<type>/<name>`, matching the commit type: `fix/`, `feat/`,
  `ci/`, `docs/`, `chore/`, `test/`.
- A commit is a subject plus flat one-sentence bullets. Subjects are
  declarative, not imperative: "the run-end bound is checked", not "check the
  run-end bound".
- **No AI attribution and no co-author trailers**, in commits or in pull
  request descriptions.
- `main` takes **squash merges only**.
- **Bring a branch up to date before every push.** `git fetch origin`, and
  if its base has moved -- the layer below it in the stack, or `main` at the
  bottom -- rebase onto the base first, resolving any conflicts then, while
  they are small. Synced first, the run
  tests the branch close to how it will land. And a branch synced at every
  push never goes stale: each sync takes in only what landed since the last
  one, so conflicts stay few and small. Sync when you are pushing anyway; a
  push made only to bring a branch up to date buys a whole CI run and
  nothing else.
- **`main` merges through a merge queue.** A stack whose pull requests'
  own CI is green goes into the queue: `gh stack merge <stack> --yes` adds
  every layer up to the one named. Never queue a stacked pull request with
  the REST `merge-async` call: it answers "enqueued" and queues nothing. The
  queue tests each group on a temporary branch, `main` plus the queued pull
  requests in order, and squash-merges each one only when that run is green,
  so what lands is exactly the tree CI ran. That run is what "branches must be up to
  date" used to buy, at one run per group instead of one per pull request per
  merge: never update a branch only so that it can merge. Every workflow
  that gates `main` triggers on `merge_group`, or the queue never sees its
  result and nothing merges.

## One stack per repository

Every repository lands its pull requests as one stack, whether or not they
touch the same files. Conflicts are then paid for once, when a pull request
joins the stack, instead of again in the merge queue after every merge.

- **A new pull request goes on the top.** Ask
  `../rust-fs-core/scripts/stack-base.sh` (run in place; in rust-fs-core
  itself, `scripts/stack-base.sh`): it prints the head branch of the top of
  the stack, or `main` when nothing is open. Rebase onto that branch, resolve
  every conflict locally while you do, and build and lint the result before
  you push. A merge that is clean on your machine is clean in CI, so a
  conflict that reaches the queue means the base was stale or the technique
  was wrong. Then link the pull request into the stack: `gh stack link
  <stack> <pull request>`.
- **One squashed commit per layer.** Squash the branch to one commit on the
  layer below before it joins, so the layers above replay without resolving
  anything again.
- **The queue still tests every layer.** It runs the full pipeline on `main`
  plus each layer and the layers below it, and squash-merges each layer as
  its own commit, so nothing lands untested and `main` stays bisectable.
- **A red layer stops the stack.** Fix it in place, on its own branch, then
  replay every layer above it with `git rebase --update-refs` and push them
  together. Do not move a red layer to the top: the layers above were
  resolved against it. Take a layer out only when its fix cannot come soon,
  and say so on its pull request.
- **Never delete a branch a pull request is based on.** Deleting it closes
  every pull request above it, and none can be reopened while it is gone.

## Changelog entries are fragments

A pull request never edits `CHANGELOG.md`. It adds one file under
`changelog.d/`, named for its branch (`fix/run-end-bound` writes
`changelog.d/fix-run-end-bound.md`), holding one or more `### Added`,
`### Changed`, `### Fixed` (or any other) headings, each over `- ` entries.
Pull requests open at the same time then never conflict over the top of one
file. A change no consumer would notice -- tests, CI, documentation -- adds
no fragment.

A release gathers them: `../rust-fs-core/scripts/changelog-gather.sh
VERSION` writes every fragment into a dated `## [VERSION]` section of
`CHANGELOG.md` and deletes the fragments, in the release commit. An entry is
a fragment until it ships and a line of `CHANGELOG.md` after, never both.
`chore lint` runs `changelog-check.sh`: every fragment is well formed, and
`CHANGELOG.md` holds no unreleased entries of its own.

## Project rules

- **No GPL/LGPL/AGPL dependencies.** Permissive only (MIT/BSD/Apache).
  Shelling out to a copyleft CLI as a *test oracle* is fine — linking or
  copying it is not.
- **Each of these is a standalone project.** Never mention a consuming
  application in the README, the source, or CLI help.
<!-- END SHARED BLOCK: agent-core v7 -->
## What this is

Partition-table parsing — MBR and GPT — over `rust-fs-core`'s device traits,
exposed through a C ABI (`tests/c_abi.rs`) and linked into the app as a
staticlib.

**Two crates, one workspace** (#150). The library is the root package and the
workspace's only default member, so a bare `cargo build`/`test`/`publish`
means it, exactly as before. `crates/blk-probe` is the `blk.probe`
command-line tool (formerly the rust-blk-probe repository): it opens a disk
image through the four `rust-img-*` readers and prints what this library
finds as JSON. Reach it with `-p rust-blk-probe` and the `blk-probe:*` chores.
It is released on its own `blk-probe-v<version>` tags by
`release-blk-probe.yml`, with its own `CHANGELOG.md` and `changelog.d/`; the
library keeps `v<version>` and `release.yml`. A change to the sniff is tested
through the tool in the same pull request: its `tests/oracle_tools.rs` holds
the XFS, Btrfs and EROFS answers to `blkid -p` over images each format's own
mkfs wrote.

## Running tests

```sh
chore test          # the suite, in debug
chore test:release  # the same suite where arithmetic wraps instead of panicking
chore test:oracle   # against sgdisk, sfdisk, blkid and partx
chore build
chore lint          # fmt, the agent-core check, clippy
chore staticlib     # what the app links
chore blk-probe:test          # the blk.probe tool's suite (Linux: with its blkid oracle)
chore blk-probe:test:release
chore blk-probe:binary        # the universal blk.probe binary
```

CI runs `test`, `test-release`, `oracle`, `fmt`, `semver`, `blk-probe` and
`blk-probe-package`, and `ci-ok` aggregates them.

Both profiles are run on purpose: arithmetic that panics in debug can wrap
silently in release. `tests/ci_profile.rs` holds the debug run to being a debug
run — and it reads *through* the tier wrapper to find it, so wrapping a run
cannot hide it.

**Every tier goes through rust-fs-core's `scripts/tier.sh`, run in place**
from the `../rust-fs-core` checkout at the pinned version, which is this
repository's half of the shared block's "Output is budgeted". It keeps no copy
of the runner, of `scripts/output-budget.sh`, or of any family script, and
rust-fs-core's `family-check` (run in CI) refuses one. The budgets and the
floors are in `chores.yml`, measured, beside the command each one bounds.

A pass prints one verdict line naming `tmp/logs/<tier>.log`; **read the log**
rather than expecting the tier to recite it. `chore test -- --verbose`
(`OUTPUT_BUDGET_VERBOSE=1`) streams the run live and does *not* lift the
budget. A failing tier is quiet too, from core v0.2.13: `--tail N` brings the
tail back. CI uploads every tier log as an artefact with `if: always()`.

## The oracle is sgdisk and sfdisk

`oracle (external tools)` builds tables with **`sgdisk`, `sfdisk`, `blkid` and `partx`** and reads
them back through this crate. That is the cross-validation the shared block
requires: our own writer and our own reader share an interpretation, so only a
third implementation can catch a misreading. A table this crate produced must
also be one those tools accept.

If a tool is missing, the job **fails** — it does not skip. Install it.

## Four workflows, one gate

`ci.yml`, `release.yml` and `release-blk-probe.yml` (tag-driven: `v*` for the
library, `blk-probe-v*` for the tool) and `fuzz.yml` (nightly cron plus
dispatch). **Only `ci.yml` runs on `pull_request`**, so only its jobs may gate:
a required check that never reports is a permanent block rather than a gate.

One required check, `ci-ok`, declared in `.github-guard`. Judging mergeability
from check **conclusions** is unreliable: an in-progress `CheckRun` reports its
conclusion as an empty string, and a `StatusContext` has no conclusion field at
all. Read `mergeStateStatus` and `statusCheckRollup.state`.

`chore check:ci-gate` holds both halves of that mechanically — every job in
`ci.yml` must appear in `ci-ok`'s `needs:`, and `.github-guard` must require
`ci-ok` and nothing else. The task runs `../rust-fs-core/scripts/ci-gate.sh` and nothing else,
so the script is what can be tested, reviewed and run without `chore` at all.
It replaced `tests/ci_aggregate_gate.rs`: that parsed a YAML file and compared
strings, exercising nothing this crate ships, and as a `cargo test` it counted
towards the executed-test floor the gate itself enforces.

## Never grow a shared tool to solve a problem here

**Never grow a shared tool to solve a problem in this repository.** `chore` is
a general-purpose task runner this project merely consumes; the same goes for
`github-guard` and the agent-skills hooks. If something needed here looks like
it belongs inside one of them, it does not. Solve it here, or ask first. The
tell is a release: if a shared tool needs a new version cut whose only purpose
is to unblock this project, the code is in the wrong repository.
