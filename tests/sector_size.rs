//! A 4Kn disk a real tool wrote, and the refusal this crate answers it
//! with.
//!
//! # Why this image is committed
//!
//! `Error::UnsupportedSectorSize` is how this crate says "this disk's
//! logical sectors are 4096 bytes, and I read partition tables in
//! 512-byte units only". The failure it prevents is the worst one a
//! probe has: describing a perfectly healthy disk as a corrupt table,
//! which sends whoever is looking at it to a repair tool.
//!
//! Until now that refusal was asserted only against 4Kn headers this
//! repository builds itself, in `tests/fixtures.rs` and
//! `tests/mutation.rs` -- our writer against our reader, which cannot
//! catch a misreading because the mistake would be baked into both. The
//! external-tool oracle in `tests/oracle_tools.rs` covers every other
//! table shape this crate can write and could not cover this one:
//! `sfdisk --sector-size 4096` is the only way to put a 4Kn table in a
//! plain file, the option arrived in util-linux **2.40**, and
//! ubuntu-latest ships 2.39.3 and rejects it. `sgdisk` has no
//! equivalent; the script format's `sector-size:` header is not a
//! substitute (measured on 2.41: the table is written at 512 bytes per
//! sector regardless); and a `losetup --sector-size 4096` loop device
//! needs root, which would cost the oracle the property that makes it
//! runnable anywhere (#123).
//!
//! So the tool is **recorded rather than required**:
//! `scripts/make-4kn-fixture.sh` writes `tests/images/4kn-gpt.img` with
//! a modern `sfdisk` and records that tool's own `--json` description
//! of it beside the image. The tests below check this crate against
//! that record. It is the same arrangement `fuzz/corpus/` already uses
//! -- real tables real tools wrote, replayed on machines that have
//! none of those tools -- and it means these run on every platform in
//! the matrix rather than on the one leg that has util-linux.
//!
//! # What makes it an oracle and not homework
//!
//! Every expectation here comes from sfdisk's record: that the image is
//! a GPT, that its sectors are 4096 bytes, where its partitions start,
//! how long they are, what type they are and what they are called.
//! Nothing in this file asserts a number this crate produced.

use std::fs;
use std::path::PathBuf;

use disk_partitions::Error;

/// The image as a device, opened as a file rather than copied into
/// memory: it is the committed bytes that are under test, and a
/// `FileDevice` is what a caller of this crate actually hands it.
fn device() -> fs_core::FileDevice {
    let path = images().join("4kn-gpt.img");
    fs_core::FileDevice::open(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}. Rebuild it with scripts/make-4kn-fixture.sh, which needs \
             util-linux 2.40 or later for `sfdisk --sector-size`.",
            path.display()
        )
    })
}

fn images() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/images")
}

/// The image's bytes, or a failure naming the script that writes it.
///
/// It does not skip when the fixture is missing. A missing fixture is
/// the one condition that would make every assertion below vacuous, and
/// a suite that quietly declines to run reads exactly like one that
/// passed.
fn image() -> Vec<u8> {
    let path = images().join("4kn-gpt.img");
    fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}. Rebuild it with scripts/make-4kn-fixture.sh, which needs \
             util-linux 2.40 or later for `sfdisk --sector-size`.",
            path.display()
        )
    })
}

/// What sfdisk said about the image when it wrote it.
fn record() -> serde_json::Value {
    let path = images().join("4kn-gpt.json");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}. It is written beside the image by \
             scripts/make-4kn-fixture.sh and is the oracle these tests compare \
             against.",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("the recorded sfdisk --json output parses")
}

fn table(record: &serde_json::Value) -> serde_json::Value {
    record["sfdisk"]["partitiontable"].clone()
}

/// A GPT type GUID as the specification stores it: the first three
/// fields little-endian, the last two as written.
fn guid_bytes(text: &str) -> [u8; 16] {
    let hex: Vec<u8> = text
        .chars()
        .filter(|c| *c != '-')
        .collect::<Vec<_>>()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(&pair.iter().collect::<String>(), 16).expect("a GUID is hexadecimal")
        })
        .collect();
    assert_eq!(hex.len(), 16, "a GUID is sixteen bytes: {text}");
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&[hex[3], hex[2], hex[1], hex[0]]);
    out[4..6].copy_from_slice(&[hex[5], hex[4]]);
    out[6..8].copy_from_slice(&[hex[7], hex[6]]);
    out[8..16].copy_from_slice(&hex[8..16]);
    out
}

/// THE REFUSAL, AGAINST A DISK SOMEBODY ELSE CALLS HEALTHY.
///
/// `sfdisk` reads this image as a GPT with two partitions. This crate
/// cannot read it -- its offsets are 512-byte units throughout -- and
/// what matters is that it says so by name instead of reporting the
/// corruption that is not there.
#[test]
fn a_4kn_gpt_sfdisk_wrote_is_refused_by_its_sector_size() {
    let dev = device();

    match disk_partitions::probe(&dev) {
        Err(Error::UnsupportedSectorSize(why)) => {
            assert!(
                why.contains("4096"),
                "the refusal must name the sector size the disk really has: {why}"
            );
        }
        Err(other) => panic!(
            "a healthy 4Kn GPT that sfdisk reads as a table came back as {other:?}. \
             That is the failure this test exists for: a disk described as broken \
             when it is only described in units this crate does not read."
        ),
        Ok((kind, parts)) => panic!(
            "a 4Kn disk was read as a {kind:?} with {} partitions, at 512-byte \
             offsets it does not use",
            parts.len()
        ),
    }
}

