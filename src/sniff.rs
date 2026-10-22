//! Filesystem-magic sniffer. Reads a small window from the start of a
//! partition and identifies the filesystem by its on-disk signature. This
//! does NOT validate the filesystem — it just answers "what is this likely
//! to be?" The driver itself does proper validation when mounting.

use crate::error::{Error, Result};
use crate::probe::Partition;
use crate::BlockRead;

/// Recognised filesystem signatures. Non-exhaustive: a filesystem the
/// sniffer learns to recognise is a new variant, which is then an
/// addition rather than a break for a caller's `match` (#184).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FsKind {
    /// ext2 / ext3 / ext4 — superblock magic 0xEF53 at byte 1080. The
    /// `version` field carries the best guess from feature flags.
    Ext { version: ExtVersion },
    /// NTFS — "NTFS    " OEM name at boot-sector offset 3.
    Ntfs,
    /// exFAT — "EXFAT   " OEM name at boot-sector offset 3.
    ExFat,
    /// FAT32 — "FAT32   " in the extended BPB at offset 0x52.
    Fat32,
    /// FAT16 / FAT12 — "FAT16   " or "FAT12   " in the extended BPB at 0x36.
    Fat16,
    /// HFS+ — "H+" or "HX" at offset 1024.
    HfsPlus,
    /// APFS container — "NXSB" at offset 32.
    Apfs,
    /// Linux swap — "SWAPSPACE2" near the end of the first page.
    LinuxSwap,
    /// ISO 9660 — "CD001" at offset 0x8001.
    Iso9660,
    /// SquashFS — "hsqs" little-endian magic at offset 0.
    Squashfs,
    /// bcachefs — a 16-byte magic 0x18 bytes into the primary superblock,
    /// which is at byte 4096 (sector 8).
    Bcachefs,
    /// Detected nothing recognisable.
    Unknown,
    // Variants added after `Unknown` keep its discriminant, which a caller
    // may have cast to a number (#184).
    /// XFS — "XFSB" at offset 0, where the primary superblock starts, with
    /// block and sector sizes that agree with their stored logarithms and at
    /// least one allocation group.
    Xfs,
    /// Btrfs — "_BHRfS_M" 0x40 bytes into the superblock at 64 KiB, whose
    /// `bytenr` names 64 KiB: the primary copy, not a mirror.
    Btrfs,
    /// EROFS — magic 0xE0F5E1E2, little-endian, at offset 1024, with a block
    /// size from 512 bytes to 64 KiB.
    Erofs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtVersion {
    /// Unable to distinguish ext2 vs ext3 vs ext4 — feature flags say no
    /// journal nor any ext4-only features.
    Ext2OrAny,
    /// HAS_JOURNAL set, no ext4-only incompat features → ext3.
    Ext3,
    /// At least one ext4 incompat feature set (extents, 64bit, flex_bg, etc).
    Ext4,
}

/// Sniff the filesystem at the start of `partition`. The partition's `length`
/// determines how much we're allowed to read.
///
/// The window is also clamped to the device, for the same reason
/// `capi::slice_on_device` clamps: a partition running past the end of
/// the device it was found on is ordinary rather than hostile — a `dd`
/// of the first part of a disk, or a table left stale after the volume
/// was shrunk, produces one — and `read_at` is all-or-nothing, so a
/// window sized from the *claim* fails outright on exactly the image the
/// clamp exists for. Without it, `partitions_sniff` returned a short
/// read for a partition `partitions_open_slice` opened happily, and the
/// consumer had to explain why something it can read has no filesystem.
///
/// A partition beginning at or past the end of the device has nothing to
/// read and is an error, checked before the window is sized: clamping
/// must not turn "there is no such region" into "an unrecognised
/// filesystem". Everything `classify` looks at is already length-guarded,
/// so a short window simply rules out the probes it cannot reach.
///
/// The window is [`WINDOW`] bytes, clamped to the partition's length.
pub fn sniff(dev: &dyn BlockRead, partition: &Partition) -> Result<FsKind> {
    let device_size = dev.size_bytes();
    // Refused here rather than left to the read: a zero-length read is
    // `Ok` at any offset on a real `FileDevice`, so the clamp below would
    // otherwise hand `classify` an empty buffer and call a region that
    // does not exist an unrecognised filesystem (#53).
    if partition.start >= device_size {
        return Err(Error::Block(fs_core::Error::ShortRead {
            offset: partition.start,
            want: std::cmp::min(WINDOW, partition.length) as usize,
            got: 0,
        }));
    }
    let want = std::cmp::min(WINDOW, partition.length);
    let available = device_size - partition.start;
    let mut buf = vec![0u8; std::cmp::min(want, available) as usize];
    dev.read_at(partition.start, &mut buf)?;
    Ok(classify(&buf))
}

