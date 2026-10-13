### Added

- **The sniffer reports bcachefs (#182).** `FsKind::Bcachefs`, from the
  superblock magic at byte 4096 + 0x18, and `PART_FS_BCACHEFS` (13) through
  the C ABI, where a bcachefs partition or image was `unknown`. Checked
  against a window of an image the reference formatter made.

### Changed

- **`FsKind` and `FsKindCode` have a new variant**, which breaks an
  exhaustive `match` on either; hence 0.7.0.
