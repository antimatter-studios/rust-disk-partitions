//! dm-thin's on-disk metadata, as far as reading a thin volume needs it:
//! the superblock at the start of the pool's metadata sub-volume, and the
//! two-level btree that maps (thin device, block) to a block of the pool's
//! data sub-volume (the kernel's drivers/md/dm-thin-metadata.c over
//! drivers/md/persistent-data).
//!
//! Every metadata block is 4 KiB and starts with a CRC32C of the rest of
//! the block, XORed with a constant for its kind. A block whose checksum
//! does not match is refused rather than read: a stale or torn node would
//! otherwise send a read to the wrong data block, and return someone
//! else's bytes as the volume's.

use super::LvmError;

/// Metadata block size in bytes (`THIN_METADATA_BLOCK_SIZE`).
pub(super) const BLOCK: u64 = 4096;

/// `THIN_SUPERBLOCK_MAGIC`.
const MAGIC: u64 = 27_022_010;
const SUPERBLOCK_CSUM_XOR: u32 = 160_774;
const BTREE_CSUM_XOR: u32 = 121_107;

/// `struct thin_disk_superblock` offsets.
const SB_MAGIC: usize = 32;
const SB_VERSION: usize = 40;
const SB_DATA_MAPPING_ROOT: usize = 320;
const SB_DATA_BLOCK_SIZE: usize = 336;
const SB_METADATA_BLOCK_SIZE: usize = 340;
const SB_INCOMPAT_FLAGS: usize = 360;

/// `struct node_header`: csum, flags, blocknr, nr_entries, max_entries,
/// value_size, padding; the keys follow, then the values.
const NODE_FLAGS: usize = 4;
const NODE_BLOCKNR: usize = 8;
const NODE_NR_ENTRIES: usize = 16;
const NODE_MAX_ENTRIES: usize = 20;
const NODE_VALUE_SIZE: usize = 24;
const NODE_HEADER: usize = 32;
const INTERNAL_NODE: u32 = 1;
const LEAF_NODE: u32 = 2;

/// Deeper than any btree over 64-bit keys in 4 KiB nodes can be; a walk
/// this long is following a cycle.
const MAX_DEPTH: usize = 16;

/// The low 24 bits of a mapping value are the time it was made; the rest
/// is the data block (`pack_block_time`).
const TIME_BITS: u32 = 24;

/// What reading a thin volume needs from the pool's superblock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Superblock {
    /// Root of the two-level data mapping btree.
    pub data_mapping_root: u64,
    /// Pool data block size in bytes.
    pub data_block_size: u64,
}

/// Reads the metadata sub-volume at a byte offset.
pub(super) type ReadBlock<'a> = dyn Fn(u64, &mut [u8]) -> fs_core::Result<()> + 'a;

/// The superblock at metadata block 0.
pub(super) fn superblock(read: &ReadBlock<'_>) -> Result<Superblock, LvmError> {
    let b = block(read, 0, SUPERBLOCK_CSUM_XOR, "thin pool superblock")?;
    if le64(&b, SB_MAGIC) != MAGIC {
        return Err(LvmError::Corrupt("thin pool superblock: bad magic".into()));
    }
    let version = le32(&b, SB_VERSION);
    if !(1..=2).contains(&version) {
        return Err(LvmError::Unsupported(format!(
            "thin pool metadata version {version}"
        )));
    }
    let incompat = le32(&b, SB_INCOMPAT_FLAGS);
    if incompat != 0 {
        return Err(LvmError::Unsupported(format!(
            "thin pool metadata incompatible features {incompat:#x}"
        )));
    }
    let meta = u64::from(le32(&b, SB_METADATA_BLOCK_SIZE)) * super::SECTOR;
    if meta != BLOCK {
        return Err(LvmError::Corrupt(format!(
            "thin pool metadata block size {meta}"
        )));
    }
    let data_block_size = u64::from(le32(&b, SB_DATA_BLOCK_SIZE)) * super::SECTOR;
    if data_block_size == 0 {
        return Err(LvmError::Corrupt("thin pool data block size 0".into()));
    }
    Ok(Superblock {
        data_mapping_root: le64(&b, SB_DATA_MAPPING_ROOT),
        data_block_size,
    })
}