/// Where bcachefs's magic is: 0x18 bytes into the superblock at byte 4096.
const BCACHEFS_MAGIC_AT: usize = 4096 + 0x18;

/// bcachefs's superblock magic.
const BCACHEFS_MAGIC: [u8; 16] = [
    0xc6, 0x85, 0x73, 0xf6, 0x66, 0xce, 0x90, 0xa9, 0xd9, 0x6a, 0x60, 0xcf, 0x80, 0x3d, 0xf7, 0xef,
];

/// Where the primary Btrfs superblock is: 64 KiB into the volume.
const BTRFS_SUPERBLOCK_AT: usize = 0x10000;

/// Where Btrfs's magic is: 0x40 bytes into the superblock at 64 KiB.
const BTRFS_MAGIC_AT: usize = BTRFS_SUPERBLOCK_AT + 0x40;

/// Btrfs's superblock magic.
const BTRFS_MAGIC: &[u8; 8] = b"_BHRfS_M";

/// Where EROFS's magic is: the superblock at byte 1024 starts with it.
const EROFS_MAGIC_AT: usize = 1024;

/// EROFS's superblock magic, 0xE0F5E1E2 as it lies on disk (little-endian).
const EROFS_MAGIC: [u8; 4] = [0xe2, 0xe1, 0xf5, 0xe0];

/// End of the ISO9660 `CD001` identifier: the furthest byte that probe reads.
const ISO9660_END: usize = 0x8006;

/// The Linux swap page sizes [`classify`] probes; `SWAPSPACE2` ends each one.
const SWAP_PAGES: [usize; 5] = [4096, 8192, 16384, 32768, 65536];

/// How many bytes [`sniff`] reads from the start of a partition: the end
/// of the furthest probe [`classify`] makes.
///
/// Derived from the probes rather than written as a number, because it
/// was once written as one — `0x8800`, sized for ISO9660 — while the swap
/// probe reached 64 KiB, and that page was unreachable through `sniff`
/// (#25).
pub const WINDOW: u64 = {
    let largest_page = SWAP_PAGES[SWAP_PAGES.len() - 1];
    let mut end = if largest_page > ISO9660_END {
        largest_page
    } else {
        ISO9660_END
    };
    // Btrfs's magic is past the last swap page (#184).
    let btrfs_end = BTRFS_MAGIC_AT + BTRFS_MAGIC.len();
    if btrfs_end > end {
        end = btrfs_end;
    }
    end as u64
};

