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
//! | RAID0 | equal-sized members (one zone) | none |
//! | RAID1 | — | all but one |
//! | RAID4 | parity on the last member | one |
//! | RAID5 | left/right, symmetric/asymmetric, parity-first, parity-last | one |
//! | RAID6 | left-symmetric (the `mdadm` default) | one |
//!
//! Anything else is refused by name with [`MdError::Unsupported`]: RAID10,
//! linear arrays, RAID0 over members of different sizes, an array in the
//! middle of a reshape, and a RAID6 with two members missing (which needs
//! the Q syndrome rather than P). Refusing is the point: a layout read with
//! the wrong geometry returns plausible bytes from the wrong places.
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

/// RAID5/6 parity layouts, by the number the superblock stores.
const LAYOUT_LEFT_ASYMMETRIC: u32 = 0;
const LAYOUT_RIGHT_ASYMMETRIC: u32 = 1;
const LAYOUT_LEFT_SYMMETRIC: u32 = 2;
const LAYOUT_RIGHT_SYMMETRIC: u32 = 3;
const LAYOUT_PARITY_FIRST: u32 = 4;
const LAYOUT_PARITY_LAST: u32 = 5;

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
fn parity_map(
    level: i32,
    layout: u32,
    n: u64,
    stripe: u64,
    i: u64,
) -> (usize, usize, Option<usize>) {
    if level == LEVEL_RAID6 {
        // Left-symmetric: P rotates leftwards from the last member, Q
        // follows it, and data starts after Q.
        let pd = n - 1 - stripe % n;
        let qd = (pd + 1) % n;
        let dd = (pd + 2 + i) % n;
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

struct Member<R> {
    dev: R,
    data_offset: u64,
}

/// An assembled `md` array, read through its members.
pub struct MdArray<R: BlockRead> {
    /// Indexed by slot; `None` where a member is missing, stale or failed.
    slots: Vec<Option<Member<R>>>,
    level: i32,
    layout: u32,
    chunk: u64,
    size: u64,
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
        let mut sizes = Vec::new();
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
            sizes.push(sb.data_size);
            *entry = Some(Member {
                dev,
                data_offset: sb.data_offset,
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
        let size = match level {
            LEVEL_RAID1 => {
                need(1)?;
                per_member
            }
            LEVEL_RAID0 => {
                need(n)?;
                needs_chunk()?;
                if sizes.iter().any(|&s| s / chunk != sizes[0] / chunk) {
                    return Err(MdError::Unsupported(
                        "RAID0 over members of different sizes (more than one zone)".into(),
                    ));
                }
                (sizes[0] / chunk)
                    .checked_mul(chunk * u64::from(n))
                    .ok_or_else(|| corrupt(0, "array size overflows"))?
            }
            LEVEL_RAID4 | LEVEL_RAID5 | LEVEL_RAID6 => {
                needs_chunk()?;
                let parity = if level == LEVEL_RAID6 { 2 } else { 1 };
                if n <= parity {
                    return Err(corrupt(0, format!("RAID{level} with {n} members")));
                }
                if level == LEVEL_RAID6 && lead.layout != LAYOUT_LEFT_SYMMETRIC {
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
                // One missing member is recovered through P. Two, on a
                // RAID6, would need Q, which is not implemented.
                if present + 1 < n {
                    if level == LEVEL_RAID6 && present + 2 >= n {
                        return Err(MdError::Unsupported(
                            "RAID6 with two members missing (needs Q reconstruction)".into(),
                        ));
                    }
                    need(n - 1)?;
                }
                (per_member / chunk * chunk)
                    .checked_mul(u64::from(n - parity))
                    .ok_or_else(|| corrupt(0, "array size overflows"))?
            }
            other => {
                return Err(MdError::Unsupported(format!("RAID level {other}")));
            }
        };
        for m in slots.iter().flatten() {
            let span = if level == LEVEL_RAID1 {
                size
            } else {
                per_member
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

    fn read_chunk(&self, k: u64, within: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let n = self.slots.len() as u64;
        if self.level == LEVEL_RAID0 {
            let off = (k / n) * self.chunk + within;
            self.read_member((k % n) as usize, off, buf)?;
            return Ok(());
        }
        let parity = if self.level == LEVEL_RAID6 { 2 } else { 1 };
        let data = n - parity;
        let stripe = k / data;
        let layout = if self.level == LEVEL_RAID4 {
            LAYOUT_PARITY_LAST
        } else {
            self.layout
        };
        let (dd, _pd, qd) = parity_map(self.level, layout, n, stripe, k % data);
        let off = stripe * self.chunk + within;
        if self.read_member(dd, off, buf)? {
            return Ok(());
        }
        // The data member is missing: XOR every other member in the row
        // except Q, which is a different syndrome.
        buf.fill(0);
        let mut tmp = vec![0u8; buf.len()];
        for slot in 0..n as usize {
            if slot == dd || Some(slot) == qd {
                continue;
            }
            if !self.read_member(slot, off, &mut tmp)? {
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
mod tests {
    //! Self-consistency only: superblocks and stripes built here, read
    //! back here. Whether these layouts are the kernel's is
    //! `tests/oracle_md.rs`'s question, not this module's.
    use super::*;

    struct Mem(Vec<u8>);
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

    const MEMBER: usize = 1 << 20;
    const DATA_OFFSET: u64 = 64 * 1024;
    const CHUNK: u64 = 16 * 1024;

    /// A 1.2 superblock for slot `slot` of an `n`-member array.
    fn sb_v12(level: i32, layout: u32, n: u32, slot: u16, events: u64) -> Vec<u8> {
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
        let data_size = (MEMBER as u64 - DATA_OFFSET) / SECTOR;
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
                let (dd, pd, _) = parity_map(level, lay, nn, k / data, k % data);
                let off = (k / data) * CHUNK;
                let p0 = DATA_OFFSET as usize + off as usize;
                for (i, b) in src.iter().enumerate() {
                    members[pd][p0 + i] ^= b;
                }
                (dd, off)
            };
            let o = DATA_OFFSET as usize + off as usize;
            members[slot][o..o + chunk].copy_from_slice(src);
        }
        members
    }

    fn check(level: i32, layout: u32, n: u32, drop: Option<usize>) {
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
            .filter(|(i, _)| Some(*i) != drop)
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
    fn raid4_and_raid6_read_back_with_one_member_missing() {
        check(4, 0, 3, None);
        check(4, 0, 3, Some(0));
        check(6, LAYOUT_LEFT_SYMMETRIC, 5, None);
        for drop in 0..5 {
            check(6, LAYOUT_LEFT_SYMMETRIC, 5, Some(drop));
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
    fn raid5_with_two_missing_and_raid6_with_two_missing_are_refused() {
        let per = (MEMBER as u64 - DATA_OFFSET) / CHUNK * CHUNK;
        let members = build(5, 2, 4, &pattern((per * 3) as usize));
        let two: Vec<Mem> = members.into_iter().take(2).map(Mem).collect();
        assert!(matches!(
            MdArray::assemble(two),
            Err(MdError::TooFewMembers { .. })
        ));
        let members = build(6, 2, 5, &pattern((per * 3) as usize));
        let three: Vec<Mem> = members.into_iter().take(3).map(Mem).collect();
        assert!(matches!(
            MdArray::assemble(three),
            Err(MdError::Unsupported(_))
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
}
