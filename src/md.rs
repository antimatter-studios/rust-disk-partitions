//! Linux software RAID (`md`): member superblocks, and the assembled
//! array as a block device.
//!
//! A disk that was a member of an `md` array has no filesystem at the
//! start of its partition. It has an `md` superblock — at the start, 4 KiB
//! in, or near the end, depending on the metadata version — and the
//! array's data at an offset the superblock records. A RAID1 leg holds a
//! whole copy of the array there; every other level holds only its share
//! of the stripes. This module reads the superblock of each member and
//! assembles the members into one [`BlockRead`] that reads the bytes the
//! kernel's `/dev/mdX` would.
//!
//! # What is read
//!
//! * **Metadata 0.90**, at the last 64 KiB-aligned 64 KiB of the member,
//!   in the little-endian layout an x86 or arm64 host writes.
//! * **Metadata 1.0, 1.1 and 1.2**, at 8 KiB before the end (rounded down
//!   to 4 KiB), at 0, and at 4 KiB respectively. The 1.x checksum is
//!   verified; the 0.90 one is not.
//!
//! The layouts were taken from the published descriptions of the on-disk
//! format (the kernel's md documentation and the Linux RAID wiki's
//! "RAID superblock formats" page) and checked against images the kernel
//! itself wrote: `tests/oracle_md.rs` builds arrays with `mdadm` on loop
//! devices and requires every byte read through [`MdArray`] to equal what
//! the kernel's `/dev/mdX` returned.
//!
//! # What is assembled
//!
//! | level | layouts | members that may be missing |
//! |---|---|---|
//! | RAID0 | one zone, or several over members of different sizes (the `alternate` layout) | none |
//! | RAID1 | — | all but one |
//! | RAID4 | parity on the last member | one |
//! | RAID5 | left/right, symmetric/asymmetric, parity-first, parity-last | one |
//! | RAID6 | left-symmetric (the `mdadm` default) | two, through P and Q |
//! | RAID10 | near, far and offset copies | any, while one copy of every chunk is left |
//!
//! Anything else is refused by name with [`MdError::Unsupported`]: linear
//! arrays, multi-zone RAID0 in the `original` layout or with no layout
//! recorded, and an array in the middle of a reshape. Refusing is the
//! point: a layout read with the wrong geometry returns plausible bytes
//! from the wrong places.
//!
//! RAID6's second syndrome, Q, is the Reed-Solomon code over GF(2^8) that
//! H. Peter Anvin's "The mathematics of RAID-6" describes: generator
//! `{02}`, field polynomial `0x11d`, and data chunk `i` of a row weighted
//! by `{02}^i`. Two lost chunks of a row are recovered with that paper's
//! formulas.
//!
//! The array is read-only. Writing would have to keep parity, bitmaps and
//! event counts consistent with what the kernel expects, and nothing here
//! does that.

use std::fmt;

use crate::BlockRead;

/// The magic number at the start of every `md` superblock, all versions.
pub const MD_MAGIC: u32 = 0xa92b_4efc;

/// Bytes in the sectors every `md` superblock counts in.
const SECTOR: u64 = 512;

/// Size of the fixed part of a 1.x superblock, before the role table.
const V1_FIXED: usize = 256;
/// The 1.x role table holds at most this many entries in the 4 KiB the
/// superblock is given. Anything larger is corrupt.
const V1_MAX_DEV: u32 = (4096 - V1_FIXED as u32) / 2;
/// Size of a 0.90 superblock.
const V0_SIZE: usize = 4096;
/// A 0.90 superblock lives in the last 64 KiB-aligned 64 KiB of the device.
const V0_RESERVED: u64 = 64 * 1024;

/// Feature bit: the member is only partly recovered (`recovery_offset`).
const FEATURE_RECOVERY_OFFSET: u32 = 0x2;
/// Feature bit: a reshape is in progress.
const FEATURE_RESHAPE_ACTIVE: u32 = 0x4;

/// The level codes the superblock stores.
const LEVEL_RAID0: i32 = 0;
const LEVEL_RAID1: i32 = 1;
const LEVEL_RAID4: i32 = 4;
const LEVEL_RAID5: i32 = 5;
const LEVEL_RAID6: i32 = 6;
const LEVEL_RAID10: i32 = 10;

/// Multi-zone RAID0 layouts, by the number the superblock stores. A
/// single-zone RAID0 reads the same whatever it says.
const RAID0_LAYOUT_ORIGINAL: u32 = 1;
const RAID0_LAYOUT_ALTERNATE: u32 = 2;

/// RAID10 layout word: near copies in bits 0-7, far copies in 8-15, and
/// this bit when the far copies are offset (one stripe apart) rather than
/// far (one section of the member apart).
const RAID10_OFFSET: u32 = 1 << 16;

/// RAID5/6 parity layouts, by the number the superblock stores.
const LAYOUT_LEFT_ASYMMETRIC: u32 = 0;
const LAYOUT_RIGHT_ASYMMETRIC: u32 = 1;
const LAYOUT_LEFT_SYMMETRIC: u32 = 2;
const LAYOUT_RIGHT_SYMMETRIC: u32 = 3;
const LAYOUT_PARITY_FIRST: u32 = 4;
const LAYOUT_PARITY_LAST: u32 = 5;
/// RAID6 only. The three DDF layouts order Q's coefficients by member
/// rather than by data chunk, and the `_6` layouts are a RAID5 layout over
/// all but the last member, with Q on the last.
const LAYOUT_ROTATING_ZERO_RESTART: u32 = 8;
const LAYOUT_ROTATING_N_RESTART: u32 = 9;
const LAYOUT_ROTATING_N_CONTINUE: u32 = 10;
const LAYOUT_LEFT_ASYMMETRIC_6: u32 = 16;
const LAYOUT_RIGHT_ASYMMETRIC_6: u32 = 17;
const LAYOUT_LEFT_SYMMETRIC_6: u32 = 18;
const LAYOUT_RIGHT_SYMMETRIC_6: u32 = 19;
const LAYOUT_PARITY_FIRST_6: u32 = 20;

/// Which metadata format a member carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdVersion {
    /// 0.90: at the end of the device, native-endian, 4 KiB.
    V0_90,
    /// 1.0: 1.x format, 8 KiB before the end of the device.
    V1_0,
    /// 1.1: 1.x format, at the start of the device.
    V1_1,
    /// 1.2: 1.x format, 4 KiB into the device. The `mdadm` default.
    V1_2,
}

/// What a member is to its array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdRole {
    /// An in-sync member in this slot (0-based) of the array.
    Active(u32),
    /// A spare, holding no array data.
    Spare,
    /// A member the array has marked failed.
    Faulty,
    /// A write journal device.
    Journal,
}

/// One member's superblock, decoded.
///
/// All offsets and sizes are in bytes, converted from the 512-byte
/// sectors the superblock stores them in.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MdSuperblock {
    /// Metadata format, which also says where the superblock was found.
    pub version: MdVersion,
    /// Byte offset of the superblock on the member.
    pub superblock_offset: u64,
    /// The array's UUID, in on-disk byte order. Every member of one
    /// array carries the same value.
    pub array_uuid: [u8; 16],
    /// The array's name (1.x only; empty for 0.90), e.g. `host:0`.
    pub name: String,
    /// RAID level as stored: 0, 1, 4, 5, 6, 10, or negative for linear
    /// and multipath.
    pub level: i32,
    /// Parity or mirror layout code (RAID5/6/10).
    pub layout: u32,
    /// Chunk size in bytes (0 for RAID1).
    pub chunk_bytes: u64,
    /// Number of member slots in the array.
    pub raid_disks: u32,
    /// This member's role.
    pub role: MdRole,
    /// Where the array data starts on this member.
    pub data_offset: u64,
    /// Bytes of array data this member can hold, from `data_offset`.
    pub data_size: u64,
    /// Bytes of each member the array uses (the superblock's `size`).
    pub component_size: u64,
    /// Update counter. Members whose count is behind the newest are
    /// stale and are not used.
    pub events: u64,
    /// The 1.x `feature_map` (0 for 0.90).
    pub feature_map: u32,
}

/// Why a set of members could not be read as an array.
#[derive(Debug)]
#[non_exhaustive]
pub enum MdError {
    /// The underlying device failed.
    Block(fs_core::Error),
    /// Member `member` carries no `md` superblock.
    NoSuperblock { member: usize },
    /// Member `member`'s 1.x superblock checksum does not match.
    BadChecksum {
        member: usize,
        stored: u32,
        computed: u32,
    },
    /// A superblock field is out of range or inconsistent.
    Corrupt { member: usize, reason: String },
    /// Member `member` belongs to a different array from member 0.
    MixedArrays { member: usize },
    /// Two members claim the same slot.
    DuplicateRole { slot: u32 },
    /// More members are missing than the level can do without.
    TooFewMembers {
        level: i32,
        present: u32,
        needed: u32,
    },
    /// Every copy of some RAID10 chunk is on a missing member. `slots`
    /// are the slots one such chunk is kept on.
    NoCopyLeft { slots: Vec<u32> },
    /// A level, layout or state this module does not assemble.
    Unsupported(String),
    /// No members were given.
    Empty,
}

