### Changed

- **CI compiles the release profile before the release tier runs.** A cold
  cache no longer puts cargo's `Compiling` lines inside the release tier's
  output budget (#55).
- **The family's scripts run in place from rust-fs-core, and this repository
  keeps no copy.** `scripts/core.sh` and `scripts/tier.sh` are gone; CI and
  chores run `../rust-fs-core/scripts/NAME.sh` at the pinned version
  (rust-fs-core 0.3.3, #212).
- **A release's notes are its CHANGELOG section**, and a tag the CHANGELOG
  does not describe stops before anything is published (rust-fs-core#209).
- **Built on the renamed crates.** `am-fs-core`, `am-img-qcow2`,
  `am-img-vhd`, `am-img-vhdx`, `am-img-vmdk` and `am-partitions` are
  `rust-fs-core` 0.3.3, `rust-img-qcow2` 0.6.0, `rust-img-vhd` 0.6.0,
  `rust-img-vhdx` 0.6.0, `rust-img-vmdk` 0.5.0 and `rust-disk-partitions`
  0.6.0, checked out beside this one at those tags. The tool's output and
  exit codes are unchanged.
