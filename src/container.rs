//! What a device holds when it is a piece of something larger rather than
//! a filesystem: an `md` member or an LVM2 physical volume.
//!
//! [`sniff`](crate::sniff()) answers "which filesystem starts here", and a
//! member of an array or a PV answers it with [`FsKind::Unknown`] — or,
//! worse, with the filesystem of the array's data when a RAID1 member keeps
//! its superblock at the end (metadata 0.90 and 1.0). This is a separate
//! probe rather than two more [`FsKind`] variants because `FsKind` is
//! exhaustive, so adding a variant breaks every `match` on it; a container is
//! not a filesystem anyway, and a caller asks the two questions for different
//! reasons — to mount, or to assemble.
//!
//! [`FsKind`]: crate::FsKind
//! [`FsKind::Unknown`]: crate::FsKind::Unknown

use std::fmt;

use crate::lvm::{self, LvmError};
use crate::md::{self, MdError};
use fs_core::BlockRead;

/// A volume-management container found on a device.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Container {
    /// A member of the `md` array with this UUID, at this RAID level.
    MdMember { array_uuid: [u8; 16], level: i32 },
    /// An LVM2 physical volume with this UUID (dashes removed, as the
    /// label stores it).
    LvmPv { uuid: String },
}

/// Why a container that is there could not be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum ContainerError {
    /// An `md` superblock is present but damaged, or the device failed.
    Md(MdError),
    /// An LVM2 label is present but damaged, or the device failed.
    Lvm(LvmError),
}

impl fmt::Display for ContainerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContainerError::Md(e) => write!(f, "md: {e}"),
            ContainerError::Lvm(e) => write!(f, "lvm: {e}"),
        }
    }
}

impl std::error::Error for ContainerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ContainerError::Md(e) => Some(e),
            ContainerError::Lvm(e) => Some(e),
        }
    }
}

/// Which container `dev` is, if any.
pub fn detect<R: BlockRead + ?Sized>(_dev: &R) -> Result<Option<Container>, ContainerError> {
    let _ = (md::MD_MAGIC, lvm::lvm_crc(&[]));
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::tests::{pv_image, uuid, vg_text, DEV};
    use crate::md::tests::{raid1_member, Mem};

    #[test]
    fn an_md_member_is_named_with_its_array() {
        assert_eq!(
            detect(&raid1_member(7, 0, 1, 0)).unwrap(),
            Some(Container::MdMember {
                array_uuid: [7; 16],
                level: 1
            })
        );
    }

    #[test]
    fn a_physical_volume_is_named_with_its_uuid() {
        let pv = Mem(pv_image(&uuid(0), &vg_text(1, ""), 512));
        assert_eq!(
            detect(&pv).unwrap(),
            Some(Container::LvmPv { uuid: uuid(0) })
        );
    }

    #[test]
    fn a_blank_device_holds_no_container() {
        assert_eq!(detect(&Mem(vec![0u8; DEV])).unwrap(), None);
    }

    /// A member whose data starts at byte 0 carries whatever the array
    /// holds at its own start — here a PV label — as well as the md
    /// superblock. It is the md member that the device is.
    #[test]
    fn an_md_member_wins_over_what_its_data_holds() {
        let mut m = raid1_member(7, 0, 1, 0);
        let pv = pv_image(&uuid(0), &vg_text(1, ""), 512);
        m.0[..4096].copy_from_slice(&pv[..4096]);
        assert!(matches!(
            detect(&m).unwrap(),
            Some(Container::MdMember { .. })
        ));
    }

    #[test]
    fn a_damaged_superblock_is_an_error_not_a_blank_device() {
        let mut m = raid1_member(7, 0, 1, 0);
        m.0[4096 + 72] ^= 1;
        assert!(matches!(
            detect(&m),
            Err(ContainerError::Md(MdError::BadChecksum { .. }))
        ));
    }
}
