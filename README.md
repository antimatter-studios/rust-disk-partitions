# partitions

Pure-Rust partition-table probe and filesystem-magic sniffer over any
random-access block source.

## What it does

Given a `BlockRead` (a tiny trait: `read_at(offset, buf)` + `size_bytes()`),
this crate tells you:

1. Is there a GPT or MBR partition table?
2. What partitions exist? (start, length, type, label, UUID)
3. For each partition, what filesystem signature is at the start?

It does **not** mount anything, decode files, or write — it's a probe.

## Status

### Read side (probe + sniff)

- [x] GPT primary header (signature, CRC32 validation, entry array)
- [x] GPT backup header read + validate, with primary/backup mismatch reporting (`gpt::parse_backup`, `gpt::validate_backup` returning `BackupStatus::Ok` / `Mismatch`)
- [x] MBR with GPT-protective fallthrough
- [x] FS sniff: ext2/3/4, NTFS, exFAT, FAT16, FAT32, HFS+, APFS, Linux swap, ISO 9660, SquashFS
- [x] `SliceReader` adapter — rebases offsets on a sub-range of any `BlockRead` (planned to move into `rust-fs-core` since slicing is a generic block-layer concern; this crate will re-export for backwards compatibility)
- [x] C ABI for FFI (`partitions_probe`, `partitions_count`, `partitions_table_kind`, `partitions_get`, `partitions_sniff`, `partitions_open_slice`, `partitions_list_free`, and `partitions_md_assemble` and `partitions_lvm_open`, which hand out an md array or a logical volume as one more device handle; header in `include/disk_partitions.h`)
- [x] Linux software RAID (`md`): superblocks 0.90, 1.0, 1.1 and 1.2
      (`md::read_superblock`), and `md::MdArray`, which assembles the
      members into one `BlockRead` reading the bytes the kernel's `/dev/mdX`
      would — RAID0 (one zone, or several in either recorded layout),
      RAID1, RAID4, RAID5 (all six layouts), RAID6 (every layout the
      kernel has, including the DDF and `-6` ones, with up to two members
      missing) and RAID10 (near, far and offset copies), the others each
      with one redundant member missing. Checked byte for byte against
      arrays the kernel built (`tests/oracle_md.rs`). Not yet: linear
      arrays and arrays mid-reshape, both refused by name
- [x] LVM2 (`lvm`): physical-volume labels (`lvm::read_pv_label`), the
      newest volume-group metadata (`lvm::read_volume_group`), and
      `lvm::LogicalVolume`, which reads a linear or striped logical volume
      as one `BlockRead` — including one on an `md` array, the way a
      Synology volume is built. Checked byte for byte against the
      kernel's device-mapper (`tests/oracle_lvm.rs`), including a group
      spanning two md arrays as SHR builds it, and the newer of a PV's two
      metadata copies (`--pvmetadatacopies 2`), and `raid1`, `raid4`,
      `raid5`, `raid6`, `raid10` (near layout) and `mirror` segments
      through their hidden image sub-volumes, and with images missing as
      far as each level survives: read from another copy or rebuilt from
      parity.
      Other segment types (thin, cache, snapshot, vdo) are refused by name
- [x] Discovery: `md::scan` sorts a set of devices into the arrays they
      are members of, `lvm::scan` sorts PVs (raw devices, slices or
      assembled arrays) into their volume groups, and `container::detect`
      says whether one device is an `md` member or an LVM2 PV. Checked by
      discovering a Synology-style layout from its disk images alone
      (`tests/oracle_lvm.rs`)
- [ ] LUKS detection
- [ ] Logical-partition (extended MBR) chain walking. Until it exists,
      `mbr::parse` and `probe` leave the extended container itself out of
      the list rather than reporting it as if it were one of the volumes
      inside it. The same goes for a `0xEE` GPT-protective entry in a
      hybrid MBR. `mbr::parse_all_entries` returns every entry for a tool
      that wants to show the table as it is on disk.

### Write side (table mutation)

- [x] GPT writer: protective MBR + primary header + entry array + backup mirror at end of disk, all CRCs computed correctly (`gpt_write::write_gpt`)
- [x] MBR writer (`mbr::write_mbr`, four primary entries)
- [x] Mutation API: `add` / `remove` / `resize` over an in-memory partition set, with 1 MiB alignment and a first-fit free-space finder (`PartitionSet`)
- [x] `commit(&dev)` semantics — writes happen on commit, not on mutation
- [x] Round-trip tests: probe → mutate → commit → re-probe matches intent (`tests/mutation.rs`)
- [x] Slot identity: a partition keeps the table slot it came from across a
      probe → mutate → commit round trip, so an unrelated edit does not
      renumber the disk. A partition with no slot yet takes the lowest free
      one (`Partition::slot`, `PartitionInfo.slot`)
- [ ] C ABI for the writer — the existing `partitions_*` handle stays read-only; a writable handle is a follow-up
- [ ] Optional `with_boot_code` variant of the MBR / protective-MBR writer for legacy BIOS boot

## Use

```rust
use disk_partitions::{probe, sniff, FileBlock};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dev = FileBlock::open("disk.img")?;
    let (table, parts) = probe(&dev)?;
    println!("{table:?}");
    for p in &parts {
        let kind = sniff(&dev, p)?;
        println!("{} bytes @ {} -> {:?}", p.length, p.start, kind);
    }
    Ok(())
}
```

## Layout

```
src/
  lib.rs        public API + BlockRead/BlockDevice + FileBlock + SliceReader
  error.rs      Error / Result
  gpt.rs        GPT header + entry array parser, backup-header validator
  gpt_write.rs  GPT writer (protective MBR + primary + backup, CRCs)
  mbr.rs        MBR parser + writer
  lvm.rs        LVM2 labels, metadata text, read-only logical volumes
  md.rs         Linux md superblocks + read-only array assembly
  mutation.rs   PartitionSet — in-memory add/remove/resize + commit
  sniff.rs      filesystem magic-byte sniffer
  probe.rs      top-level dispatch (try GPT, fall back to MBR)
tests/
  fixtures.rs   hand-built GPT/MBR + sniff fixtures
  mutation.rs   round-trip mutate/commit/re-probe tests
```

## Verifying a release

From the next release onward, every version published to crates.io is
also attached to the GitHub release for its tag, with a build-provenance
attestation signed by this repository's release workflow. It proves the
crate was built by `.github/workflows/release.yml` from a commit in this
repository, not uploaded from someone's machine. To check the crates.io
download of version `X.Y.Z`:

```sh
curl -sSfLo rust-disk-partitions-X.Y.Z.crate https://static.crates.io/crates/rust-disk-partitions/rust-disk-partitions-X.Y.Z.crate
gh attestation verify rust-disk-partitions-X.Y.Z.crate \
  --repo antimatter-studios/rust-disk-partitions \
  --signer-workflow antimatter-studios/rust-disk-partitions/.github/workflows/release.yml
```

The workflow refuses to attest a `.crate` whose sha256 differs from the
checksum crates.io records for that version, so the file on the release
page and the crates.io download are the same bytes.

## License

MIT.
