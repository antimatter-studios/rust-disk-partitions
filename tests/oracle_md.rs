//! `md` arrays assembled by this crate against the kernel's `/dev/mdX`.
//!
//! `scripts/make-md-oracle.sh` has the kernel build each array with
//! `mdadm` on loop devices, write random data through `/dev/mdX`, and dump
//! every byte of the device. The members are then detached and left as
//! plain files. This test assembles those files with
//! [`disk_partitions::md::MdArray`] and requires:
//!
//! * the array's size, and every byte of it, to equal the kernel's dump;
//! * for each level with redundancy, the same with each member left out
//!   in turn, which is what exercises mirror fallback and parity
//!   reconstruction against parity the kernel computed;
//! * each 1.x member's level, slot, member count, chunk and data offset to
//!   agree with what `mdadm --examine` printed for it.
//!
//! # The fixtures are required, never probed for
//!
//! The arrays need root and the md driver, so they are built by a script
//! the `oracle (external tools)` job runs under sudo before this test.
//! When the directory is missing this test fails naming that script: a
//! run without the arrays would otherwise be green having compared
//! nothing.

use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use disk_partitions::capi::{partitions_md_assemble, ArrayErrorCode};
use disk_partitions::md::{read_superblock, MdArray, MdRole, MdVersion};
use disk_partitions::{BlockRead, FileBlock};
use fs_core::ffi::{
    fs_core_device_close, fs_core_device_read_at, fs_core_device_size_bytes, fs_core_file_open,
    FsCoreDevice, FsCoreErrorCode,
};

fn oracle_dir() -> PathBuf {
    let dir = std::env::var_os("MD_ORACLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp/md-oracle"));
    assert!(
        dir.is_dir(),
        "{} does not exist. Build it first with \
         `sudo scripts/make-md-oracle.sh {}` (needs mdadm, loop devices and \
         the md driver; the `oracle (external tools)` job in \
         .github/workflows/ci.yml does this). This test does not skip.",
        dir.display(),
        dir.display()
    );
    dir
}

struct Case {
    name: String,
    dir: PathBuf,
    members: usize,
    level: i32,
}

fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    for e in fs::read_dir(oracle_dir()).expect("read oracle dir") {
        let dir = e.expect("dir entry").path();
        let Ok(spec) = fs::read_to_string(dir.join("case")) else {
            continue;
        };
        let field = |k: &str| -> String {
            spec.split_whitespace()
                .find_map(|w| w.strip_prefix(&format!("{k}=")))
                .unwrap_or_else(|| panic!("{}: no {k}= in {spec:?}", dir.display()))
                .to_string()
        };
        out.push(Case {
            name: dir.file_name().unwrap().to_string_lossy().into_owned(),
            members: field("members").parse().unwrap(),
            level: field("level").parse().unwrap(),
            dir,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    assert!(
        out.len() >= 23,
        "only {} md oracle cases found; the script builds 23",
        out.len()
    );
    out
}

fn open(case: &Case, slot: usize) -> FileBlock {
    FileBlock::open(case.dir.join(format!("member-{slot}.img"))).expect("open member")
}

/// Assemble from `slots` and require every byte to equal the kernel's.
fn compare(case: &Case, slots: &[usize], expect: &[u8]) {
    let devs: Vec<FileBlock> = slots.iter().map(|&s| open(case, s)).collect();
    let array =
        MdArray::assemble(devs).unwrap_or_else(|e| panic!("{}: members {slots:?}: {e}", case.name));
    assert_eq!(
        array.size_bytes(),
        expect.len() as u64,
        "{}: members {slots:?}: array size, ours vs the kernel's",
        case.name
    );
    let mut got = vec![0u8; expect.len()];
    array.read_at(0, &mut got).unwrap();
    if got != expect {
        let at = got.iter().zip(expect).position(|(a, b)| a != b).unwrap();
        panic!(
            "{}: members {slots:?}: first differing byte at {at} (ours {:#04x}, kernel {:#04x})",
            case.name, got[at], expect[at]
        );
    }
}

#[test]
fn every_array_reads_the_bytes_the_kernel_wrote() {
    let mut compared = 0u64;
    for case in cases() {
        let expect = fs::read(case.dir.join("array.bin")).expect("array.bin");
        let all: Vec<usize> = (0..case.members).collect();
        // Shuffled order: assembly places members by their superblock.
        let mut reversed = all.clone();
        reversed.reverse();
        compare(&case, &reversed, &expect);
        compared += expect.len() as u64;
        // Every level with redundancy can lose any one member: RAID10's
        // copies of a chunk are always on different members.
        if case.level > 0 {
            for drop in 0..case.members {
                let some: Vec<usize> = all.iter().copied().filter(|&s| s != drop).collect();
                compare(&case, &some, &expect);
                compared += expect.len() as u64;
            }
        }
        // RAID6 can lose any two: data through P and Q together, or
        // through Q alone when P is one of the two.
        if case.level == 6 {
            for a in 0..case.members {
                for b in a + 1..case.members {
                    let some: Vec<usize> =
                        all.iter().copied().filter(|&s| s != a && s != b).collect();
                    compare(&case, &some, &expect);
                    compared += expect.len() as u64;
                }
            }
        }
        println!("md oracle: {} matches the kernel", case.name);
    }
    println!("md oracle: {compared} bytes compared");
}

/// The value of `key : value` in an `mdadm --examine` dump.
fn examine_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(" : ")?;
        (k.trim() == key).then_some(v.trim())
    })
}