impl fmt::Display for MdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MdError::Block(e) => write!(f, "{e}"),
            MdError::NoSuperblock { member } => write!(f, "member {member}: no md superblock"),
            MdError::BadChecksum {
                member,
                stored,
                computed,
            } => write!(
                f,
                "member {member}: superblock checksum {stored:#010x}, computed {computed:#010x}"
            ),
            MdError::Corrupt { member, reason } => write!(f, "member {member}: {reason}"),
            MdError::MixedArrays { member } => {
                write!(f, "member {member} belongs to a different array")
            }
            MdError::DuplicateRole { slot } => write!(f, "two members claim slot {slot}"),
            MdError::TooFewMembers {
                level,
                present,
                needed,
            } => write!(
                f,
                "RAID{level} needs {needed} usable members, {present} present"
            ),
            MdError::NoCopyLeft { slots } => {
                write!(f, "every copy of some data is on missing slots {slots:?}")
            }
            MdError::Unsupported(s) => write!(f, "unsupported: {s}"),
            MdError::Empty => write!(f, "no members given"),
        }
    }
}

impl std::error::Error for MdError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MdError::Block(e) => Some(e),
            _ => None,
        }
    }
}

impl From<fs_core::Error> for MdError {
    fn from(e: fs_core::Error) -> Self {
        MdError::Block(e)
    }
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// The 1.x superblock checksum over `sb`, which must already be cut to
/// `256 + 2 * max_dev` bytes.
///
/// The sum of the little-endian 32-bit words with the checksum field
/// (offset 216) taken as zero, a trailing 16-bit word added if the
/// length is not a multiple of four, and the 64-bit total folded once
/// into 32 bits.
pub fn v1_checksum(sb: &[u8]) -> u32 {
    let mut sum: u64 = 0;
    let mut i = 0;
    while i + 4 <= sb.len() {
        if i != 216 {
            sum += u64::from(le32(sb, i));
        }
        i += 4;
    }
    if i + 2 <= sb.len() {
        sum += u64::from(le16(sb, i));
    }
    ((sum & 0xffff_ffff) + (sum >> 32)) as u32
}

fn corrupt(member: usize, reason: impl Into<String>) -> MdError {
    MdError::Corrupt {
        member,
        reason: reason.into(),
    }
}

/// Decode a 1.x superblock read from `at` on a device of `dev_size`
/// bytes. `Ok(None)` when the magic or major version is not there.
fn parse_v1(
    buf: &[u8],
    at: u64,
    version: MdVersion,
    dev_size: u64,
    member: usize,
) -> Result<Option<MdSuperblock>, MdError> {
    if le32(buf, 0) != MD_MAGIC || le32(buf, 4) != 1 {
        return Ok(None);
    }
    // The superblock records where it lives; a 1.2 superblock seen from
    // the 1.1 position (or the reverse) does not say it is there.
    let super_offset = le64(buf, 144);
    if super_offset.checked_mul(SECTOR) != Some(at) {
        return Ok(None);
    }
    let max_dev = le32(buf, 220);
    if max_dev > V1_MAX_DEV {
        return Err(corrupt(member, format!("max_dev {max_dev} exceeds 4 KiB")));
    }
    let len = V1_FIXED + 2 * max_dev as usize;
    let stored = le32(buf, 216);
    let computed = v1_checksum(&buf[..len]);
    if stored != computed {
        return Err(MdError::BadChecksum {
            member,
            stored,
            computed,
        });
    }

    let mut array_uuid = [0u8; 16];
    array_uuid.copy_from_slice(&buf[16..32]);
    let name_raw = &buf[32..64];
    let name_end = name_raw.iter().position(|&c| c == 0).unwrap_or(32);
    let name = String::from_utf8_lossy(&name_raw[..name_end]).into_owned();

    let level = le32(buf, 72) as i32;
    let layout = le32(buf, 76);
    let size = le64(buf, 80);
    let chunk = le32(buf, 88);
    let raid_disks = le32(buf, 92);
    let data_offset = le64(buf, 128);
    let data_size = le64(buf, 136);
    let dev_number = le32(buf, 160);
    let events = le64(buf, 200);
    let feature_map = le32(buf, 8);

    let to_bytes = |sectors: u64, what: &str| {
        sectors
            .checked_mul(SECTOR)
            .ok_or_else(|| corrupt(member, format!("{what} overflows")))
    };
    let data_offset = to_bytes(data_offset, "data_offset")?;
    let data_size = to_bytes(data_size, "data_size")?;
    let component_size = to_bytes(size, "size")?;
    match data_offset.checked_add(data_size) {
        Some(end) if end <= dev_size => {}
        _ => {
            return Err(corrupt(
                member,
                format!(
                    "data area {data_offset}+{data_size} runs past the device ({dev_size} bytes)"
                ),
            ))
        }
    }

    let role = if dev_number >= max_dev {
        return Err(corrupt(
            member,
            format!("dev_number {dev_number} is outside the role table ({max_dev})"),
        ));
    } else {
        match le16(buf, V1_FIXED + 2 * dev_number as usize) {
            0xffff => MdRole::Spare,
            0xfffe => MdRole::Faulty,
            0xfffd => MdRole::Journal,
            r => MdRole::Active(u32::from(r)),
        }
    };

    Ok(Some(MdSuperblock {
        version,
        superblock_offset: at,
        array_uuid,
        name,
        level,
        layout,
        chunk_bytes: u64::from(chunk) * SECTOR,
        raid_disks,
        role,
        data_offset,
        data_size,
        component_size,
        events,
        feature_map,
    }))
}

/// Decode a 0.90 superblock. `Ok(None)` when it is not one.
fn parse_v0(buf: &[u8], at: u64, member: usize) -> Result<Option<MdSuperblock>, MdError> {
    let w = |i: usize| le32(buf, i * 4);
    if w(0) != MD_MAGIC || w(1) != 0 || w(2) != 90 {
        return Ok(None);
    }
    let mut array_uuid = [0u8; 16];
    for (k, word) in [5usize, 13, 14, 15].into_iter().enumerate() {
        array_uuid[k * 4..k * 4 + 4].copy_from_slice(&buf[word * 4..word * 4 + 4]);
    }
    let level = w(7) as i32;
    let size_kib = u64::from(w(8));
    let raid_disks = w(10);
    // Events: a 64-bit counter laid out so a little-endian host reads it
    // natively at word 39.
    let events = le64(buf, 39 * 4);
    let layout = w(64);
    let chunk_bytes = u64::from(w(65));
    // This member's descriptor: number, major, minor, raid_disk, state.
    let this = 992;
    let raid_disk = w(this + 3);
    let state = w(this + 4);
    const FAULTY: u32 = 1 << 0;
    const ACTIVE: u32 = 1 << 1;
    const SYNC: u32 = 1 << 2;
    let role = if state & FAULTY != 0 {
        MdRole::Faulty
    } else if state & ACTIVE != 0 && state & SYNC != 0 {
        MdRole::Active(raid_disk)
    } else {
        MdRole::Spare
    };
    let component_size = size_kib * 1024;
    if component_size > at {
        return Err(corrupt(
            member,
            format!("size {component_size} runs into the superblock at {at}"),
        ));
    }
    Ok(Some(MdSuperblock {
        version: MdVersion::V0_90,
        superblock_offset: at,
        array_uuid,
        name: String::new(),
        level,
        layout,
        chunk_bytes,
        raid_disks,
        role,
        data_offset: 0,
        // Everything before the reserved area is usable.
        data_size: at,
        component_size,
        events,
        feature_map: 0,
    }))
}

/// Read the `md` superblock of one device, trying every version's
/// location. `Ok(None)` when there is none.
///
/// A 1.x superblock is preferred over a 0.90 one when both are present,
/// because 1.x carries a checksum this module verifies.
pub fn read_superblock<R: BlockRead + ?Sized>(dev: &R) -> Result<Option<MdSuperblock>, MdError> {
    read_superblock_of(dev, 0)
}

fn read_superblock_of<R: BlockRead + ?Sized>(
    dev: &R,
    member: usize,
) -> Result<Option<MdSuperblock>, MdError> {
    let size = dev.size_bytes();
    let mut buf = vec![0u8; 4096];
    let mut candidates = vec![(0u64, MdVersion::V1_1), (4096u64, MdVersion::V1_2)];
    if let Some(end) = (size / SECTOR).checked_sub(16) {
        candidates.push(((end & !7) * SECTOR, MdVersion::V1_0));
    }
    for (at, version) in candidates {
        if at + 4096 > size {
            continue;
        }
        dev.read_at(at, &mut buf)?;
        if let Some(sb) = parse_v1(&buf, at, version, size, member)? {
            return Ok(Some(sb));
        }
    }
    if size >= V0_RESERVED {
        let at = (size & !(V0_RESERVED - 1)) - V0_RESERVED;
        let mut b0 = vec![0u8; V0_SIZE];
        dev.read_at(at, &mut b0)?;
        return parse_v0(&b0, at, member);
    }
    Ok(None)
}

/// Where a RAID5/6 chunk lives: (data member, parity member, Q member).
pub(crate) fn parity_map(
    level: i32,
    layout: u32,
    n: u64,
    stripe: u64,
    i: u64,
) -> (usize, usize, Option<usize>) {
    if level == LEVEL_RAID6 {
        let (dd, pd, qd) = raid6_map(layout, n, stripe, i);
        return (dd as usize, pd as usize, Some(qd as usize));
    }
    let data = n - 1;
    let (pd, dd) = match layout {
        LAYOUT_LEFT_ASYMMETRIC => {
            let pd = data - stripe % n;
            (pd, if i >= pd { i + 1 } else { i })
        }
        LAYOUT_RIGHT_ASYMMETRIC => {
            let pd = stripe % n;
            (pd, if i >= pd { i + 1 } else { i })
        }
        LAYOUT_LEFT_SYMMETRIC => {
            let pd = data - stripe % n;
            (pd, (pd + 1 + i) % n)
        }
        LAYOUT_RIGHT_SYMMETRIC => {
            let pd = stripe % n;
            (pd, (pd + 1 + i) % n)
        }
        LAYOUT_PARITY_FIRST => (0, i + 1),
        // LAYOUT_PARITY_LAST, and RAID4 whatever its layout says.
        _ => (data, i),
    };
    (dd as usize, pd as usize, None)
}

/// The RAID6 layouts [`raid6_map`] places.
fn raid6_layout_known(layout: u32) -> bool {
    matches!(
        layout,
        LAYOUT_LEFT_ASYMMETRIC..=LAYOUT_PARITY_LAST
            | LAYOUT_ROTATING_ZERO_RESTART..=LAYOUT_ROTATING_N_CONTINUE
            | LAYOUT_LEFT_ASYMMETRIC_6..=LAYOUT_PARITY_FIRST_6
    )
}

/// Members holding logical data chunk `i`, P and Q of RAID6 row `stripe`
/// over `n` members: the kernel's `raid5_compute_sector`, case 6.
fn raid6_map(layout: u32, n: u64, stripe: u64, i: u64) -> (u64, u64, u64) {
    let data = n - 2;
    // P at `pd`, Q after it -- or on member 0 when P is last -- and data
    // in member order around them: "Q D D D P", "D D P Q D".
    let asymmetric = |pd: u64| {
        if pd == n - 1 {
            (i + 1, pd, 0)
        } else if i >= pd {
            (i + 2, pd, pd + 1)
        } else {
            (i, pd, pd + 1)
        }
    };
    match layout {
        LAYOUT_LEFT_ASYMMETRIC => asymmetric(n - 1 - stripe % n),
        // The same rows as left-asymmetric, one stripe on: "D D D P Q"
        // first rather than "Q D D D P".
        LAYOUT_ROTATING_N_RESTART => asymmetric(n - 1 - (stripe + 1) % n),
        LAYOUT_RIGHT_ASYMMETRIC | LAYOUT_ROTATING_ZERO_RESTART => asymmetric(stripe % n),
        LAYOUT_LEFT_SYMMETRIC => {
            let pd = n - 1 - stripe % n;
            ((pd + 2 + i) % n, pd, (pd + 1) % n)
        }
        LAYOUT_RIGHT_SYMMETRIC => {
            let pd = stripe % n;
            ((pd + 2 + i) % n, pd, (pd + 1) % n)
        }
        LAYOUT_PARITY_FIRST => (i + 2, 0, 1),
        LAYOUT_PARITY_LAST => (i, data, data + 1),
        // Left-symmetric with Q before P.
        LAYOUT_ROTATING_N_CONTINUE => {
            let pd = n - 1 - stripe % n;
            ((pd + 1 + i) % n, pd, (pd + n - 1) % n)
        }
        // A RAID5 layout over the first n - 1 members; Q on the last.
        LAYOUT_LEFT_ASYMMETRIC_6 | LAYOUT_RIGHT_ASYMMETRIC_6 => {
            let pd = if layout == LAYOUT_LEFT_ASYMMETRIC_6 {
                data - stripe % (n - 1)
            } else {
                stripe % (n - 1)
            };
            (if i >= pd { i + 1 } else { i }, pd, n - 1)
        }
        LAYOUT_LEFT_SYMMETRIC_6 | LAYOUT_RIGHT_SYMMETRIC_6 => {
            let pd = if layout == LAYOUT_LEFT_SYMMETRIC_6 {
                data - stripe % (n - 1)
            } else {
                stripe % (n - 1)
            };
            ((pd + 1 + i) % (n - 1), pd, n - 1)
        }
        LAYOUT_PARITY_FIRST_6 => (i + 1, 0, n - 1),
        other => unreachable!("RAID6 layout {other} is refused at assembly"),
    }
}

/// The power of `{02}` that multiplies logical data chunk `i` of RAID6
/// row `stripe` in Q: its slot in the kernel's `set_syndrome_sources`.
///
/// Outside the DDF layouts, data members are numbered in member order,
/// starting at the member after Q (`raid6_d0`, member 0 when Q is last)
/// and skipping P and Q. In the DDF layouts every member counts from
/// member 0, P and Q included, so a data chunk's number is its member's.
fn q_exponent(layout: u32, n: u64, stripe: u64, i: u64) -> i64 {
    let (dd, pd, qd) = raid6_map(layout, n, stripe, i);
    if (LAYOUT_ROTATING_ZERO_RESTART..=LAYOUT_ROTATING_N_CONTINUE).contains(&layout) {
        return dd as i64;
    }
    let d0 = if qd == n - 1 { 0 } else { qd + 1 };
    let mut slot = 0;
    let mut m = d0;
    while m != dd {
        if m != pd && m != qd {
            slot += 1;
        }
        m = (m + 1) % n;
    }
    slot
}

/// GF(2^8) over `x^8 + x^4 + x^3 + x^2 + 1`: `EXP[i]` is `{02}^i`, and
/// `LOG` its inverse.
const GF_EXP: [u8; 256] = gf_tables().0;
const GF_LOG: [u8; 256] = gf_tables().1;

const fn gf_tables() -> ([u8; 256], [u8; 256]) {
    let mut exp = [0u8; 256];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11d;
        }
        i += 1;
    }
    exp[255] = exp[0];
    (exp, log)
}

