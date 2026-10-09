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

use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use disk_partitions::capi::{
    partitions_list_free, partitions_lvm_open, partitions_md_assemble, partitions_open_slice,
    partitions_probe, ArrayErrorCode, PartitionList,
};
use disk_partitions::container::{self, Container};
use disk_partitions::lvm::{self, read_pv_label, read_volume_group, LogicalVolume};
use disk_partitions::md::{self, MdArray};
use disk_partitions::{probe, BlockRead, FileBlock, OwnedSlice};
use fs_core::ffi::{
    fs_core_device_close, fs_core_device_read_at, fs_core_device_size_bytes, fs_core_file_open,
    FsCoreDevice, FsCoreErrorCode,
};

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

fn file_handle(path: &Path) -> *mut FsCoreDevice {
    let c = CString::new(path.to_str().expect("UTF-8 path")).unwrap();
    let h = unsafe { fs_core_file_open(c.as_ptr(), false) };
    assert!(!h.is_null(), "open {}", path.display());
    h
}

/// Open `name` through `partitions_lvm_open`, read every byte through the
/// handle, and close it.
fn lv_through_c(what: &str, devices: &[*mut FsCoreDevice], name: &str) -> Vec<u8> {
    let ptrs: Vec<*const FsCoreDevice> = devices.iter().map(|&d| d as *const _).collect();
    let cname = CString::new(name).unwrap();
    let mut lv: *mut FsCoreDevice = std::ptr::null_mut();
    let mut reason = -1i32;
    let rc = unsafe {
        partitions_lvm_open(
            ptrs.as_ptr(),
            ptrs.len(),
            cname.as_ptr(),
            &mut lv,
            &mut reason,
        )
    };
    assert_eq!(rc, FsCoreErrorCode::Ok, "{what}: reason {reason}");
    assert_eq!(reason, ArrayErrorCode::None as i32, "{what}: reason");
    let size = unsafe { fs_core_device_size_bytes(lv) };
    let mut got = vec![0u8; size as usize];
    let rc = unsafe { fs_core_device_read_at(lv, 0, got.as_mut_ptr(), got.len()) };
    assert_eq!(rc, FsCoreErrorCode::Ok, "{what}: read through the handle");
    unsafe { fs_core_device_close(lv) };
    got
}

fn same_bytes(what: &str, got: &[u8], expect_path: &Path) {
    let expect = fs::read(expect_path).unwrap_or_else(|e| panic!("{what}: {e}"));
    assert_eq!(
        got.len(),
        expect.len(),
        "{what}: size, ours vs the kernel's"
    );
    if got != expect {
        let at = got.iter().zip(&expect).position(|(a, b)| a != b).unwrap();
        panic!("{what}: first differing byte at {at}");
    }
    println!("lvm oracle: {what} matches the kernel through the C ABI");
}

/// A striped volume opened through `partitions_lvm_open` from
/// `fs_core_file_open` handles, PVs reversed (#164).
#[test]
fn a_striped_volume_reads_the_kernels_bytes_through_the_c_abi() {
    let dir = oracle_dir().join("striped");
    let pvs: Vec<*mut FsCoreDevice> = (0..3)
        .rev()
        .map(|i| file_handle(&dir.join(format!("pv-{i}.img"))))
        .collect();
    let got = lv_through_c("striped/s", &pvs, "s");
    for pv in pvs {
        unsafe { fs_core_device_close(pv) };
    }
    same_bytes("striped/s", &got, &dir.join("s.bin"));
}

