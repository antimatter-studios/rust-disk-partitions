### Added

- **The sniffer reports XFS, Btrfs and EROFS (#184).** `FsKind::Xfs`,
  `FsKind::Btrfs` and `FsKind::Erofs`, from each superblock's magic, and
  `PART_FS_XFS` (14), `PART_FS_BTRFS` (15) and `PART_FS_EROFS` (16) through
  the C ABI, where a partition or image holding one was `unknown`. The
  sniff window grows to 64 KiB plus the 72 bytes Btrfs's magic needs.
  Checked against windows of images each reference formatter made.