/// Stand-alone classifier — exposed for tests and for callers who already
/// have the bytes in hand.
pub fn classify(buf: &[u8]) -> FsKind {
    // SquashFS first — magic at offset 0, cheap.
    if buf.len() >= 4 && &buf[0..4] == b"hsqs" {
        return FsKind::Squashfs;
    }

    // bcachefs: the primary superblock at sector 8, its magic 0x18 bytes in
    // (#182). Sixteen bytes no other filesystem here writes at that place.
    if buf.len() >= BCACHEFS_MAGIC_AT + BCACHEFS_MAGIC.len()
        && buf[BCACHEFS_MAGIC_AT..BCACHEFS_MAGIC_AT + BCACHEFS_MAGIC.len()] == BCACHEFS_MAGIC
    {
        return FsKind::Bcachefs;
    }

    // XFS: the superblock is the volume's first sector (#184), and its
    // sizes agree with their own logarithms (#150).
    if is_xfs(buf) {
        return FsKind::Xfs;
    }

    // EROFS: the superblock at byte 1024 opens with its magic (#184), and
    // its block size is one the format allows (#150).
    if buf.len() > EROFS_MAGIC_AT && is_erofs(&buf[EROFS_MAGIC_AT..]) {
        return FsKind::Erofs;
    }

    // Btrfs: the superblock at 64 KiB, its magic 0x40 bytes in (#184), and
    // it says it lives at 64 KiB (#150).
    if buf.len() > BTRFS_SUPERBLOCK_AT && is_btrfs(&buf[BTRFS_SUPERBLOCK_AT..]) {
        return FsKind::Btrfs;
    }

    // FAT / NTFS / exFAT all start with a BPB-like layout — boot sector at 0,
    // 0x55 0xAA at 510, OEM-ish strings at offset 3.
    if buf.len() >= 512 && buf[510] == 0x55 && buf[511] == 0xAA {
        let oem = &buf[3..11];
        if oem == b"NTFS    " {
            return FsKind::Ntfs;
        }
        if oem == b"EXFAT   " {
            return FsKind::ExFat;
        }
        // FAT32 stores "FAT32   " at offset 0x52 (extended BPB).
        if buf.len() >= 0x5A {
            let fat32_tag = &buf[0x52..0x5A];
            if fat32_tag == b"FAT32   " {
                return FsKind::Fat32;
            }
        }
        // FAT16 / FAT12 store the tag at offset 0x36.
        if buf.len() >= 0x3E {
            let fat16_tag = &buf[0x36..0x3E];
            if fat16_tag == b"FAT16   " || fat16_tag == b"FAT12   " {
                return FsKind::Fat16;
            }
        }
    }

    // ext: superblock at offset 1024, magic 0xEF53 at offset 1080.
    if buf.len() >= 1082 {
        let magic = u16::from_le_bytes([buf[1080], buf[1081]]);
        if magic == 0xEF53 {
            return FsKind::Ext {
                version: classify_ext(buf),
            };
        }
    }

    // HFS+: signature 'H+' (0x4842 BE) or 'HX' at offset 1024.
    if buf.len() >= 1026 {
        let sig = &buf[1024..1026];
        if sig == b"H+" || sig == b"HX" {
            return FsKind::HfsPlus;
        }
    }

    // APFS: container superblock magic 'NXSB' at offset 32.
    if buf.len() >= 36 && &buf[32..36] == b"NXSB" {
        return FsKind::Apfs;
    }

    // Linux swap: 'SWAPSPACE2' at (page_size - 10). Page can be 4096..65536.
    // Probe the common pages.
    for page in SWAP_PAGES {
        if buf.len() >= page {
            let off = page - 10;
            if &buf[off..off + 10] == b"SWAPSPACE2" {
                return FsKind::LinuxSwap;
            }
        }
    }

    // ISO 9660: 'CD001' at offset 0x8001.
    if buf.len() >= ISO9660_END && &buf[0x8001..ISO9660_END] == b"CD001" {
        return FsKind::Iso9660;
    }

    FsKind::Unknown
}

