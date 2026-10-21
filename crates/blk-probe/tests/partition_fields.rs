//! Every partition carries its table slot, the table rules it breaks, and how
//! much of it the device actually holds (#28).
//!
//! `rust-disk-partitions` 0.5.0 reports three things per entry that the document did
//! not: `slot`, `issues` and `available_length`. The last is the one a
//! consumer sizes a buffer with: `length` is what the table claims, and on an
//! image that stops early the device holds less.
//!
//! The truncated image here is a real GPT disk `sgdisk` wrote, cut short in
//! the middle of its last partition. The expected `available_length` is
//! computed from nothing but the file's own length and the partition's
//! start, and `tests/oracle_tools.rs` holds that start, and `length`, to
//! what `sfdisk` reports for the untruncated image.

use std::path::PathBuf;

fn corpus(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fuzz/corpus/probe")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn probe(bytes: &[u8], what: &str) -> String {
    blk_probe::probe_bytes(bytes, None, what)
        .unwrap_or_else(|e| panic!("{what}: would not probe: {e}"))
        .json
}

/// The value of a flat `"key":value` pair, as text, from `at` onwards.
fn field(json: &str, at: usize, key: &str) -> String {
    let needle = format!("\"{key}\":");
    let start = json[at..]
        .find(&needle)
        .unwrap_or_else(|| panic!("no {key} in {}", &json[at..]))
        + at
        + needle.len();
    let rest = &json[start..];
    rest[..rest.find([',', '}']).expect("end of a numeric field")].to_string()
}

fn num(json: &str, at: usize, key: &str) -> u64 {
    field(json, at, key)
        .parse()
        .unwrap_or_else(|e| panic!("{key} is not a number: {e}"))
}

fn partition_offsets(json: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(found) = json[at..].find("{\"index\":") {
        out.push(at + found);
        at += found + 1;
    }
    out
}

#[test]
fn a_whole_image_reports_every_partition_as_fully_available() {
    for name in [
        "gpt.img",
        "mbr.img",
        "gpt-fixed.vhd",
        "gpt.qcow2",
        "gpt.vmdk",
    ] {
        let json = probe(&corpus(name), name);
        let parts = partition_offsets(&json);
        assert!(!parts.is_empty(), "{name}: no partitions to check");
        for (i, at) in parts.into_iter().enumerate() {
            assert_eq!(
                num(&json, at, "available_length"),
                num(&json, at, "length"),
                "{name}: partition {i} fits on the image and is not all available",
            );
            assert_eq!(
                num(&json, at, "issues"),
                0,
                "{name}: partition {i} of a table a tool wrote breaks a rule",
            );
            // Every committed table is packed from the first slot, so the
            // slot is the index. oracle_tools.rs holds it to sfdisk's number.
            assert_eq!(
                num(&json, at, "slot"),
                i as u64,
                "{name}: partition {i} is not in slot {i}"
            );
        }
    }
}

#[test]
fn a_truncated_image_reports_what_is_there_beside_what_is_claimed() {
    let whole = corpus("gpt.img");
    let json = probe(&whole, "gpt.img");
    let parts = partition_offsets(&json);
    let last = *parts.last().expect("gpt.img has partitions");
    let start = num(&json, last, "start");
    let length = num(&json, last, "length");
    assert!(
        length >= 2 * 512,
        "the last partition is too small to cut in half"
    );

    // Cut on a sector boundary, half-way through the last partition.
    let cut = (start + length / 2) / 512 * 512;
    let truncated = &whole[..cut as usize];
    let json = probe(truncated, "gpt.img, truncated");
    let parts = partition_offsets(&json);
    let at = *parts
        .last()
        .expect("the truncated image still has its table");

    assert_eq!(num(&json, at, "start"), start, "the claim moved");
    assert_eq!(num(&json, at, "length"), length, "the claim moved");
    assert_eq!(
        num(&json, at, "available_length"),
        cut - start,
        "the device holds {} of the partition's bytes",
        cut - start,
    );
}
