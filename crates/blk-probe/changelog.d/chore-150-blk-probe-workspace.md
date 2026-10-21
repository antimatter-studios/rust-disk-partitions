### Added

- **bcachefs is reported as `"bcachefs"`.** The library has recognised it
  since 0.7.0, and this tool printed `"unknown"` for it because its table of
  filesystem codes stopped at SquashFS.

### Changed

- **The tool lives in the rust-disk-partitions workspace** at
  `crates/blk-probe`, and is released from there on `blk-probe-v<version>`
  tags, with the same `rust-blk-probe-<version>-<platform>.tar.gz` tarballs.
  Verify one with `gh attestation verify ... --repo
  antimatter-studios/rust-disk-partitions --signer-workflow
  antimatter-studios/rust-disk-partitions/.github/workflows/release-blk-probe.yml`
  (#150).
- **The filesystem sniff is the library's alone.** The XFS, Btrfs and EROFS
  rules this tool kept in `src/superblock.rs` moved into rust-disk-partitions'
  `sniff.rs`, and the tool reads their answer through the C ABI like every
  other kind. `blk_probe::superblock` is gone; the JSON is unchanged.
- **Built on rust-disk-partitions 0.7.1 from this workspace, rust-fs-core
  0.3.12, and the image readers from crates.io**: rust-img-qcow2 0.6.1,
  rust-img-vhd 0.6.1, rust-img-vhdx 0.6.1 and rust-img-vmdk 0.5.1.