// THE THREE RULES BELOW CAME FROM rust-blk-probe's `superblock.rs` (#150).
// The tool used to run them itself, after this sniff had answered `Unknown`,
// because the magic alone let eight right bytes in the middle of some other
// structure pass for a superblock. Each is the magic plus the fields that
// give the rest of the superblock its meaning; the layouts are the formats'
// own (`xfs_format.h`, `btrfs_tree.h`, `erofs_fs.h`), and the tool's oracle
// (crates/blk-probe/tests/oracle_tools.rs) holds every answer to what
// `blkid -p` says about an image each format's own mkfs wrote.

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// `struct xfs_dsb`, big-endian, at the start of `sb`: `sb_magicnum` at 0,
/// `sb_blocksize` at 4, `sb_agcount` at 88, `sb_sectsize` at 102,
/// `sb_blocklog` at 120 and `sb_sectlog` at 121.
///
/// The block and sector sizes are each stored twice, once as a size and once
/// as a log2, and must agree -- which is the check that turns four magic
/// bytes into a superblock. The ranges are the format's: a block of 512 bytes
/// to 64 KiB, a sector of 512 bytes to 32 KiB, and at least one allocation
/// group.
fn is_xfs(sb: &[u8]) -> bool {
    if sb.len() < 122 || &sb[0..4] != b"XFSB" {
        return false;
    }
    let block = be32(sb, 4);
    let sector = u32::from(be16(sb, 102));
    let (block_log, sector_log) = (u32::from(sb[120]), u32::from(sb[121]));
    (9..=16).contains(&block_log)
        && block == 1 << block_log
        && (9..=15).contains(&sector_log)
        && sector == 1 << sector_log
        && be32(sb, 88) > 0
}

/// `struct btrfs_super_block`, little-endian, at the start of `sb`: `bytenr`
/// at 48, the magic at 64.
///
/// `bytenr` is where the superblock says it lives. The primary copy's is its
/// own offset, 64 KiB, so a stray `_BHRfS_M` -- or a mirror copy seen through
/// the wrong window -- does not pass for a primary superblock.
fn is_btrfs(sb: &[u8]) -> bool {
    sb.len() >= 72
        && &sb[64..72] == BTRFS_MAGIC
        && u64::from_le_bytes(sb[48..56].try_into().unwrap()) == BTRFS_SUPERBLOCK_AT as u64
}

/// `struct erofs_super_block`, little-endian, at the start of `sb`: the magic
/// at 0 and `blkszbits` at 12, the log2 of the block size, which the format
/// allows from 512 bytes to 64 KiB.
fn is_erofs(sb: &[u8]) -> bool {
    sb.len() >= 13 && sb[0..4] == EROFS_MAGIC && (9..=16).contains(&sb[12])
}

/// Best-effort ext2/3/4 discrimination from the superblock feature flags.
///
/// Layout (offsets relative to start of partition):
///
/// ```text
///   0x400  +0x5C  s_feature_compat   (u32 little-endian)
///   0x400  +0x60  s_feature_incompat
///   0x400  +0x64  s_feature_ro_compat
/// ```
fn classify_ext(buf: &[u8]) -> ExtVersion {
    let sb = 1024usize;
    if buf.len() < sb + 0x68 {
        return ExtVersion::Ext2OrAny;
    }
    let feature_compat = u32::from_le_bytes(buf[sb + 0x5C..sb + 0x60].try_into().unwrap());
    let feature_incompat = u32::from_le_bytes(buf[sb + 0x60..sb + 0x64].try_into().unwrap());

    const EXT3_FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x4;
    // Any of these incompat bits implies ext4.
    const EXT4_INCOMPAT_MASK: u32 = 0x040 // EXTENTS
        | 0x080 // 64BIT
        | 0x100 // MMP
        | 0x200 // FLEX_BG
        | 0x400 // EA_INODE
        | 0x1000 // DIRDATA
        | 0x2000 // BG_USE_META_CSUM
        | 0x4000 // LARGEDIR
        | 0x8000; // INLINE_DATA

    if feature_incompat & EXT4_INCOMPAT_MASK != 0 {
        return ExtVersion::Ext4;
    }
    if feature_compat & EXT3_FEATURE_COMPAT_HAS_JOURNAL != 0 {
        return ExtVersion::Ext3;
    }
    ExtVersion::Ext2OrAny
}
