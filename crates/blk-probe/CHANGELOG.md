# Changelog

Notable changes to `rust-blk-probe`, newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

Changes not yet released are fragments in [`changelog.d/`](changelog.d/), one per
pull request; a release gathers them into a section here.

The tool was the `rust-blk-probe` repository through v0.1.2, and the links
below to those releases point there. Since #150 it is `crates/blk-probe` in
the rust-disk-partitions workspace, released on `blk-probe-v<version>` tags.

## [Unreleased]

## [0.1.2] — 2026-10-03

A tool release: the library's API is unchanged. Nothing is published to
crates.io; the release is the tarballs.

v0.1.1 was versioned in `Cargo.toml` but never tagged, so no release carries
its changes. v0.1.2 is the first release to attach the linux-x86_64 tarball
beside the darwin-arm64 one (#52), and it ships everything listed under v0.1.1
below.

### Changed

- **`am-fs-core` 0.2.18** (#48). The CI gate runs from `rust-fs-core`'s
  `scripts/core.sh ci-gate` rather than a copy kept here, and every
  `rust-fs-core` ref moves to 0.2.18. The probe's behaviour is unchanged.

## [0.1.1] — 2026-10-03

A tool release: the library's API is unchanged. Nothing is published to
crates.io; the release is the tarballs.

### Added

- **`blk.probe --version`** (#47). It prints `blk.probe (rust-blk-probe)
  <version>`, the family's identifying line, written by
  `fs_core::cli::version` rather than a copy of its format, so a package
  formula's test and an installer can tell this program and its version from
  anything else on PATH. It was an unknown flag, exit 1.
  `scripts/check-package.sh` now refuses a tarball whose binary does not
  report the version in the tarball's name.

- **A linux-x86_64 release tarball** (#45). v0.1.0 shipped
  `rust-blk-probe-0.1.0-darwin-arm64.tar.gz` alone, so `blk.probe` could not be
  installed on Linux. `release.yml` now packages
  `rust-blk-probe-<version>-linux-x86_64.tar.gz` beside it, in the same
  `bin/blk.probe` + `LICENSE` layout, on a native Ubuntu runner; one job
  attests both and attaches them with their `.sha256` files, and `ci.yml`'s
  `package` legs build both on every pull request.
  `tests/scripts/test-release-platforms.sh` fails a workflow that drops either.

## [0.1.0] — 2026-09-29

The first release: `rust-blk-probe-0.1.0-darwin-arm64.tar.gz` on the GitHub
release, with build provenance. Nothing is published to crates.io.

### Added

- **A release tarball, built on every pull request and published with
  build provenance on a tag** (#36). `scripts/package.sh` writes
  `rust-blk-probe-<version>-darwin-arm64.tar.gz` holding `bin/blk.probe` and
  `LICENSE`, and `scripts/check-package.sh` refuses one with any other layout,
  a checksum that does not match, a binary for another platform, or a probe
  that does not run. `ci.yml`'s `package` job runs both on every pull request;
  `release.yml` runs them on a `v*.*.*` tag after the whole of `ci.yml`, then
  attests the tarball and attaches it to the GitHub release.

- **XFS, Btrfs and EROFS are identified** (#36). They came back `unknown`:
  `am-partitions`' sniff has no kind for any of the three, and its window ends
  where the Btrfs superblock begins. `src/superblock.rs` recognises each from
  its own superblock -- the magic plus the fields that must agree with it --
  and runs only where that sniff answered `unknown`. `tests/oracle_tools.rs`
  writes each with its own mkfs, whole-device and as partitions of a GPT
  `sfdisk` wrote, and holds the answer to what `blkid -p` says.

- **Each partition reports its table slot, the rules it breaks, and how much
  of it is on the image.** `am-partitions` moves to 0.5.0, and every entry in
  the document gains `slot` (zero-based; absent when the entry has none),
  `issues` (the `PARTITIONS_ENTRY_*` bits; 0 for none) and
  `available_length`. `length` is what the table claims; `available_length`
  is what the image holds, and on an image cut short in the middle of a
  partition the two differ — the claim used to be the only number reported.
  `tests/partition_fields.rs` holds it to a real GPT image truncated half-way
  through its last partition, and `tests/oracle_tools.rs` holds `slot` to
  sfdisk's partition number and `available_length` to sfdisk's size (#28).

- **A partition-table probe CLI.** Reports the table type and partition layout
  of a device, and sniffs a filesystem on a device with no table at all.
- A lint gate and CI.
- **Every test tier is budgeted, logged and floored** (#17). `scripts/tier.sh`
  runs a tier quietly — the transcript goes to `tmp/logs/<tier>.log`, a pass
  prints one verdict line naming it, and a run that passed but printed more
  than its measured budget exits 65. `scripts/test-floor.sh` fails a tier that
  executed fewer tests than its floor, which is the failure a budget cannot
  see: `cargo test` exits 0 on "0 passed". The wrapper itself is
  `rust-fs-core`'s and is resolved at run time rather than committed here.
- **A release-profile tier.** Every test used to run only against
  `opt-level = 1` with `debug_assert!` bodies live and overflow checks on,
  while the shipped binary is built with `opt-level = 3`, LTO and none of
  those (#13).
- CI uploads `tmp/logs/` as an artifact on every leg, with `if: always()`.
- **The sibling refs are written once**, in `scripts/clone-siblings.sh`, which
  `ci.yml` and `fuzz.yml` both call (#27). They were spelled out in both
  workflows, and a pin in two places is a pin that moves in one of them —
  `fuzz.yml` being the copy that never reports on a pull request.
- Every sibling requirement in `Cargo.toml` names a **release** rather than a
  floor: `version = "0.2"` is satisfied by 0.2.0 through 0.2.13, so the lock
  alone decided what was built, and it sat seven patches behind while nothing
  complained. `tests/scripts/test-sibling-pins-agree.sh` refuses a floor, a
  dependency with no pin, a pin with no dependency, and a workflow that clones
  a sibling itself.
- **The probe is a library, and the binary is a front for it** (#16).
  `blk_probe::probe_path` is what the CLI runs; `blk_probe::probe_bytes` is
  the same probe over an image already in memory, backed by an `FsCoreDevice`
  built from callbacks rather than a file.
- **Two fuzzing tiers over one corpus.** `fuzz/` holds `cargo-fuzz` targets
  for the whole-device probe and for an image wearing another format's magic;
  `tests/fuzz_decoders.rs` replays and mutates the same corpus on the stable
  toolchain in every pull request, under a deadline and a floor of 15,000
  executed cases.
- `scripts/make-fuzz-corpus.sh` builds the corpus from images `sgdisk`,
  `sfdisk`, `mkfs.ext4`, `mksquashfs` and `qemu-img` wrote, and those images
  are the oracle for what the probe reports about them.
- **`blkid` and `sfdisk` are run over the same images and compared field by
  field** (#13), in `tests/oracle_tools.rs`: the table `blkid` reports, the
  filesystem it names on a whole-device image, and every partition's start,
  length, type GUID or type byte and name as `sfdisk --dump` prints them. It
  is compiled on Linux only, because those are util-linux's tools and there is
  no equivalent on macOS to reach for; where they are absent on Linux the test
  fails naming the package rather than returning early.

### Fixed

- **A use-after-free on a container that fails to open.** `*_open_on_device`
  consumes the handle it is given before it parses anything, so closing the
  inner device after a NULL return frees it twice; a truncated VHDX crashed
  the probe with SIGBUS instead of exiting 2. Found by the fuzz corpus on the
  first run of the gate that replayed it.
- The toolchain is pinned, matching the sibling crates.
- **One check gates a merge, and it stands for every job.** `ci.yml` grew a
  `ci-ok` job that `needs:` every other job in the workflow and fails when any
  of them failed, was cancelled or was **skipped**; `.github-guard` requires it
  and nothing else, in place of the three names it pinned before (`fmt`,
  `test / ubuntu-latest`, `test / macos-latest`). Two of those three were legs
  of one matrix, so adding a leg — the `ubuntu-24.04-arm` one #15 needs —
  would have produced a check that reports on every pull request and gates
  nothing. `tests/ci_aggregate_gate.rs` holds both halves to it: every job in
  `ci.yml` must appear in `ci-ok`'s `needs`, `ci-ok` must carry `if: always()`,
  and `.github-guard` must require `ci-ok` alone.

### Changed

- **The tool is `blk.probe` (breaking).** It was `blk-probe`. `chore binary`
  now leaves `dist/blk.probe`, `chore artifact` prints that path, usage reads
  `usage: blk.probe …` and error output is prefixed `blk.probe:`. Cargo
  refuses a dot in a target name, so the `[[bin]]` target is `blk_probe` and
  the new `scripts/stage-binary.sh` renames it when staging; nothing named
  `blk-probe` is built. **A consumer that stages or runs the tool by name must
  change with it.** The fuzz crate is renamed `rust-blk-probe-fuzz` to match.
- **The package is `rust-blk-probe` and the binary is `blk-probe`.** Both were
  `diskprobe`. The package now matches its repository, as every sibling will,
  and the binary matches the package. **A consumer that stages or runs the
  binary by name must change with it**: the build writes `dist/blk-probe`, and
  error output is prefixed `blk-probe:`.
- **Builds into its own `dist/` and returns that path**, rather than writing
  into whatever consumes it. Where the output lands is this crate's business,
  not its consumer's.
- **The container magics have names, and the JSON envelope is written once.**

### Fixed

- **A failed probe is distinguishable from a disk that simply has no partition
  table.** Both had been reported the same way, so a consumer could not tell
  "this disk is unpartitioned" from "I could not read this disk".
- **A short read is not a short file.** Treating one as the other truncates
  content silently instead of erroring.

[Unreleased]: https://github.com/antimatter-studios/rust-disk-partitions/commits/main/crates/blk-probe
[0.1.2]: https://github.com/antimatter-studios/rust-blk-probe/compare/v0.1.0...v0.1.2
[0.1.1]: https://github.com/antimatter-studios/rust-blk-probe/compare/v0.1.0...v0.1.2
[0.1.0]: https://github.com/antimatter-studios/rust-blk-probe/releases/tag/v0.1.0
