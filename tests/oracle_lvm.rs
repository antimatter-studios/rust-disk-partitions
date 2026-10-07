//! LVM2 logical volumes read by this crate against the kernel's
//! device-mapper, including one on md RAID5 inside GPT partitions, the
//! way a Synology volume is laid out.
//!
//! `scripts/make-lvm-oracle.sh` has `lvm2` and `mdadm` build the volumes
//! on loop devices, write random data through `/dev/<vg>/<lv>` and
//! `/dev/mdX`, and dump every byte. This test opens the detached image
//! files with [`disk_partitions::lvm::LogicalVolume`] — through
//! [`disk_partitions::probe`] and [`disk_partitions::md::MdArray`] for the
//! stacked case — and requires the kernel's bytes back.
//!
//! The fixtures need root, so they are built by the `oracle (external
//! tools)` job before this test runs. When they are missing this test
//! fails naming the script; it does not skip.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use disk_partitions::lvm::{read_volume_group, LogicalVolume};
use disk_partitions::md::MdArray;
use disk_partitions::{probe, BlockRead, FileBlock, OwnedSlice};

fn oracle_dir() -> PathBuf {
    let dir = std::env::var_os("LVM_ORACLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp/lvm-oracle"));
    assert!(
        dir.is_dir(),
        "{} does not exist. Build it first with `sudo scripts/make-lvm-oracle.sh {}` \
         (needs mdadm, lvm2, loop devices and device-mapper; the `oracle (external \
         tools)` job in .github/workflows/ci.yml does this). This test does not skip.",
        dir.display(),
        dir.display()
    );
    dir
}

fn same(what: &str, dev: &dyn BlockRead, expect_path: &Path) {
    let expect = fs::read(expect_path).unwrap_or_else(|e| panic!("{what}: {e}"));
    assert_eq!(
        dev.size_bytes(),
        expect.len() as u64,
        "{what}: size, ours vs the kernel's"
    );
    let mut got = vec![0u8; expect.len()];
    dev.read_at(0, &mut got).unwrap();
    if got != expect {
        let at = got.iter().zip(&expect).position(|(a, b)| a != b).unwrap();
        panic!(
            "{what}: first differing byte at {at} (ours {:#04x}, kernel {:#04x})",
            got[at], expect[at]
        );
    }
    println!(
        "lvm oracle: {what} matches the kernel ({} bytes)",
        expect.len()
    );
}

fn pvs(dir: &Path, n: usize) -> Vec<FileBlock> {
    (0..n)
        .map(|i| FileBlock::open(dir.join(format!("pv-{i}.img"))).unwrap())
        .collect()
}

#[test]
fn a_linear_volume_in_two_segments_reads_the_kernels_bytes() {
    let dir = oracle_dir().join("linear");
    let a = LogicalVolume::open(pvs(&dir, 1), "a").unwrap();
    assert_eq!(
        a.info().segments.len(),
        2,
        "lvextend after b leaves a in two segments"
    );
    same("linear/a", &a, &dir.join("a.bin"));
    let b = LogicalVolume::open(pvs(&dir, 1), "b").unwrap();
    same("linear/b", &b, &dir.join("b.bin"));
}

#[test]
fn a_striped_volume_reads_the_kernels_bytes_whatever_the_pv_order() {
    let dir = oracle_dir().join("striped");
    let s = LogicalVolume::open(pvs(&dir, 3), "s").unwrap();
    assert_eq!(s.info().segments[0].stripes.len(), 3);
    same("striped/s", &s, &dir.join("s.bin"));
    let mut reversed = pvs(&dir, 3);
    reversed.reverse();
    let s = LogicalVolume::open(reversed, "s").unwrap();
    same("striped/s (PVs reversed)", &s, &dir.join("s.bin"));
}

/// Partition `index` (0-based) of each Synology-like disk image.
fn partitions(dir: &Path, index: usize, skip: Option<usize>) -> Vec<OwnedSlice> {
    (0..3)
        .filter(|&n| Some(n) != skip)
        .map(|n| {
            let disk: Arc<dyn BlockRead> =
                Arc::new(FileBlock::open(dir.join(format!("disk-{n}.img"))).unwrap());
            let (_, parts) = probe(&*disk).unwrap();
            let p = &parts[index];
            OwnedSlice::new(disk, p.start, p.length)
        })
        .collect()
}

#[test]
fn a_synology_layout_reads_through_gpt_md_and_lvm() {
    let dir = oracle_dir().join("synology");
    for skip in [None, Some(0), Some(1), Some(2)] {
        let system = MdArray::assemble(partitions(&dir, 0, skip)).unwrap();
        assert_eq!(system.superblock().level, 1);
        same(
            &format!("synology/system without disk {skip:?}"),
            &system,
            &dir.join("system.bin"),
        );
        let data = MdArray::assemble(partitions(&dir, 1, skip)).unwrap();
        assert_eq!(data.superblock().level, 5);
        let vg = read_volume_group(std::slice::from_ref(&data)).unwrap();
        assert_eq!(vg.name, "vg1000");
        let lv = LogicalVolume::open(vec![data], "lv").unwrap();
        same(
            &format!("synology/vg1000/lv without disk {skip:?}"),
            &lv,
            &dir.join("lv.bin"),
        );
    }
}
