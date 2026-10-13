### Added

- **LVM volumes with `raid10` segments are read (#166).** lvm2's raid10 is
  dm-raid over md's raid10 personality in the near layout; each chunk is
  read from its first near copy, placed by md's own RAID10 geometry. Checked
  byte for byte against a volume lvm2 built and the kernel filled. dm-raid's
  far and offset layouts stay refused.
- **LVM raid and mirror volumes are read with images missing (#166).** An
  image whose PV was not given is read around as md reads around a missing
  member: from another mirror or raid10 copy, or rebuilt from raid5 parity
  or raid6 P and Q, sharing md's own reconstruction. A volume missing more
  images than its level survives names the missing PV. Checked against the
  kernel's bytes with each PV of the oracle left out, and every pair for
  raid6.
- **LVM thin volumes and thin snapshots are read (#166).** A thin volume's
  blocks are looked up in its pool's dm-thin metadata: the superblock, then
  the two-level btree from device to block to pool data block, every
  metadata block's checksum verified. A block never written reads as zeros.
  Checked byte for byte against a thin volume and a thin snapshot lvm2
  built and the kernel filled. A thin volume over an external origin is
  refused.
- **md linear arrays are read (#165).** The members' data areas end to end in
  slot order, each rounded down to the chunk when the superblock records a
  rounding, as the kernel's `linear_conf` sizes them. Checked byte for byte
  against linear arrays mdadm built in metadata 1.2 and 0.90, and one
  rounded to 64 KiB, on a kernel that has the linear personality again
  (it was absent from 6.8 to 6.13).