#[test]
fn every_v1_superblock_agrees_with_mdadm_examine() {
    let mut checked = 0;
    for case in cases() {
        for slot in 0..case.members {
            let text = fs::read_to_string(case.dir.join(format!("member-{slot}.examine"))).unwrap();
            let sb = read_superblock(&open(&case, slot))
                .unwrap()
                .unwrap_or_else(|| panic!("{} member {slot}: no superblock", case.name));
            let what = format!("{} member {slot}", case.name);
            assert_eq!(sb.level, case.level, "{what}: level");
            assert_eq!(sb.role, MdRole::Active(slot as u32), "{what}: role");
            if sb.version == MdVersion::V0_90 {
                assert!(case.name.ends_with("v0.90"), "{what}: version");
                continue;
            }
            let version = examine_field(&text, "Version").expect("Version");
            let expect_version = match sb.version {
                MdVersion::V1_0 => "1.0",
                MdVersion::V1_1 => "1.1",
                MdVersion::V1_2 => "1.2",
                MdVersion::V0_90 => unreachable!(),
            };
            assert_eq!(version, expect_version, "{what}: version");
            let devices: u32 = examine_field(&text, "Raid Devices")
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(sb.raid_disks, devices, "{what}: raid devices");
            let sectors =
                |v: &str| -> u64 { v.split_whitespace().next().unwrap().parse().unwrap() };
            let super_offset = examine_field(&text, "Super Offset")
                .unwrap_or_else(|| panic!("{what}: no Super Offset in\n{text}"));
            assert_eq!(
                sb.superblock_offset,
                sectors(super_offset) * 512,
                "{what}: superblock offset"
            );
            // mdadm prints no Data Offset line for 1.0, where the data
            // starts at the beginning of the member (CI run 37622837093).
            let data_offset = match examine_field(&text, "Data Offset") {
                Some(v) => sectors(v) * 512,
                None if sb.version == MdVersion::V1_0 => 0,
                None => panic!("{what}: no Data Offset in\n{text}"),
            };
            assert_eq!(sb.data_offset, data_offset, "{what}: data offset");
            let role = examine_field(&text, "Device Role").expect("Device Role");
            assert_eq!(role, format!("Active device {slot}"), "{what}: device role");
            if let Some(chunk) = examine_field(&text, "Chunk Size") {
                let kib: u64 = chunk.trim_end_matches('K').parse().unwrap();
                assert_eq!(sb.chunk_bytes, kib * 1024, "{what}: chunk");
            }
            checked += 1;
        }
    }
    assert!(checked >= 30, "only {checked} 1.x members checked");
    println!("md oracle: {checked} superblocks agree with mdadm --examine");
}

/// Every byte of a C handle, read through `fs_core_device_read_at`.
fn read_handle(what: &str, dev: *const FsCoreDevice) -> Vec<u8> {
    let size = unsafe { fs_core_device_size_bytes(dev) };
    let mut got = vec![0u8; size as usize];
    let rc = unsafe { fs_core_device_read_at(dev, 0, got.as_mut_ptr(), got.len()) };
    assert_eq!(rc, FsCoreErrorCode::Ok, "{what}: read through the handle");
    got
}

fn file_handle(path: &Path) -> *mut FsCoreDevice {
    let c = CString::new(path.to_str().expect("UTF-8 path")).unwrap();
    let h = unsafe { fs_core_file_open(c.as_ptr(), false) };
    assert!(!h.is_null(), "open {}", path.display());
    h
}

/// The same arrays, assembled through `partitions_md_assemble` from
/// `fs_core_file_open` handles, as a C caller would (#164). The members
/// go in reversed, and every level with redundancy also loses member 0.
#[test]
fn every_array_reads_the_kernels_bytes_through_the_c_abi() {
    let mut compared = 0u64;
    for case in cases() {
        let expect = fs::read(case.dir.join("array.bin")).expect("array.bin");
        let mut sets: Vec<Vec<usize>> = vec![(0..case.members).rev().collect()];
        if case.level > 0 {
            sets.push((1..case.members).collect());
        }
        for slots in sets {
            let what = format!("{}: members {slots:?} through the C ABI", case.name);
            let members: Vec<*mut FsCoreDevice> = slots
                .iter()
                .map(|s| file_handle(&case.dir.join(format!("member-{s}.img"))))
                .collect();
            let ptrs: Vec<*const FsCoreDevice> = members.iter().map(|&m| m as *const _).collect();
            let mut array: *mut FsCoreDevice = std::ptr::null_mut();
            let mut reason = -1i32;
            let rc = unsafe {
                partitions_md_assemble(ptrs.as_ptr(), ptrs.len(), &mut array, &mut reason)
            };
            assert_eq!(rc, FsCoreErrorCode::Ok, "{what}: reason {reason}");
            assert_eq!(reason, ArrayErrorCode::None as i32, "{what}: reason");
            // The array holds its members: closing them first is allowed.
            for m in members {
                unsafe { fs_core_device_close(m) };
            }
            let got = read_handle(&what, array);
            unsafe { fs_core_device_close(array) };
            assert_eq!(
                got.len(),
                expect.len(),
                "{what}: array size, ours vs the kernel's"
            );
            if got != expect {
                let at = got.iter().zip(&expect).position(|(a, b)| a != b).unwrap();
                panic!("{what}: first differing byte at {at}");
            }
            compared += expect.len() as u64;
        }
    }
    println!("md oracle: {compared} bytes compared through the C ABI");
}