/// `{02}^e`, for any exponent: the group has order 255.
fn gf_pow2(e: i64) -> u8 {
    GF_EXP[e.rem_euclid(255) as usize]
}

fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        GF_EXP[(usize::from(GF_LOG[usize::from(a)]) + usize::from(GF_LOG[usize::from(b)])) % 255]
    }
}

/// The multiplicative inverse of a non-zero `a`.
fn gf_inv(a: u8) -> u8 {
    GF_EXP[(255 - usize::from(GF_LOG[usize::from(a)])) % 255]
}

struct Member<R> {
    dev: R,
    data_offset: u64,
    data_size: u64,
}

/// A run of a RAID0 array striped over the members that reach that far.
/// Members of equal size make one zone; members of different sizes make
/// one per distinct size, each over fewer members than the last.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Zone {
    /// Array byte where the zone starts.
    start: u64,
    /// Array bytes in the zone.
    len: u64,
    /// Byte, from each member's data offset, where the zone starts.
    dev_start: u64,
    /// The slots striped over, in slot order.
    slots: Vec<usize>,
}

/// The zones of a RAID0 over members whose usable sizes (already a
/// multiple of the chunk) are `sizes`, indexed by slot.
fn raid0_zones(sizes: &[u64]) -> Vec<Zone> {
    let mut ends: Vec<u64> = sizes.iter().copied().filter(|&s| s > 0).collect();
    ends.sort_unstable();
    ends.dedup();
    let mut zones = Vec::new();
    let (mut start, mut prev) = (0u64, 0u64);
    for end in ends {
        let slots: Vec<usize> = (0..sizes.len()).filter(|&s| sizes[s] >= end).collect();
        let len = (end - prev) * slots.len() as u64;
        zones.push(Zone {
            start,
            len,
            dev_start: prev,
            slots,
        });
        start += len;
        prev = end;
    }
    zones
}

/// A RAID10 layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Raid10 {
    /// Copies placed side by side on consecutive members.
    near: u64,
    /// Copies placed further down the members.
    far: u64,
    /// Far copies are one chunk row apart (`offset`) rather than one
    /// section of the member apart (`far`).
    offset: bool,
    /// Chunk rows in one far section.
    stride: u64,
}

impl Raid10 {
    /// The "near" layout: `near` copies side by side on consecutive
    /// members, and no far copy. What lvm2's `raid10` builds.
    pub(crate) fn near(near: u64) -> Self {
        Raid10 {
            near,
            far: 1,
            offset: false,
            stride: 0,
        }
    }

    /// Where array chunk `c`'s primary copy is, over `d` members:
    /// `copies(d, c)[0]`, without building the rest.
    pub(crate) fn primary(&self, d: u64, c: u64) -> (usize, u64) {
        let at = c * self.near;
        let row = at / d;
        let row = if self.offset { row * self.far } else { row };
        ((at % d) as usize, row)
    }

    /// Every place array chunk `c` is kept, primary copy first, as
    /// (slot, chunk row on that member), over `d` members.
    pub(crate) fn copies(&self, d: u64, c: u64) -> Vec<(usize, u64)> {
        let mut out = Vec::with_capacity((self.near * self.far) as usize);
        for i in 0..self.near {
            let at = c * self.near + i;
            let (dev, row) = (at % d, at / d);
            for k in 0..self.far {
                let slot = (dev + k * self.near) % d;
                let row = if self.offset {
                    row * self.far + k
                } else {
                    row + k * self.stride
                };
                out.push((slot as usize, row));
            }
        }
        out
    }
}

/// An assembled `md` array, read through its members.
pub struct MdArray<R: BlockRead> {
    /// Indexed by slot; `None` where a member is missing, stale or failed.
    slots: Vec<Option<Member<R>>>,
    level: i32,
    layout: u32,
    chunk: u64,
    size: u64,
    /// RAID0 only.
    zones: Vec<Zone>,
    /// RAID10 only.
    raid10: Option<Raid10>,
    superblock: MdSuperblock,
}

impl<R: BlockRead> fmt::Debug for MdArray<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MdArray")
            .field("level", &self.level)
            .field("layout", &self.layout)
            .field("chunk", &self.chunk)
            .field("size", &self.size)
            .field("present", &self.present())
            .finish()
    }
}

/// Devices sorted by the `md` array each is a member of: what [`scan`]
/// returns.
#[derive(Debug)]
#[non_exhaustive]
pub struct MdScan<R> {
    /// One entry per array found, in the order its first member was given.
    pub arrays: Vec<MdGroup<R>>,
    /// Devices that carry no `md` superblock.
    pub others: Vec<R>,
    /// Devices whose superblock is there but could not be read, with why.
    pub refused: Vec<(R, MdError)>,
}