/// The Synology layout through nothing but the C ABI: each disk's second
/// partition from `partitions_probe` and `partitions_open_slice`, the
/// RAID5 array from `partitions_md_assemble` with one disk missing, and
/// the volume from `partitions_lvm_open` over the array's handle (#164).
#[test]
fn a_synology_layout_reads_through_gpt_md_and_lvm_in_the_c_abi() {
    let dir = oracle_dir().join("synology");
    for skip in [None, Some(0)] {
        let mut slices = Vec::new();
        for n in (0..3).filter(|&n| Some(n) != skip) {
            let disk = file_handle(&dir.join(format!("disk-{n}.img")));
            let mut list: *mut PartitionList = std::ptr::null_mut();
            assert_eq!(
                unsafe { partitions_probe(disk, &mut list) },
                FsCoreErrorCode::Ok
            );
            let slice = unsafe { partitions_open_slice(list, 1) };
            assert!(!slice.is_null(), "disk {n}: partition 2");
            unsafe {
                partitions_list_free(list);
                fs_core_device_close(disk);
            }
            slices.push(slice);
        }
        let ptrs: Vec<*const FsCoreDevice> = slices.iter().map(|&s| s as *const _).collect();
        let mut array: *mut FsCoreDevice = std::ptr::null_mut();
        let mut reason = -1i32;
        let rc =
            unsafe { partitions_md_assemble(ptrs.as_ptr(), ptrs.len(), &mut array, &mut reason) };
        assert_eq!(
            rc,
            FsCoreErrorCode::Ok,
            "data array without disk {skip:?}: reason {reason}"
        );
        for s in slices {
            unsafe { fs_core_device_close(s) };
        }
        let what = format!("synology/vg1000/lv without disk {skip:?}");
        let got = lv_through_c(&what, &[array], "lv");
        unsafe { fs_core_device_close(array) };
        same_bytes(&what, &got, &dir.join("lv.bin"));
    }
}

/// Every partition of the given Synology-like disk images, as devices.
fn every_partition(dir: &Path, skip: Option<usize>) -> Vec<OwnedSlice> {
    (0..3)
        .filter(|&n| Some(n) != skip)
        .flat_map(|n| {
            let disk: Arc<dyn BlockRead> =
                Arc::new(FileBlock::open(dir.join(format!("disk-{n}.img"))).unwrap());
            let (_, parts) = probe(&*disk).unwrap();
            parts
                .into_iter()
                .map(move |p| OwnedSlice::new(disk.clone(), p.start, p.length))
        })
        .collect()
}

/// Handed only the disk images, discovery finds both md arrays and
/// `vg1000/lv` without being told which partitions belong together, and
/// the volume reads the kernel's bytes (#163).
#[test]
fn a_synology_layout_is_discovered_from_the_disks_alone() {
    let dir = oracle_dir().join("synology");
    for skip in [None, Some(2)] {
        let parts = every_partition(&dir, skip);
        for p in &parts {
            assert!(
                matches!(container::detect(p), Ok(Some(Container::MdMember { .. }))),
                "every partition on these disks is an md member"
            );
        }
        let found = md::scan(parts);
        let refused: Vec<String> = found.refused.iter().map(|(_, e)| e.to_string()).collect();
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(found.arrays.len(), 2, "the system array and the data array");
        let mut arrays = Vec::new();
        for group in found.arrays {
            let array = group.assemble().expect("each array assembles");
            arrays.push(array);
        }
        let levels: Vec<i32> = arrays.iter().map(|a| a.superblock().level).collect();
        assert_eq!(levels, [1, 5]);
        same(
            &format!("discovered system array without disk {skip:?}"),
            &arrays[0],
            &dir.join("system.bin"),
        );
        assert!(
            matches!(
                container::detect(&arrays[1]),
                Ok(Some(Container::LvmPv { .. }))
            ),
            "the data array is a physical volume"
        );

        let vgs = lvm::scan(arrays);
        assert_eq!(vgs.volume_groups.len(), 1);
        assert_eq!(vgs.others.len(), 1, "the system array holds no PV");
        let group = vgs.volume_groups.into_iter().next().unwrap();
        assert_eq!(group.volume_group.name, "vg1000");
        assert!(group.missing.is_empty());
        let names: Vec<&str> = group
            .volume_group
            .logical_volumes
            .iter()
            .map(|l| l.name.as_str())
            .collect();
        assert!(names.contains(&"lv"), "{names:?}");
        let lv = LogicalVolume::open(group.devices, "lv").unwrap();
        same(
            &format!("discovered vg1000/lv without disk {skip:?}"),
            &lv,
            &dir.join("lv.bin"),
        );
    }
}

