//! The public surface a consumer builds with, held to the release rule.
//!
//! This crate's compatibility boundary is the **minor**, and the way
//! that rule was broken was not a signature change anybody argued
//! about: a public struct with no `Default`, no constructor and no
//! `#[non_exhaustive]` quietly gained a required field, the version
//! stayed put, and the changelog recorded nothing. It happened six
//! times between `v0.4.1` and `v0.5.0` across two structs, and the only
//! signal anybody got was `rust-blk-probe` failing to compile against a
//! sibling checkout with `missing fields issues and slot` (#74, #131).
//!
//! The tests below destructure each publicly-constructible struct
//! **exhaustively** — no `..` rest pattern, which is what makes them a
//! guard rather than a description. Adding a field stops this file
//! compiling, at the moment the field is added, with the reason written
//! beside it.
//!
//! # If this file fails to compile
//!
//! You have added a required field to a struct a consumer builds with a
//! struct literal. That is a breaking change by this crate's own rule.
//! Three things go with it, in the same pull request:
//!
//! 1. name the field here, so the next one is caught too;
//! 2. bump the **minor** in `Cargo.toml` (or confirm the unreleased
//!    version is already a minor ahead of the last tag);
//! 3. record it in `CHANGELOG.md` under a `### Changed` heading that
//!    says it is breaking — an `### Added` bullet reads like a new
//!    convenience and is what let five of the six through.
//!
//! `#[non_exhaustive]` is deliberately **not** the answer here. These
//! structs are built by literal in this repository's own test crates,
//! which are separate crates, so it would break the suite that proves
//! the crate works while doing nothing for a consumer who has to fill
//! the field in regardless.

use disk_partitions::capi::PartitionInfo;
use disk_partitions::mutation::PartitionSet;
use disk_partitions::{Partition, PartitionKind, TableKind};

/// Every field of `Partition`, named.
#[test]
fn partition_has_exactly_the_fields_the_changelog_documents() {
    let p = Partition {
        start: 1024,
        length: 2048,
        kind: PartitionKind::Whole,
        label: None,
        uuid: None,
        slot: None,
        issues: 0,
        available_length: 2048,
    };

    // Exhaustive: no `..`. A new field is a compile error here.
    let Partition {
        start,
        length,
        kind,
        label,
        uuid,
        slot,
        issues,
        available_length,
    } = p;

    assert_eq!(start, 1024);
    assert_eq!(length, 2048);
    assert_eq!(kind, PartitionKind::Whole);
    assert_eq!(label, None);
    assert_eq!(uuid, None);
    assert_eq!(slot, None);
    assert_eq!(issues, 0);
    assert_eq!(available_length, 2048);
}

/// Every field of `PartitionSet`, named.
///
/// This is the struct the first report of the problem missed
/// altogether: it took three required fields to `Partition`'s three,
/// and a downstream `PartitionSet { .. }` literal breaks exactly as a
/// `Partition` one does.
#[test]
fn partition_set_has_exactly_the_fields_the_changelog_documents() {
    let set = PartitionSet {
        table_kind: TableKind::Gpt,
        partitions: Vec::new(),
        disk_size: 1 << 20,
        disk_guid: [0u8; 16],
        gpt_entry_tails: Default::default(),
        gpt_geometry: disk_partitions::gpt_write::GptGeometry::canonical(),
        reserved: Vec::new(),
    };

    let PartitionSet {
        table_kind,
        partitions,
        disk_size,
        disk_guid,
        gpt_entry_tails,
        gpt_geometry,
        reserved,
    } = set;

    assert_eq!(table_kind, TableKind::Gpt);
    assert!(partitions.is_empty());
    assert_eq!(disk_size, 1 << 20);
    assert_eq!(disk_guid, [0u8; 16]);
    assert!(gpt_entry_tails.is_empty());
    assert_eq!(
        gpt_geometry,
        disk_partitions::gpt_write::GptGeometry::canonical()
    );
    assert!(reserved.is_empty());
}

/// Every field of `PartitionInfo`, named.
///
/// `tests/c_abi.rs` answers a different question — that the C header
/// and the Rust struct agree on size and every offset — and it answers
/// it about the fields it is given. This one is about the field list
/// itself, and about the consumer that builds a `PartitionInfo` by hand
/// (`rust-blk-probe` does), for whom a new field is a compile error
/// rather than a padding change.
#[test]
fn partition_info_has_exactly_the_fields_the_header_declares() {
    let info = PartitionInfo {
        start: 0,
        length: 0,
        fs_kind: 0,
        table_kind: 0,
        type_guid: [0u8; 16],
        type_byte: 0,
        _pad: [0u8; 7],
        label: std::ptr::null(),
        label_len: 0,
        bootable: 0,
        _pad2: [0u8; 7],
        attributes: 0,
        slot: -1,
        issues: 0,
        available_length: 0,
    };

    let PartitionInfo {
        start,
        length,
        fs_kind,
        table_kind,
        type_guid,
        type_byte,
        _pad,
        label,
        label_len,
        bootable,
        _pad2,
        attributes,
        slot,
        issues,
        available_length,
    } = info;

    assert_eq!(start, 0);
    assert_eq!(length, 0);
    assert_eq!(fs_kind, 0);
    assert_eq!(table_kind, 0);
    assert_eq!(type_guid, [0u8; 16]);
    assert_eq!(type_byte, 0);
    assert_eq!(_pad, [0u8; 7]);
    assert!(label.is_null());
    assert_eq!(label_len, 0);
    assert_eq!(bootable, 0);
    assert_eq!(_pad2, [0u8; 7]);
    assert_eq!(attributes, 0);
    assert_eq!(slot, -1);
    assert_eq!(issues, 0);
    assert_eq!(available_length, 0);
}

/// The version in `Cargo.toml` has a section of its own in the
/// changelog.
///
/// The release that prompted this file had its breaking changes sitting
/// on `main` under `[Unreleased]` with the version still reading
/// `0.4.1`, so nothing downstream could ask for them and nothing said
/// they were waiting. A version with no section is a release nobody
/// wrote down; a section with no version is a release nobody cut.
#[test]
fn the_crate_version_has_a_changelog_section_of_its_own() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("Cargo.toml is beside this test");
    let version = manifest
        .parse::<toml::Table>()
        .expect("Cargo.toml parses as TOML")["package"]["version"]
        .as_str()
        .expect("package.version is a string")
        .to_string();

    let changelog = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/CHANGELOG.md"))
        .expect("CHANGELOG.md is beside this test");
    let heading = format!("## [{version}]");
    assert!(
        changelog.lines().any(|l| l.starts_with(&heading)),
        "Cargo.toml says version {version} and CHANGELOG.md has no `{heading}` section: \
         either the release was not written down, or the version was not bumped for what \
         is written down"
    );
}