/// The members of one array that [`scan`] found.
#[derive(Debug)]
#[non_exhaustive]
pub struct MdGroup<R> {
    /// The UUID every member carries.
    pub array_uuid: [u8; 16],
    /// The newest superblock among the members, which speaks for the array.
    pub superblock: MdSuperblock,
    /// Every member, in the order given.
    pub members: Vec<R>,
}

impl<R: BlockRead> MdGroup<R> {
    /// Assemble the array from its members.
    pub fn assemble(self) -> Result<MdArray<R>, MdError> {
        MdArray::assemble(self.members)
    }
}

/// Sort `devices` into the arrays they are members of.
///
/// Nothing is assembled and nothing is refused for being incomplete: a
/// group with too few members still comes back, and
/// [`MdGroup::assemble`] says whether the level can read it. A member
/// whose event count is behind stays in its group, since assembly sets
/// it aside itself. Errors in `refused` name a device by its position in
/// `devices`.
pub fn scan<R: BlockRead>(devices: Vec<R>) -> MdScan<R> {
    let mut found = MdScan {
        arrays: Vec::new(),
        others: Vec::new(),
        refused: Vec::new(),
    };
    for (member, dev) in devices.into_iter().enumerate() {
        match read_superblock_of(&dev, member) {
            Ok(None) => found.others.push(dev),
            Err(e) => found.refused.push((dev, e)),
            Ok(Some(sb)) => {
                match found
                    .arrays
                    .iter_mut()
                    .find(|g| g.array_uuid == sb.array_uuid)
                {
                    Some(group) => {
                        if sb.events > group.superblock.events {
                            group.superblock = sb;
                        }
                        group.members.push(dev);
                    }
                    None => found.arrays.push(MdGroup {
                        array_uuid: sb.array_uuid,
                        superblock: sb,
                        members: vec![dev],
                    }),
                }
            }
        }
    }
    found
}

impl<R: BlockRead> MdArray<R> {
    /// Assemble an array from its member devices, in any order.
    ///
    /// Every device must carry a superblock of the same array. Spares,
    /// failed members and members whose event count is behind the newest
    /// are set aside; the rest are placed in their slots, and the level
    /// decides whether enough are left.
    pub fn assemble(devices: Vec<R>) -> Result<Self, MdError> {
        if devices.is_empty() {
            return Err(MdError::Empty);
        }
        let mut found = Vec::with_capacity(devices.len());
        for (member, dev) in devices.into_iter().enumerate() {
            let sb = read_superblock_of(&dev, member)?.ok_or(MdError::NoSuperblock { member })?;
            found.push((member, dev, sb));
        }
        let first = found[0].2.clone();
        for (member, _, sb) in &found {
            if sb.array_uuid != first.array_uuid {
                return Err(MdError::MixedArrays { member: *member });
            }
        }
        let newest = found.iter().map(|(_, _, sb)| sb.events).max().unwrap_or(0);
        // The newest superblock speaks for the array.
        let lead = found
            .iter()
            .find(|(_, _, sb)| sb.events == newest)
            .map(|(_, _, sb)| sb.clone())
            .expect("a member has the newest event count");

        if lead.feature_map & FEATURE_RESHAPE_ACTIVE != 0 {
            return Err(MdError::Unsupported("the array is mid-reshape".into()));
        }
        let n = lead.raid_disks;
        if n == 0 || n > V1_MAX_DEV {
            return Err(corrupt(0, format!("raid_disks {n}")));
        }
        let level = lead.level;
        let mut slots: Vec<Option<Member<R>>> = (0..n).map(|_| None).collect();
        for (_, dev, sb) in found {
            let MdRole::Active(slot) = sb.role else {
                continue;
            };
            if sb.events < newest || sb.feature_map & FEATURE_RECOVERY_OFFSET != 0 {
                continue;
            }
            if slot >= n {
                continue;
            }
            let entry = &mut slots[slot as usize];
            if entry.is_some() {
                return Err(MdError::DuplicateRole { slot });
            }
            *entry = Some(Member {
                dev,
                data_offset: sb.data_offset,
                data_size: sb.data_size,
            });
        }
        let present = slots.iter().filter(|s| s.is_some()).count() as u32;

        let need = |needed: u32| {
            if present < needed {
                Err(MdError::TooFewMembers {
                    level,
                    present,
                    needed,
                })
            } else {
                Ok(())
            }
        };
        let chunk = lead.chunk_bytes;
        let needs_chunk = || {
            if chunk == 0 || chunk % SECTOR != 0 {
                Err(corrupt(0, format!("chunk size {chunk}")))
            } else {
                Ok(())
            }
        };
        let per_member = lead.component_size;
        let mut zones = Vec::new();
        let mut raid10 = None;
        let size = match level {
            LEVEL_RAID1 => {
                need(1)?;
                per_member
            }
            LEVEL_RAID0 => {
                need(n)?;
                needs_chunk()?;
                let sizes: Vec<u64> = slots
                    .iter()
                    .flatten()
                    .map(|m| m.data_size / chunk * chunk)
                    .collect();
                zones = raid0_zones(&sizes);
                // Which member a chunk of a later zone is on has been
                // computed two ways over the kernel's history, and the
                // superblock records which. Both are read; an array that
                // records neither is refused rather than guessed, as the
                // kernel refuses it.
                if zones.len() > 1
                    && lead.layout != RAID0_LAYOUT_ALTERNATE
                    && lead.layout != RAID0_LAYOUT_ORIGINAL
                {
                    return Err(MdError::Unsupported(format!(
                        "multi-zone RAID0 with layout {}",
                        lead.layout
                    )));
                }
                zones.iter().map(|z| z.len).sum()
            }
            LEVEL_RAID10 => {
                needs_chunk()?;
                if lead.layout & !(RAID10_OFFSET | 0xffff) != 0 {
                    return Err(MdError::Unsupported(format!(
                        "RAID10 layout {:#x}",
                        lead.layout
                    )));
                }
                let near = u64::from(lead.layout & 0xff);
                let far = u64::from((lead.layout >> 8) & 0xff);
                let d = u64::from(n);
                if near == 0 || far == 0 || near * far > d {
                    return Err(corrupt(
                        0,
                        format!("RAID10 layout {:#x} over {n} members", lead.layout),
                    ));
                }
                let dev_chunks = per_member / chunk;
                let geo = Raid10 {
                    near,
                    far,
                    offset: lead.layout & RAID10_OFFSET != 0,
                    stride: dev_chunks / far,
                };
                // The copies of chunk c fall on the same slots as those of
                // chunk c + d, so d chunks show every combination.
                for c in 0..d {
                    let copies = geo.copies(d, c);
                    if copies.iter().all(|&(s, _)| slots[s].is_none()) {
                        return Err(MdError::NoCopyLeft {
                            slots: copies.iter().map(|&(s, _)| s as u32).collect(),
                        });
                    }
                }
                raid10 = Some(geo);
                (dev_chunks / far * d / near)
                    .checked_mul(chunk)
                    .ok_or_else(|| corrupt(0, "array size overflows"))?
            }
            LEVEL_RAID4 | LEVEL_RAID5 | LEVEL_RAID6 => {
                needs_chunk()?;
                let parity = if level == LEVEL_RAID6 { 2 } else { 1 };
                if n <= parity {
                    return Err(corrupt(0, format!("RAID{level} with {n} members")));
                }
                if level == LEVEL_RAID6 && !raid6_layout_known(lead.layout) {
                    return Err(MdError::Unsupported(format!(
                        "RAID6 layout {}",
                        lead.layout
                    )));
                }
                if level == LEVEL_RAID5 && lead.layout > LAYOUT_PARITY_LAST {
                    return Err(MdError::Unsupported(format!(
                        "RAID5 layout {}",
                        lead.layout
                    )));
                }
                // One missing member is recovered through P; a second, on
                // a RAID6, through Q.
                need(n - parity)?;
                (per_member / chunk * chunk)
                    .checked_mul(u64::from(n - parity))
                    .ok_or_else(|| corrupt(0, "array size overflows"))?
            }
            other => {
                return Err(MdError::Unsupported(format!("RAID level {other}")));
            }
        };
        for m in slots.iter().flatten() {
            let span = match level {
                LEVEL_RAID1 => size,
                // Zones end within each member's own data size.
                LEVEL_RAID0 => 0,
                _ => per_member,
            };
            match m.data_offset.checked_add(span) {
                Some(end) if end <= m.dev.size_bytes() => {}
                _ => return Err(corrupt(0, "a member is shorter than the array needs")),
            }
        }
        Ok(MdArray {
            slots,
            level,
            layout: lead.layout,
            chunk,
            size,
            zones,
            raid10,
            superblock: lead,
        })
    }

    /// The superblock that describes the array: the newest one found.
    pub fn superblock(&self) -> &MdSuperblock {
        &self.superblock
    }