/// The root of thin device `dev`'s block map, if the pool has the device.
pub(super) fn device_root(
    read: &ReadBlock<'_>,
    sb: &Superblock,
    dev: u64,
) -> Result<Option<u64>, LvmError> {
    lookup(read, sb.data_mapping_root, dev)
}

/// The pool data block that block `vb` of the device whose map is at
/// `root` is kept in, or `None` for a block never written: a hole.
pub(super) fn data_block(
    read: &ReadBlock<'_>,
    root: u64,
    vb: u64,
) -> Result<Option<u64>, LvmError> {
    Ok(lookup(read, root, vb)?.map(|v| v >> TIME_BITS))
}

/// The 64-bit value under `key` in the btree at `root`.
fn lookup(read: &ReadBlock<'_>, root: u64, key: u64) -> Result<Option<u64>, LvmError> {
    let mut at = root;
    for _ in 0..MAX_DEPTH {
        let b = block(read, at, BTREE_CSUM_XOR, "thin pool btree node")?;
        if le64(&b, NODE_BLOCKNR) != at {
            return Err(LvmError::Corrupt(format!(
                "thin pool btree node at block {at} says it is block {}",
                le64(&b, NODE_BLOCKNR)
            )));
        }
        let flags = le32(&b, NODE_FLAGS);
        let nr = le32(&b, NODE_NR_ENTRIES) as usize;
        let max = le32(&b, NODE_MAX_ENTRIES) as usize;
        let value_size = le32(&b, NODE_VALUE_SIZE) as usize;
        if value_size != 8 || nr > max || NODE_HEADER + max * (8 + value_size) > BLOCK as usize {
            return Err(LvmError::Corrupt(format!(
                "thin pool btree node at block {at}: {nr} of {max} entries of {value_size} bytes"
            )));
        }
        let key_at = |i: usize| le64(&b, NODE_HEADER + 8 * i);
        let value_at = |i: usize| le64(&b, NODE_HEADER + 8 * max + 8 * i);
        // The last entry whose key is at or below `key` (the kernel's
        // lower_bound): the child that covers it, or the leaf entry that
        // may be it.
        let keys: Vec<u64> = (0..nr).map(key_at).collect();
        let below = keys.partition_point(|&k| k <= key);
        let Some(i) = below.checked_sub(1) else {
            return Ok(None);
        };
        match flags {
            INTERNAL_NODE => at = value_at(i),
            LEAF_NODE => return Ok((keys[i] == key).then(|| value_at(i))),
            _ => {
                return Err(LvmError::Corrupt(format!(
                    "thin pool btree node at block {at}: flags {flags:#x}"
                )))
            }
        }
    }
    Err(LvmError::Corrupt(format!(
        "thin pool btree from block {root} is deeper than {MAX_DEPTH}"
    )))
}

/// Metadata block `n`, its checksum verified against `xor`.
fn block(read: &ReadBlock<'_>, n: u64, xor: u32, what: &str) -> Result<Vec<u8>, LvmError> {
    let mut b = vec![0u8; BLOCK as usize];
    let at = n
        .checked_mul(BLOCK)
        .ok_or_else(|| LvmError::Corrupt(format!("{what}: block {n} is out of range")))?;
    read(at, &mut b).map_err(LvmError::Block)?;
    let want = crc32c_register(&b[4..]) ^ xor;
    if le32(&b, 0) != want {
        return Err(LvmError::Corrupt(format!(
            "{what} at metadata block {n}: checksum mismatch"
        )));
    }
    Ok(b)
}

/// The CRC32C register after `data` from an all-ones start, not inverted
/// at the end: what the kernel's `crc32c(~0, ...)` returns, and so what
/// `dm_bm_checksum` XORs with the block's constant.
fn crc32c_register(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc = CRC32C[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

/// Castagnoli, reflected (polynomial 0x82F63B78).
const CRC32C: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x82f6_3b78
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The register for the standard check string is the complement of
    /// CRC32C's published check value, 0xE3069283.
    #[test]
    fn the_register_is_crc32c_before_its_final_inversion() {
        assert_eq!(!crc32c_register(b"123456789"), 0xe306_9283);
    }
}
