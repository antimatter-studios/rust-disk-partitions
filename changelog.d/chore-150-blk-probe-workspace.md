### Changed

- **The XFS, Btrfs and EROFS sniff checks the superblock's own fields, not
  only its magic.** XFS needs block and sector sizes that agree with their
  stored logarithms and at least one allocation group, Btrfs a `bytenr` naming
  64 KiB (the primary copy, not a mirror), and EROFS a block size from 512
  bytes to 64 KiB; eight right bytes inside some other structure are
  `Unknown`. The rules are the blk.probe tool's, moved into `sniff.rs` with
  the tool (#150), and its oracle holds them to `blkid -p` over images each
  format's own mkfs wrote.
- **The repository is a Cargo workspace.** The library stays at the root and
  is its only default member, so its dependencies, build and publish are
  unchanged; the `blk.probe` command-line tool, formerly the rust-blk-probe
  repository, is the second member at `crates/blk-probe` (#150).