    /// How many member slots hold a usable member.
    pub fn present(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Whether every slot holds a usable member.
    pub fn is_degraded(&self) -> bool {
        self.present() < self.slots.len()
    }

    fn read_member(&self, slot: usize, off: u64, buf: &mut [u8]) -> fs_core::Result<bool> {
        match &self.slots[slot] {
            Some(m) => {
                m.dev.read_at(m.data_offset + off, buf)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn read_raid1(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let mut last = None;
        for m in self.slots.iter().flatten() {
            match m.dev.read_at(m.data_offset + offset, buf) {
                Ok(()) => return Ok(()),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| fs_core::Error::Custom("no RAID1 member present".into())))
    }

    /// Read RAID0 bytes from `pos` up to the end of its chunk or zone,
    /// whichever is nearer; returns how many were read.
    fn read_zoned(&self, pos: u64, buf: &mut [u8]) -> fs_core::Result<usize> {
        let z = self
            .zones
            .iter()
            .find(|z| pos >= z.start && pos < z.start + z.len)
            .expect("zones cover the array");
        let within = pos - z.start;
        let k = z.slots.len() as u64;
        let (c, inner) = (within / self.chunk, within % self.chunk);
        let take = ((self.chunk - inner) as usize).min(buf.len());
        let off = z.dev_start + (c / k) * self.chunk + inner;
        // The member is picked by the chunk's number within its zone, or,
        // in the original layout, within the whole array: the kernel's
        // `map_sector` is handed `orig_sector` there. The two agree in the
        // first zone, which starts at 0.
        let pick = if self.layout == RAID0_LAYOUT_ORIGINAL {
            pos / self.chunk
        } else {
            c
        };
        self.read_member(z.slots[(pick % k) as usize], off, &mut buf[..take])?;
        Ok(take)
    }

    fn read_chunk(&self, k: u64, within: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let striping = Striping {
            level: self.level,
            layout: self.layout,
            chunk: self.chunk,
            members: self.slots.len() as u64,
            raid10: self.raid10,
        };
        read_chunk(
            &striping,
            &|slot, off, b| self.read_member(slot, off, b),
            k,
            within,
            buf,
        )
    }
}

/// How a RAID4, 5, 6 or 10 array places its chunks over its members:
/// what [`read_chunk`] needs, whoever keeps the members.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Striping {
    /// md level: 4, 5, 6 or 10.
    pub(crate) level: i32,
    /// md layout (unused at level 10, which reads `raid10`).
    pub(crate) layout: u32,
    /// Chunk size in bytes.
    pub(crate) chunk: u64,
    /// Member count, present or not.
    pub(crate) members: u64,
    /// Level 10 only.
    pub(crate) raid10: Option<Raid10>,
}

/// Reads `buf.len()` bytes `off` into member `slot`'s data, or says
/// `false` without reading when that member is missing.
pub(crate) type ReadMember<'a> = dyn Fn(usize, u64, &mut [u8]) -> fs_core::Result<bool> + 'a;

/// Read `buf` from array chunk `k`, starting `within` bytes into it and
/// not past its end, through `member`. A chunk whose member is missing
/// is read from another RAID10 copy, or rebuilt from the rest of its row:
/// through P at RAID4 and RAID5, and through P, Q or both at RAID6.
pub(crate) fn read_chunk(
    s: &Striping,
    member: &ReadMember<'_>,
    k: u64,
    within: u64,
    buf: &mut [u8],
) -> fs_core::Result<()> {
    let n = s.members;
    if let Some(geo) = &s.raid10 {
        let mut last = None;
        // The primary copy first, without building the list of every
        // copy: it is the one present on any array that is not degraded.
        let (slot, row) = geo.primary(n, k);
        match member(slot, row * s.chunk + within, buf) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => last = Some(e),
        }
        for (slot, row) in geo.copies(n, k).into_iter().skip(1) {
            match member(slot, row * s.chunk + within, buf) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => last = Some(e),
            }
        }
        return Err(last.unwrap_or_else(|| {
            fs_core::Error::Custom(format!("md: no copy of chunk {k} is present"))
        }));
    }
    let parity = if s.level == LEVEL_RAID6 { 2 } else { 1 };
    let data = n - parity;
    let stripe = k / data;
    let layout = if s.level == LEVEL_RAID4 {
        LAYOUT_PARITY_LAST
    } else {
        s.layout
    };
    let (dd, _pd, qd) = parity_map(s.level, layout, n, stripe, k % data);
    let off = stripe * s.chunk + within;
    if member(dd, off, buf)? {
        return Ok(());
    }
    if s.level == LEVEL_RAID6 {
        return rebuild_raid6(s, member, stripe, k % data, off, buf);
    }
    // The data member is missing: XOR every other member in the row.
    buf.fill(0);
    let mut tmp = vec![0u8; buf.len()];
    for slot in 0..n as usize {
        if slot == dd || Some(slot) == qd {
            continue;
        }
        if !member(slot, off, &mut tmp)? {
            return Err(fs_core::Error::Custom(format!(
                "md: slot {slot} missing while reconstructing slot {dd}"
            )));
        }
        for (b, t) in buf.iter_mut().zip(&tmp) {
            *b ^= t;
        }
    }
    Ok(())
}

/// Rebuild data chunk `x` of RAID6 row `stripe`, whose member is
/// missing, from what the row still holds: through P when every other
/// data chunk is there, through Q when P is missing too, and through
/// both when a second data chunk is missing.
fn rebuild_raid6(
    s: &Striping,
    member: &ReadMember<'_>,
    stripe: u64,
    x: u64,
    off: u64,
    buf: &mut [u8],
) -> fs_core::Result<()> {
    let n = s.members;
    let layout = s.layout;
    let len = buf.len();
    let (_, pd, qd) = parity_map(LEVEL_RAID6, layout, n, stripe, 0);
    let qd = qd.expect("a RAID6 row has Q");
    // P and Q with every data chunk that is present taken back out,
    // leaving only the missing chunks' contributions.
    let mut p = vec![0u8; len];
    let mut q = vec![0u8; len];
    let have_p = member(pd, off, &mut p)?;
    let have_q = member(qd, off, &mut q)?;
    let mut missing = Vec::new();
    let mut d = vec![0u8; len];
    for i in 0..n - 2 {
        let (slot, _, _) = parity_map(LEVEL_RAID6, layout, n, stripe, i);
        if !member(slot, off, &mut d)? {
            missing.push(i);
            continue;
        }
        let g = gf_pow2(q_exponent(layout, n, stripe, i));
        for ((pb, qb), &db) in p.iter_mut().zip(q.iter_mut()).zip(&d) {
            *pb ^= db;
            *qb ^= gf_mul(g, db);
        }
    }
    let lost = || {
        fs_core::Error::Custom(format!(
            "md: RAID6 row {stripe} has lost data chunks {missing:?}, P present {have_p}, Q present {have_q}"
        ))
    };
    match missing.as_slice() {
        [_] if have_p => buf.copy_from_slice(&p),
        [_] if have_q => {
            // Q' = {02}^ex . D_x, ex being D_x's number in Q.
            let inv = gf_pow2(-q_exponent(layout, n, stripe, x));
            for (b, &qb) in buf.iter_mut().zip(&q) {
                *b = gf_mul(inv, qb);
            }
        }
        &[a, b] if have_p && have_q => {
            let y = if a == x { b } else { a };
            let ex = q_exponent(layout, n, stripe, x);
            let ey = q_exponent(layout, n, stripe, y);
            // P' = D_x + D_y and Q' = g^ex D_x + g^ey D_y, so
            // D_x = A P' + B Q' with A = g^(ey-ex) / (g^(ey-ex) + 1)
            // and B = g^(-ex) / (g^(ey-ex) + 1).
            let gyx = gf_pow2(ey - ex);
            if gyx ^ 1 == 0 {
                return Err(lost());
            }
            let denom = gf_inv(gyx ^ 1);
            let ca = gf_mul(gyx, denom);
            let cb = gf_mul(gf_pow2(-ex), denom);
            for ((out, &pb), &qb) in buf.iter_mut().zip(&p).zip(&q) {
                *out = gf_mul(ca, pb) ^ gf_mul(cb, qb);
            }
        }
        _ => return Err(lost()),
    }
    Ok(())
}

impl<R: BlockRead> BlockRead for MdArray<R> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let len = buf.len() as u64;
        match offset.checked_add(len) {
            Some(end) if end <= self.size => {}
            _ => {
                return Err(fs_core::Error::OutOfBounds {
                    offset,
                    len,
                    size: self.size,
                })
            }
        }
        if self.level == LEVEL_RAID1 {
            return self.read_raid1(offset, buf);
        }
        let mut done = 0usize;
        if self.level == LEVEL_RAID0 {
            while done < buf.len() {
                done += self.read_zoned(offset + done as u64, &mut buf[done..])?;
            }
            return Ok(());
        }
        while done < buf.len() {
            let pos = offset + done as u64;
            let k = pos / self.chunk;
            let within = pos % self.chunk;
            let take = ((self.chunk - within) as usize).min(buf.len() - done);
            self.read_chunk(k, within, &mut buf[done..done + take])?;
            done += take;
        }
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.size
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! Self-consistency only: superblocks and stripes built here, read
    //! back here. Whether these layouts are the kernel's is
    //! `tests/oracle_md.rs`'s question, not this module's.
    use super::*;

    pub(crate) struct Mem(pub(crate) Vec<u8>);
    impl BlockRead for Mem {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let o = offset as usize;
            if o + buf.len() > self.0.len() {
                return Err(fs_core::Error::OutOfBounds {
                    offset,
                    len: buf.len() as u64,
                    size: self.0.len() as u64,
                });
            }
            buf.copy_from_slice(&self.0[o..o + buf.len()]);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
    }

    pub(crate) const MEMBER: usize = 1 << 20;
    pub(crate) const DATA_OFFSET: u64 = 64 * 1024;
    const CHUNK: u64 = 16 * 1024;

    /// A 1.2 superblock for slot `slot` of an `n`-member array.
    pub(crate) fn sb_v12(level: i32, layout: u32, n: u32, slot: u16, events: u64) -> Vec<u8> {
        sb_v12_sized(
            level,
            layout,
            n,
            slot,
            events,
            (MEMBER as u64 - DATA_OFFSET) / SECTOR,
        )
    }