/// And consulting the backup does not turn the refusal into an answer.
///
/// `probe_with_status` falls back to the backup GPT when the primary
/// does not parse. A 4Kn disk's backup is not at the 512-byte offset
/// that fallback looks at either, so the risk is a different wrong
/// answer rather than the same right one.
#[test]
fn the_backup_fallback_does_not_rescue_a_4kn_disk_into_a_wrong_answer() {
    let dev = device();
    match disk_partitions::probe_with_status(&dev) {
        Err(Error::UnsupportedSectorSize(_)) => {}
        other => panic!("probe_with_status on a 4Kn disk gave {other:?}"),
    }
}

/// THE FIXTURE IS THE DISK SFDISK DESCRIBED.
///
/// A committed image is only an oracle while the bytes and the record
/// still belong together. This checks them against each other at every
/// point that decides the question: the signature is where 4096-byte
/// LBAs put it and is NOT where 512-byte ones would, and every
/// partition sfdisk listed is in the entry array where sfdisk said,
/// with the type and name sfdisk gave it.
#[test]
fn the_committed_image_is_the_4kn_disk_sfdisk_recorded() {
    let bytes = image();
    let record = record();
    let table = table(&record);

    assert_eq!(table["label"], "gpt", "the record is of a GPT");
    assert_eq!(
        table["sectorsize"], 4096,
        "the record is of a 4096-byte-sector disk; without that this fixture \
         is not the thing under test"
    );

    // A 4Kn disk keeps its GPT header at byte 4096, because that is
    // where LBA 1 begins. Byte 512 is still inside LBA 0 -- the tail of
    // the protective MBR -- and is what a 512-byte reader looks at.
    assert_eq!(
        &bytes[4096..4104],
        b"EFI PART",
        "no GPT header at byte 4096: this is not a 4Kn image"
    );
    assert_ne!(
        &bytes[512..520],
        b"EFI PART",
        "a GPT header at byte 512 would make this an ordinary 512-byte disk"
    );
    assert_eq!(
        &bytes[510..512],
        &[0x55, 0xAA],
        "a 4Kn disk still carries a protective MBR in the first 512 bytes"
    );
    // `my_lba` at header+24 says the header believes it is at LBA 1,
    // which is only true at byte 4096 when an LBA is 4096 bytes.
    assert_eq!(
        u64::from_le_bytes(bytes[4096 + 24..4096 + 32].try_into().unwrap()),
        1
    );

    // The entry array, at the LBA the header names, in 4096-byte units.
    let entry_lba = u64::from_le_bytes(bytes[4096 + 72..4096 + 80].try_into().unwrap());
    let entry_size = u32::from_le_bytes(bytes[4096 + 84..4096 + 88].try_into().unwrap()) as usize;
    let array = (entry_lba * 4096) as usize;

    let partitions = table["partitions"].as_array().expect("a list").clone();
    assert_eq!(
        partitions.len(),
        2,
        "the script writes two partitions; the record lists {}",
        partitions.len()
    );

    for (slot, want) in partitions.iter().enumerate() {
        let off = array + slot * entry_size;
        let start = u64::from_le_bytes(bytes[off + 32..off + 40].try_into().unwrap());
        let end = u64::from_le_bytes(bytes[off + 40..off + 48].try_into().unwrap());
        let type_guid: [u8; 16] = bytes[off..off + 16].try_into().unwrap();
        let name: String = char::decode_utf16(
            bytes[off + 56..off + 128]
                .chunks(2)
                .map(|p| u16::from_le_bytes([p[0], p[1]]))
                .take_while(|u| *u != 0),
        )
        .map(|c| c.expect("a partition name is valid UTF-16"))
        .collect();

        assert_eq!(
            start,
            want["start"].as_u64().unwrap(),
            "slot {slot}: the entry array and sfdisk disagree about where it starts"
        );
        assert_eq!(
            end - start + 1,
            want["size"].as_u64().unwrap(),
            "slot {slot}: the entry array and sfdisk disagree about how long it is"
        );
        assert_eq!(
            type_guid,
            guid_bytes(want["type"].as_str().unwrap()),
            "slot {slot}: the entry array and sfdisk disagree about its type"
        );
        assert_eq!(
            name,
            want["name"].as_str().unwrap(),
            "slot {slot}: the entry array and sfdisk disagree about its name"
        );
    }
}

/// The C caller gets the same refusal, and can read why.
///
/// A consumer sees this crate through the FFI, so a refusal that
/// arrives there as a bare error code with no message is a refusal
/// nobody can act on -- and "unsupported sector size" and "corrupt
/// table" demand opposite responses from a user.
#[test]
fn the_c_abi_refuses_a_4kn_disk_and_says_why() {
    use disk_partitions::capi::*;
    use std::ptr;
    use std::sync::Arc;

    let handle = fs_core::ffi::FsCoreDevice::into_handle(Arc::new(device()));

    let mut list: *mut PartitionList = ptr::null_mut();
    let rc = unsafe { partitions_probe(handle, &mut list) };
    assert_ne!(
        rc,
        fs_core::ffi::FsCoreErrorCode::Ok,
        "the C ABI read a 4Kn disk at 512-byte offsets and called it a table"
    );
    assert!(list.is_null(), "a refused probe must not hand back a list");

    let message = unsafe { std::ffi::CStr::from_ptr(fs_core::ffi::fs_core_last_error_message()) }
        .to_string_lossy()
        .into_owned();
    assert!(
        message.contains("4096"),
        "the C caller cannot tell an unsupported sector size from a corrupt \
         table: {message}"
    );

    unsafe { fs_core::ffi::fs_core_device_close(handle) };
}
