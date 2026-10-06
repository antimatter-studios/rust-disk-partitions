#![no_main]
//! Sector zero, as an MBR.
//!
//! Four sixteen-byte entries, each declaring a start LBA and a length
//! whose sum is used as a range, plus a type byte that decides whether
//! the entry is a partition, an extended container, or a GPT
//! protective marker.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut sector = [0u8; disk_partitions::SECTOR_SIZE_USIZE];
    let take = data.len().min(sector.len());
    sector[..take].copy_from_slice(&data[..take]);

    // Both device sizes: 0 is a device that stated nothing about
    // itself, and the fuzzed one exercises the clamp that fills
    // `available_length` (#38).
    let device_size = u64::from_le_bytes(sector[..8].try_into().unwrap());
    let _ = disk_partitions::mbr::parse(&sector, 0);
    let _ = disk_partitions::mbr::parse(&sector, device_size);
    let _ = disk_partitions::mbr::is_protective(&sector);
    let _ = disk_partitions::mbr::has_gpt_marker(&sector);
});