    /// [`sb_v12`] for a member whose data area is `data_size` sectors.
    fn sb_v12_sized(
        level: i32,
        layout: u32,
        n: u32,
        slot: u16,
        events: u64,
        data_size: u64,
    ) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        let put32 =
            |b: &mut [u8], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let put64 =
            |b: &mut [u8], at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        put32(&mut b, 0, MD_MAGIC);
        put32(&mut b, 4, 1);
        b[16..32].copy_from_slice(&[0xAB; 16]);
        b[32..38].copy_from_slice(b"host:0");
        put32(&mut b, 72, level as u32);
        put32(&mut b, 76, layout);
        put64(&mut b, 80, data_size / (CHUNK / SECTOR) * (CHUNK / SECTOR));
        put32(&mut b, 88, (CHUNK / SECTOR) as u32);
        put32(&mut b, 92, n);
        put64(&mut b, 128, DATA_OFFSET / SECTOR);
        put64(&mut b, 136, data_size);
        put64(&mut b, 144, 8);
        put32(&mut b, 160, u32::from(slot));
        put64(&mut b, 200, events);
        put32(&mut b, 220, n);
        for s in 0..n as usize {
            b[256 + 2 * s..258 + 2 * s].copy_from_slice(&(s as u16).to_le_bytes());
        }
        let len = 256 + 2 * n as usize;
        let c = v1_checksum(&b[..len]);
        put32(&mut b, 216, c);
        b
    }

    /// A pseudo-random logical array of `len` bytes.
    fn pattern(len: usize) -> Vec<u8> {
        let mut x: u32 = 0x1234_5678;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    /// Lay `logical` out over `n` members the way `parity_map` says.
    fn build(level: i32, layout: u32, n: u32, logical: &[u8]) -> Vec<Vec<u8>> {
        let mut members: Vec<Vec<u8>> = (0..n)
            .map(|s| {
                let mut m = vec![0u8; MEMBER];
                m[4096..8192].copy_from_slice(&sb_v12(level, layout, n, s as u16, 7));
                m
            })
            .collect();
        let chunk = CHUNK as usize;
        let nn = u64::from(n);
        let parity: u64 = match level {
            6 => 2,
            4 | 5 => 1,
            _ => 0,
        };
        let data = nn - parity;
        for k in 0..(logical.len() / chunk) as u64 {
            let src = &logical[k as usize * chunk..(k as usize + 1) * chunk];
            let (slot, off) = if level == 0 {
                ((k % nn) as usize, (k / nn) * CHUNK)
            } else {
                let lay = if level == 4 {
                    LAYOUT_PARITY_LAST
                } else {
                    layout
                };
                let (dd, pd, qd) = parity_map(level, lay, nn, k / data, k % data);
                let off = (k / data) * CHUNK;
                let p0 = DATA_OFFSET as usize + off as usize;
                for (i, b) in src.iter().enumerate() {
                    members[pd][p0 + i] ^= b;
                }
                if let Some(qd) = qd {
                    let g = gf_pow2(q_exponent(lay, nn, k / data, k % data));
                    for (i, &b) in src.iter().enumerate() {
                        members[qd][p0 + i] ^= gf_mul(g, b);
                    }
                }
                (dd, off)
            };
            let o = DATA_OFFSET as usize + off as usize;
            members[slot][o..o + chunk].copy_from_slice(src);
        }
        members
    }

    fn check(level: i32, layout: u32, n: u32, drop: Option<usize>) {
        check_without(level, layout, n, drop.as_slice());
    }

    fn check_without(level: i32, layout: u32, n: u32, drop: &[usize]) {
        let parity = match level {
            6 => 2,
            4 | 5 => 1,
            _ => 0,
        };
        let per = (MEMBER as u64 - DATA_OFFSET) / CHUNK * CHUNK;
        let logical = pattern((per * u64::from(n - parity)) as usize);
        let members = build(level, layout, n, &logical);
        let devs: Vec<Mem> = members
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !drop.contains(i))
            .map(|(_, m)| Mem(m))
            .collect();
        let a = MdArray::assemble(devs).expect("assembles");
        assert_eq!(a.size_bytes(), logical.len() as u64);
        let mut got = vec![0u8; logical.len()];
        a.read_at(0, &mut got).unwrap();
        assert!(got == logical, "RAID{level} layout {layout} drop {drop:?}");
        // An unaligned read across chunk boundaries.
        let mut part = vec![0u8; 3 * CHUNK as usize + 17];
        a.read_at(CHUNK - 5, &mut part).unwrap();
        assert_eq!(part[..], logical[(CHUNK - 5) as usize..][..part.len()]);
    }

    #[test]
    fn raid0_reads_back_its_stripes() {
        check(0, 0, 3, None);
    }

    #[test]
    fn raid5_every_layout_reads_back_healthy_and_with_each_member_missing() {
        for layout in 0..=5 {
            check(5, layout, 4, None);
            for drop in 0..4 {
                check(5, layout, 4, Some(drop));
            }
        }
    }

    #[test]
    fn raid4_reads_back_with_one_member_missing_and_raid6_with_any_two() {
        check(4, 0, 3, None);
        check(4, 0, 3, Some(0));
        check(6, LAYOUT_LEFT_SYMMETRIC, 5, None);
        for a in 0..5 {
            check(6, LAYOUT_LEFT_SYMMETRIC, 5, Some(a));
            for b in a + 1..5 {
                check_without(6, LAYOUT_LEFT_SYMMETRIC, 5, &[a, b]);
            }
        }
    }

    /// Every RAID6 layout the kernel has, on four and five members,
    /// healthy and with any two missing. Self-consistency only:
    /// `tests/oracle_md.rs` holds each against an array `mdadm` built.
    #[test]
    fn raid6_every_layout_reads_back_with_any_two_missing() {
        for layout in [
            LAYOUT_LEFT_ASYMMETRIC,
            LAYOUT_RIGHT_ASYMMETRIC,
            LAYOUT_LEFT_SYMMETRIC,
            LAYOUT_RIGHT_SYMMETRIC,
            LAYOUT_PARITY_FIRST,
            LAYOUT_PARITY_LAST,
            LAYOUT_ROTATING_ZERO_RESTART,
            LAYOUT_ROTATING_N_RESTART,
            LAYOUT_ROTATING_N_CONTINUE,
            LAYOUT_LEFT_ASYMMETRIC_6,
            LAYOUT_RIGHT_ASYMMETRIC_6,
            LAYOUT_LEFT_SYMMETRIC_6,
            LAYOUT_RIGHT_SYMMETRIC_6,
            LAYOUT_PARITY_FIRST_6,
        ] {
            for n in [4u32, 5] {
                check(6, layout, n, None);
                for a in 0..n as usize {
                    for b in a + 1..n as usize {
                        check_without(6, layout, n, &[a, b]);
                    }
                }
            }
        }
    }

    /// Where the kernel puts each chunk, P and Q, transcribed from
    /// `raid5_compute_sector` for a few rows of five members, so a slip in
    /// the map cannot pass by agreeing with a builder that shares it.
    #[test]
    fn raid6_rows_are_where_the_kernel_puts_them() {
        // (layout, stripe) -> (data members in order, P, Q).
        let rows: &[(u32, u64, [usize; 3], usize, usize)] = &[
            (LAYOUT_LEFT_ASYMMETRIC, 0, [1, 2, 3], 4, 0),
            (LAYOUT_LEFT_ASYMMETRIC, 1, [0, 1, 2], 3, 4),
            (LAYOUT_LEFT_ASYMMETRIC, 2, [0, 1, 4], 2, 3),
            (LAYOUT_RIGHT_ASYMMETRIC, 0, [2, 3, 4], 0, 1),
            (LAYOUT_RIGHT_ASYMMETRIC, 4, [1, 2, 3], 4, 0),
            (LAYOUT_LEFT_SYMMETRIC, 0, [1, 2, 3], 4, 0),
            (LAYOUT_LEFT_SYMMETRIC, 1, [0, 1, 2], 3, 4),
            (LAYOUT_RIGHT_SYMMETRIC, 1, [3, 4, 0], 1, 2),
            (LAYOUT_PARITY_FIRST, 3, [2, 3, 4], 0, 1),
            (LAYOUT_PARITY_LAST, 3, [0, 1, 2], 3, 4),
            (LAYOUT_ROTATING_N_RESTART, 0, [0, 1, 2], 3, 4),
            (LAYOUT_ROTATING_N_CONTINUE, 0, [0, 1, 2], 4, 3),
            (LAYOUT_ROTATING_N_CONTINUE, 1, [4, 0, 1], 3, 2),
            (LAYOUT_LEFT_ASYMMETRIC_6, 0, [0, 1, 2], 3, 4),
            (LAYOUT_LEFT_ASYMMETRIC_6, 1, [0, 1, 3], 2, 4),
            (LAYOUT_RIGHT_SYMMETRIC_6, 1, [2, 3, 0], 1, 4),
            (LAYOUT_LEFT_SYMMETRIC_6, 1, [3, 0, 1], 2, 4),
            (LAYOUT_PARITY_FIRST_6, 2, [1, 2, 3], 0, 4),
        ];
        for &(layout, stripe, data, p, q) in rows {
            for (i, &d) in data.iter().enumerate() {
                assert_eq!(
                    parity_map(6, layout, 5, stripe, i as u64),
                    (d, p, Some(q)),
                    "layout {layout} stripe {stripe} chunk {i}"
                );
            }
        }
    }

    /// Q's coefficients, from `set_syndrome_sources`: outside the DDF
    /// layouts, data members are numbered in member order starting after
    /// Q; in them, every member counts, P and Q included.
    #[test]
    fn q_numbers_data_the_way_the_kernel_does() {
        // Left-asymmetric, row 2 of five: D0 D1 P Q D2. The walk starts
        // after Q, at member 4, so D2 is first.
        let e: Vec<i64> = (0..3)
            .map(|i| q_exponent(LAYOUT_LEFT_ASYMMETRIC, 5, 2, i))
            .collect();
        assert_eq!(e, [1, 2, 0]);
        // Left-symmetric, any row: data follows Q in logical order.
        let e: Vec<i64> = (0..3)
            .map(|i| q_exponent(LAYOUT_LEFT_SYMMETRIC, 5, 2, i))
            .collect();
        assert_eq!(e, [0, 1, 2]);
        // DDF N-continue, row 1: D1 D2 Q P D0 -- numbered by member.
        let e: Vec<i64> = (0..3)
            .map(|i| q_exponent(LAYOUT_ROTATING_N_CONTINUE, 5, 1, i))
            .collect();
        assert_eq!(e, [4, 0, 1]);
        // Left-symmetric-6, row 1: D1 D2 P D0 Q. Q is last, so the walk
        // starts at member 0.
        let e: Vec<i64> = (0..3)
            .map(|i| q_exponent(LAYOUT_LEFT_SYMMETRIC_6, 5, 1, i))
            .collect();
        assert_eq!(e, [2, 0, 1]);
    }

    #[test]
    fn the_field_is_the_one_the_raid6_paper_uses() {
        // {02}^8 reduces by 0x11d to 0x1d, and every non-zero element
        // has an inverse.
        assert_eq!(gf_pow2(8), 0x1d);
        assert_eq!(gf_pow2(255), 1);
        assert_eq!(gf_mul(0x80, 2), 0x1d);
        for a in 1..=255u8 {
            assert_eq!(gf_mul(a, gf_inv(a)), 1, "{a:#x}");
        }
    }

    #[test]
    fn a_bad_checksum_is_refused() {
        let mut m = vec![0u8; MEMBER];
        m[4096..8192].copy_from_slice(&sb_v12(1, 0, 2, 0, 1));
        m[4096 + 72] ^= 1;
        assert!(matches!(
            read_superblock(&Mem(m)),
            Err(MdError::BadChecksum { .. })
        ));
    }

    #[test]
    fn members_of_two_arrays_are_refused() {
        let mut a = vec![0u8; MEMBER];
        a[4096..8192].copy_from_slice(&sb_v12(1, 0, 2, 0, 1));
        let mut b = vec![0u8; MEMBER];
        let mut sb = sb_v12(1, 0, 2, 1, 1);
        sb[16] ^= 1;
        let c = v1_checksum(&sb[..260]);
        sb[216..220].copy_from_slice(&c.to_le_bytes());
        b[4096..8192].copy_from_slice(&sb);
        assert!(matches!(
            MdArray::assemble(vec![Mem(a), Mem(b)]),
            Err(MdError::MixedArrays { member: 1 })
        ));
    }

    #[test]
    fn a_stale_member_is_set_aside() {
        let mut a = vec![0u8; MEMBER];
        a[4096..8192].copy_from_slice(&sb_v12(1, 0, 2, 0, 9));
        a[DATA_OFFSET as usize] = 0xAA;
        let mut b = vec![0u8; MEMBER];
        b[4096..8192].copy_from_slice(&sb_v12(1, 0, 2, 1, 8));
        b[DATA_OFFSET as usize] = 0xBB;
        let arr = MdArray::assemble(vec![Mem(b), Mem(a)]).unwrap();
        assert_eq!(arr.present(), 1);
        let mut one = [0u8; 1];
        arr.read_at(0, &mut one).unwrap();
        assert_eq!(one[0], 0xAA, "the newer member is read");
    }

    #[test]
    fn raid5_with_two_missing_and_raid6_with_three_missing_are_refused() {
        let per = (MEMBER as u64 - DATA_OFFSET) / CHUNK * CHUNK;
        let members = build(5, 2, 4, &pattern((per * 3) as usize));
        let two: Vec<Mem> = members.into_iter().take(2).map(Mem).collect();
        assert!(matches!(
            MdArray::assemble(two),
            Err(MdError::TooFewMembers { .. })
        ));
        let members = build(6, 2, 5, &pattern((per * 3) as usize));
        let two: Vec<Mem> = members.into_iter().take(2).map(Mem).collect();
        assert!(matches!(
            MdArray::assemble(two),
            Err(MdError::TooFewMembers { .. })
        ));
    }

    #[test]
    fn a_device_without_a_superblock_is_not_a_member() {
        assert!(read_superblock(&Mem(vec![0u8; MEMBER])).unwrap().is_none());
        assert!(matches!(
            MdArray::assemble(vec![Mem(vec![0u8; MEMBER])]),
            Err(MdError::NoSuperblock { member: 0 })
        ));
    }

    #[test]
    fn reads_past_the_end_are_refused() {
        let per = (MEMBER as u64 - DATA_OFFSET) / CHUNK * CHUNK;
        let members = build(0, 0, 2, &pattern((per * 2) as usize));
        let a = MdArray::assemble(members.into_iter().map(Mem).collect()).unwrap();
        let mut b = [0u8; 2];
        assert!(a.read_at(a.size_bytes() - 1, &mut b).is_err());
        assert!(a.read_at(u64::MAX, &mut b).is_err());
    }

    #[test]
    fn raid10_copies_land_where_the_documented_layouts_put_them() {
        // near=2 over 3 members: copies side by side, wrapping into the
        // next row.
        let near = Raid10 {
            near: 2,
            far: 1,
            offset: false,
            stride: 0,
        };
        assert_eq!(near.copies(3, 0), [(0, 0), (1, 0)]);
        assert_eq!(near.copies(3, 1), [(2, 0), (0, 1)]);
        assert_eq!(near.copies(3, 2), [(1, 1), (2, 1)]);
        // far=2 over 4 members: a RAID0 over the first half of each
        // member, then the same again in the second half, one member on.
        let far = Raid10 {
            near: 1,
            far: 2,
            offset: false,
            stride: 50,
        };
        assert_eq!(far.copies(4, 0), [(0, 0), (1, 50)]);
        assert_eq!(far.copies(4, 3), [(3, 0), (0, 50)]);
        assert_eq!(far.copies(4, 4), [(0, 1), (1, 51)]);
        // offset=2 over 3 members: each row repeated in the next, one
        // member on.
        let offset = Raid10 {
            near: 1,
            far: 2,
            offset: true,
            stride: 0,
        };
        assert_eq!(offset.copies(3, 0), [(0, 0), (1, 1)]);
        assert_eq!(offset.copies(3, 2), [(2, 0), (0, 1)]);
        assert_eq!(offset.copies(3, 3), [(0, 2), (1, 3)]);
    }

    /// Members of an `n`-member RAID10 in `layout`, laid out by
    /// `Raid10::copies`, and the logical array they hold.
    fn build_raid10(layout: u32, n: u32) -> (Vec<Vec<u8>>, Vec<u8>) {
        let near = u64::from(layout & 0xff);
        let far = u64::from((layout >> 8) & 0xff);
        let dev_chunks = (MEMBER as u64 - DATA_OFFSET) / CHUNK;
        let geo = Raid10 {
            near,
            far,
            offset: layout & RAID10_OFFSET != 0,
            stride: dev_chunks / far,
        };
        let chunks = dev_chunks / far * u64::from(n) / near;
        let logical = pattern((chunks * CHUNK) as usize);
        let mut members: Vec<Vec<u8>> = (0..n)
            .map(|s| {
                let mut m = vec![0u8; MEMBER];
                m[4096..8192].copy_from_slice(&sb_v12(10, layout, n, s as u16, 7));
                m
            })
            .collect();
        let c = CHUNK as usize;
        for k in 0..chunks {
            let src = &logical[k as usize * c..][..c];
            for (slot, row) in geo.copies(u64::from(n), k) {
                let o = (DATA_OFFSET + row * CHUNK) as usize;
                members[slot][o..o + c].copy_from_slice(src);
            }
        }
        (members, logical)
    }

    #[test]
    fn raid10_reads_back_with_any_one_member_missing_and_refuses_a_lost_pair() {
        for (layout, n) in [(0x102, 4), (0x102, 3), (0x201, 4), (0x1_0201, 3)] {
            let (members, logical) = build_raid10(layout, n);
            for drop in [None, Some(0), Some(1), Some(n as usize - 1)] {
                let devs: Vec<Mem> = members
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| Some(*i) != drop)
                    .map(|(_, m)| Mem(m.clone()))
                    .collect();
                let a = MdArray::assemble(devs).expect("assembles");
                assert_eq!(a.size_bytes(), logical.len() as u64);
                let mut got = vec![0u8; logical.len()];
                a.read_at(0, &mut got).unwrap();
                assert!(got == logical, "layout {layout:#x} n {n} drop {drop:?}");
                let mut part = vec![0u8; 2 * CHUNK as usize + 9];
                a.read_at(CHUNK - 3, &mut part).unwrap();
                assert_eq!(part[..], logical[(CHUNK - 3) as usize..][..part.len()]);
            }
        }
        // near=2 over 4: slots 0 and 1 hold the only copies of chunk 0.
        let (members, _) = build_raid10(0x102, 4);
        let devs: Vec<Mem> = members.into_iter().skip(2).map(Mem).collect();
        assert!(matches!(
            MdArray::assemble(devs),
            Err(MdError::NoCopyLeft { .. })
        ));
    }

    #[test]
    fn raid0_zones_follow_the_member_sizes() {
        let c = CHUNK;
        let z = raid0_zones(&[3 * c, c, 2 * c]);
        assert_eq!(z.len(), 3);
        assert_eq!((z[0].start, z[0].len, z[0].dev_start), (0, 3 * c, 0));
        assert_eq!(z[0].slots, [0, 1, 2]);
        assert_eq!((z[1].start, z[1].len, z[1].dev_start), (3 * c, 2 * c, c));
        assert_eq!(z[1].slots, [0, 2]);
        assert_eq!((z[2].start, z[2].len, z[2].dev_start), (5 * c, c, 2 * c));
        assert_eq!(z[2].slots, [0]);
        assert_eq!(raid0_zones(&[2 * c, 2 * c]).len(), 1);
    }

    #[test]
    fn multi_zone_raid0_reads_back_in_both_recorded_layouts() {
        // Members of 40, 16 and 24 chunks: zones of 3x16, 2x8 and 1x16.
        let chunks = [40u64, 16, 24];
        let build = |layout: u32| -> (Vec<Mem>, Vec<u8>) {
            let sizes: Vec<u64> = chunks.iter().map(|&k| k * CHUNK).collect();
            let zones = raid0_zones(&sizes);
            let total: u64 = zones.iter().map(|z| z.len).sum();
            let logical = pattern(total as usize);
            let mut members: Vec<Vec<u8>> = (0..3)
                .map(|s| {
                    let mut m = vec![0u8; MEMBER];
                    let sb = sb_v12_sized(0, layout, 3, s as u16, 1, sizes[s] / SECTOR);
                    m[4096..8192].copy_from_slice(&sb);
                    m
                })
                .collect();
            // Written zone by zone: chunk i of a zone goes to row i / k of
            // the zone. The alternate layout picks the zone's (i mod k)-th
            // member; the original picks by the chunk's number in the
            // whole array instead (`map_sector` handed `orig_sector`).
            let c = CHUNK as usize;
            for z in &zones {
                let k = z.slots.len();
                for i in 0..(z.len / CHUNK) as usize {
                    let src = &logical[z.start as usize + i * c..][..c];
                    let o = DATA_OFFSET as usize + z.dev_start as usize + (i / k) * c;
                    let pick = if layout == RAID0_LAYOUT_ORIGINAL {
                        (z.start as usize / c + i) % k
                    } else {
                        i % k
                    };
                    members[z.slots[pick]][o..o + c].copy_from_slice(src);
                }
            }
            (members.into_iter().map(Mem).collect(), logical)
        };
        for layout in [RAID0_LAYOUT_ALTERNATE, RAID0_LAYOUT_ORIGINAL] {
            let (devs, logical) = build(layout);
            let a = MdArray::assemble(devs).expect("assembles");
            assert_eq!(a.size_bytes(), 80 * CHUNK);
            let mut got = vec![0u8; logical.len()];
            a.read_at(0, &mut got).unwrap();
            assert!(got == logical, "layout {layout}");
            // Across the boundary between the first and second zones.
            let mut part = vec![0u8; 3 * CHUNK as usize];
            a.read_at(47 * CHUNK + 5, &mut part).unwrap();
            assert_eq!(part[..], logical[(47 * CHUNK + 5) as usize..][..part.len()]);
        }
        // With no layout recorded the kernel refuses to guess, and so
        // does this.
        let (devs, _) = build(0);
        assert!(matches!(
            MdArray::assemble(devs),
            Err(MdError::Unsupported(_))
        ));
    }

    /// A RAID1 member in `slot` of array `uuid_byte`, at event `events`.
    pub(crate) fn raid1_member(uuid_byte: u8, slot: u16, events: u64, fill: u8) -> Mem {
        let mut m = vec![0u8; MEMBER];
        let mut sb = sb_v12(1, 0, 2, slot, events);
        sb[16..32].copy_from_slice(&[uuid_byte; 16]);
        let c = v1_checksum(&sb[..260]);
        sb[216..220].copy_from_slice(&c.to_le_bytes());
        m[4096..8192].copy_from_slice(&sb);
        m[DATA_OFFSET as usize..].fill(fill);
        Mem(m)
    }

    #[test]
    fn a_scan_sorts_devices_into_their_arrays() {
        let devices = vec![
            raid1_member(1, 0, 5, 0x11),
            Mem(vec![0u8; MEMBER]),
            raid1_member(2, 1, 9, 0x22),
            raid1_member(1, 1, 5, 0x11),
            raid1_member(2, 0, 9, 0x22),
        ];
        let found = scan(devices);
        assert_eq!(found.arrays.len(), 2, "two arrays");
        assert_eq!(found.others.len(), 1, "the blank device is not a member");
        assert!(found.refused.is_empty());
        // In the order each array's first member was given.
        assert_eq!(found.arrays[0].array_uuid, [1; 16]);
        assert_eq!(found.arrays[1].array_uuid, [2; 16]);
        assert_eq!(found.arrays[1].superblock.events, 9);
        for (group, fill) in found.arrays.into_iter().zip([0x11u8, 0x22]) {
            assert_eq!(group.members.len(), 2);
            let a = group.assemble().expect("each group assembles");
            let mut b = [0u8; 4];
            a.read_at(0, &mut b).unwrap();
            assert_eq!(b, [fill; 4]);
        }
    }

    #[test]
    fn a_stale_member_is_grouped_and_set_aside_at_assembly() {
        // Slot 1 missed the last write: its event count is behind, and
        // its data is old.
        let found = scan(vec![
            raid1_member(1, 1, 4, 0xEE),
            raid1_member(1, 0, 5, 0x11),
        ]);
        assert_eq!(found.arrays.len(), 1);
        let group = found.arrays.into_iter().next().unwrap();
        assert_eq!(group.members.len(), 2, "a stale member is still a member");
        assert_eq!(group.superblock.events, 5, "the newest superblock speaks");
        let a = group.assemble().unwrap();
        assert!(a.is_degraded(), "the stale member is not read");
        let mut b = [0u8; 4];
        a.read_at(0, &mut b).unwrap();
        assert_eq!(
            b, [0x11; 4],
            "the current member's data, not the stale one's"
        );
    }

    #[test]
    fn a_damaged_superblock_is_refused_by_name_not_dropped() {
        let mut bad = raid1_member(1, 0, 5, 0);
        bad.0[4096 + 72] ^= 1;
        let found = scan(vec![bad, raid1_member(1, 1, 5, 0)]);
        assert_eq!(found.arrays.len(), 1);
        assert_eq!(found.arrays[0].members.len(), 1);
        assert_eq!(found.refused.len(), 1);
        assert!(matches!(found.refused[0].1, MdError::BadChecksum { .. }));
    }
    /// `primary` is the first of `copies`, for near, far and offset
    /// layouts over member counts that do and do not divide the copies.
    #[test]
    fn a_raid10_chunks_primary_copy_is_the_first_of_its_copies() {
        for d in 2..=7u64 {
            for (near, far, offset) in [(1, 2, false), (2, 1, false), (2, 2, true), (3, 1, false)] {
                if near * far > d {
                    continue;
                }
                let geo = Raid10 {
                    near,
                    far,
                    offset,
                    stride: 11,
                };
                for c in 0..40 {
                    assert_eq!(
                        geo.primary(d, c),
                        geo.copies(d, c)[0],
                        "{geo:?} d={d} c={c}"
                    );
                }
            }
        }
        assert_eq!(Raid10::near(2).copies(3, 1), vec![(2, 0), (0, 1)]);
    }
    /// A linear array: its members' data areas end to end, in slot
    /// order, each rounded down to the chunk ("rounding") when the
    /// superblock records one, as the kernel's `linear_conf` sizes them.
    #[test]
    fn a_linear_array_reads_its_members_end_to_end() {
        // Data areas of 40, 16 and 24 KiB plus 1 KiB, so rounding to a
        // 16 KiB chunk drops a tail from every member.
        let kib = 1024u64;
        let sizes = [41 * kib, 17 * kib, 25 * kib];
        for chunk_sectors in [0u64, CHUNK / SECTOR] {
            let used: Vec<u64> = sizes
                .iter()
                .map(|&s| match chunk_sectors * SECTOR {
                    0 => s,
                    c => s / c * c,
                })
                .collect();
            let logical = pattern(used.iter().sum::<u64>() as usize);
            let mut at = 0usize;
            let mut members = Vec::new();
            for (slot, (&size, &take)) in sizes.iter().zip(&used).enumerate() {
                let mut m = vec![0u8; MEMBER];
                let mut sb = sb_v12_sized(-1, 0, 3, slot as u16, 1, size / SECTOR);
                sb[88..92].copy_from_slice(&(chunk_sectors as u32).to_le_bytes());
                let c = v1_checksum(&sb[..256 + 2 * 3]);
                sb[216..220].copy_from_slice(&c.to_le_bytes());
                m[4096..8192].copy_from_slice(&sb);
                let o = DATA_OFFSET as usize;
                m[o..o + take as usize].copy_from_slice(&logical[at..at + take as usize]);
                at += take as usize;
                members.push(Mem(m));
            }
            // Any order: each member's slot is in its superblock.
            members.reverse();
            let a = MdArray::assemble(members).expect("assembles");
            assert_eq!(
                a.size_bytes(),
                logical.len() as u64,
                "chunk {chunk_sectors}"
            );
            let mut got = vec![0u8; logical.len()];
            a.read_at(0, &mut got).unwrap();
            assert_eq!(got, logical, "chunk {chunk_sectors}");
            // Across the first member's end into the second.
            let mut part = vec![0u8; 300];
            a.read_at(used[0] - 100, &mut part).unwrap();
            assert_eq!(part[..], logical[used[0] as usize - 100..][..300]);
        }
    }
}