/// Synology's SHR on disks of two sizes: an md RAID5 over a partition of
/// every disk and an md RAID1 over the larger disks' extra partitions,
/// both PVs of one VG, with the LV running across the two (#166). With
/// each disk missing in turn, both arrays still read.
#[test]
fn a_volume_group_over_two_md_arrays_reads_the_kernels_bytes() {
    let dir = oracle_dir().join("shr");
    let disk = |n: usize| -> Arc<dyn BlockRead> {
        Arc::new(FileBlock::open(dir.join(format!("disk-{n}.img"))).unwrap())
    };
    for skip in [None, Some(0), Some(3)] {
        let mut raid5 = Vec::new();
        let mut raid1 = Vec::new();
        for n in (0..4).filter(|&n| Some(n) != skip) {
            let d = disk(n);
            let (_, parts) = probe(&*d).unwrap();
            raid5.push(OwnedSlice::new(d.clone(), parts[0].start, parts[0].length));
            if n >= 2 {
                raid1.push(OwnedSlice::new(d.clone(), parts[1].start, parts[1].length));
            }
        }
        let raid5 = MdArray::assemble(raid5).unwrap();
        let raid1 = MdArray::assemble(raid1).unwrap();
        assert_eq!(raid5.superblock().level, 5);
        assert_eq!(raid1.superblock().level, 1);
        // In either order: the metadata says which PV is which.
        for order in [[0, 1], [1, 0]] {
            let arrays = [&raid5, &raid1];
            let devs: Vec<&MdArray<OwnedSlice>> = order.iter().map(|&i| arrays[i]).collect();
            let vg = read_volume_group(&devs).unwrap();
            assert_eq!(vg.name, "vgshr");
            assert_eq!(vg.physical_volumes.len(), 2);
            let lv = LogicalVolume::open(devs, "lv").unwrap();
            let pvs_used: std::collections::BTreeSet<&str> = lv
                .info()
                .segments
                .iter()
                .flat_map(|s| s.stripes.iter().map(|st| st.pv.as_str()))
                .collect();
            assert_eq!(pvs_used.len(), 2, "the volume spans both arrays");
            same(
                &format!("shr/vgshr/lv without disk {skip:?}, PVs {order:?}"),
                &lv,
                &dir.join("lv.bin"),
            );
        }
    }
}

/// An in-memory copy of an image, so a test can damage it.
struct Copy(Vec<u8>);
impl BlockRead for Copy {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let o = offset as usize;
        let end = o
            .checked_add(buf.len())
            .filter(|&e| e <= self.0.len())
            .ok_or(fs_core::Error::OutOfBounds {
                offset,
                len: buf.len() as u64,
                size: self.0.len() as u64,
            })?;
        buf.copy_from_slice(&self.0[o..end]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

/// PVs made with `--pvmetadatacopies 2` keep a second copy of the
/// metadata at their end. The volume reads, and still reads when every
/// PV's first copy is damaged and only the second is left (#166).
#[test]
fn a_second_metadata_copy_is_read_when_the_first_is_damaged() {
    let dir = oracle_dir().join("two-mdas");
    let images: Vec<Vec<u8>> = (0..2)
        .map(|n| fs::read(dir.join(format!("pv-{n}.img"))).unwrap())
        .collect();
    let label = read_pv_label(&Copy(images[0].clone())).unwrap().unwrap();
    assert_eq!(
        label.metadata_areas.len(),
        2,
        "lvm2 wrote two metadata areas: {:?}",
        label.metadata_areas
    );
    let lv = LogicalVolume::open(images.iter().cloned().map(Copy).collect(), "m").unwrap();
    same("two-mdas/m", &lv, &dir.join("m.bin"));

    let damaged: Vec<Copy> = images
        .into_iter()
        .map(|mut img| {
            let label = read_pv_label(&Copy(img.clone())).unwrap().unwrap();
            let (first, _) = label.metadata_areas[0];
            // A byte of the first area's header: its checksum fails.
            img[first as usize + 30] ^= 0xff;
            Copy(img)
        })
        .collect();
    let lv = LogicalVolume::open(damaged, "m").unwrap();
    same("two-mdas/m, first copies damaged", &lv, &dir.join("m.bin"));
}
