#!/usr/bin/env bash
# Build LVM2 volume groups with the kernel, including one stacked on md
# the way a Synology volume is, and record what the kernel reads back,
# for tests/oracle_lvm.rs to compare against.
#
#   linear/    one PV, two LVs; `a` is extended after `b`, so it has two
#              segments that are not adjacent on the PV
#   striped/   three PVs, one LV striped across them at 64 KiB
#   synology/  three GPT disks: partition 1 of each is an md RAID1 with
#              0.90 metadata (the system volume), partition 2 an md RAID5
#              with 1.2 metadata, which is an LVM PV holding vg1000/lv
#   shr/       four GPT disks of two sizes, the way Synology's SHR builds
#              them: an md RAID5 over a partition of every disk, an md
#              RAID1 over the extra partition of the two larger disks, and
#              one VG over both arrays with vgshr/lv spanning them
#   two-mdas/  two PVs made with `--pvmetadatacopies 2`, so each keeps a
#              second copy of the metadata at its end; one LV across both
#   raid/      five PVs holding LVs whose segments are dm-raid and
#              dm-mirror rather than striped: r1 (raid1, two images), r5
#              (raid5, two data stripes), r6 (raid6, three data stripes),
#              r10 (raid10, two stripes of two near copies) and m1
#              (mirror, two images, core log)
#
# Each case holds the member images (`pv-<n>.img` or `disk-<n>.img`),
# and `<name>.bin` for each volume: every byte the kernel's device returned
# after random data was written through it.
#
# NEEDS ROOT, THE md DRIVER AND DEVICE-MAPPER. CI runs it under sudo in
# the `oracle (external tools)` job; nothing here skips.
#
# Usage: sudo scripts/make-lvm-oracle.sh OUT
set -euo pipefail

out="${1:?usage: make-lvm-oracle.sh OUT}"
for t in mdadm pvcreate sfdisk; do
    command -v "$t" >/dev/null || { echo "$t not found (apt-get install mdadm lvm2 fdisk); this script does not skip" >&2; exit 1; }
done
# Named here so a missing one says which, rather than surfacing as a
# create error further down.
modprobe -a raid1 raid456 raid10 dm_mod dm_raid dm_mirror || { echo "cannot load raid1, raid456, raid10, dm_mod, dm_raid or dm_mirror; this script does not skip" >&2; exit 1; }

# Only the devices this script made: a host's own PVs are never touched,
# and no devices file decides what is visible.
LVM=(--devicesfile "")

rm -rf "$out"
mkdir -p "$out"
loops=()
vgs=()
mds=()
cleanup() {
    for v in "${vgs[@]}"; do vgchange "${LVM[@]}" -an "$v" >/dev/null 2>&1 || true; done
    for m in "${mds[@]}"; do mdadm --stop "$m" >/dev/null 2>&1 || true; done
    for l in "${loops[@]}"; do losetup -d "$l" 2>/dev/null || true; done
}
trap cleanup EXIT

attach() {
    local l
    l="$(losetup --find --show --partscan "$1")"
    loops+=("$l")
    echo "$l"
}

# fill DEVICE NAME DIR: random data through DEVICE, then every byte of it.
fill() {
    local dev="$1" bytes
    bytes="$(blockdev --getsize64 "$dev")"
    dd if=/dev/urandom of="$dev" bs=64K count=$((bytes / 65536)) iflag=fullblock oflag=direct status=none
    sync
    dd if="$dev" of="$3/$2.bin" bs=64K iflag=direct status=none
    echo "lvm oracle: $(basename "$3")/$2 ($bytes bytes)"
}

finish() {
    local vg="$1"
    vgchange "${LVM[@]}" -an "$vg" >/dev/null
    vgs=()
}

# --- linear: two segments of one LV, out of order on the PV ------------
d="$out/linear"
mkdir -p "$d"
truncate -s 32M "$d/pv-0.img"
p0="$(attach "$d/pv-0.img")"
pvcreate "${LVM[@]}" -q "$p0"
vgcreate "${LVM[@]}" -q -s 1M oracle-linear "$p0"
vgs+=(oracle-linear)
lvcreate "${LVM[@]}" -q -y -L 4M -n a oracle-linear
lvcreate "${LVM[@]}" -q -y -L 8M -n b oracle-linear
lvextend "${LVM[@]}" -q -L +4M oracle-linear/a
fill /dev/oracle-linear/a a "$d"
fill /dev/oracle-linear/b b "$d"
finish oracle-linear

# --- striped: three PVs, 64 KiB stripes ---------------------------------
d="$out/striped"
mkdir -p "$d"
pvs=()
for n in 0 1 2; do
    truncate -s 24M "$d/pv-$n.img"
    pvs+=("$(attach "$d/pv-$n.img")")
done
pvcreate "${LVM[@]}" -q "${pvs[@]}"
vgcreate "${LVM[@]}" -q -s 1M oracle-striped "${pvs[@]}"
vgs+=(oracle-striped)
lvcreate "${LVM[@]}" -q -y -i 3 -I 64k -L 30M -n s oracle-striped
fill /dev/oracle-striped/s s "$d"
finish oracle-striped

# --- synology: GPT disks, md RAID1 system + md RAID5 data + LVM --------
d="$out/synology"
mkdir -p "$d"
parts1=()
parts2=()
for n in 0 1 2; do
    truncate -s 64M "$d/disk-$n.img"
    printf 'label: gpt\nsize=16MiB, type=A19D880F-05FC-4D3B-A006-743F0F84911E\ntype=A19D880F-05FC-4D3B-A006-743F0F84911E\n' \
        | sfdisk -q "$d/disk-$n.img"
    l="$(attach "$d/disk-$n.img")"
    parts1+=("${l}p1")
    parts2+=("${l}p2")
done
udevadm settle || true
mdadm --create /dev/md120 --run --level=1 --raid-devices=3 --metadata=0.90 "${parts1[@]}" </dev/null
mds+=(/dev/md120)
mdadm --create /dev/md121 --run --level=5 --raid-devices=3 --metadata=1.2 --chunk=64 "${parts2[@]}" </dev/null
mds+=(/dev/md121)
mdadm --wait /dev/md120 /dev/md121 >/dev/null 2>&1 || true
fill /dev/md120 system "$d"
pvcreate "${LVM[@]}" -q /dev/md121
vgcreate "${LVM[@]}" -q -s 4M vg1000 /dev/md121
vgs+=(vg1000)
lvcreate "${LVM[@]}" -q -y -l 100%FREE -n lv vg1000
fill /dev/vg1000/lv lv "$d"
mdadm --wait /dev/md121 >/dev/null 2>&1 || true
finish vg1000
for m in "${mds[@]}"; do mdadm --stop "$m" >/dev/null; done
mds=()

# --- shr: one VG over two md arrays, from disks of two sizes -----------
d="$out/shr"
mkdir -p "$d"
raid5=()
raid1=()
for n in 0 1 2 3; do
    if [ "$n" -lt 2 ]; then
        truncate -s 48M "$d/disk-$n.img"
        printf 'label: gpt\nsize=40MiB, type=A19D880F-05FC-4D3B-A006-743F0F84911E\n' \
            | sfdisk -q "$d/disk-$n.img"
    else
        truncate -s 80M "$d/disk-$n.img"
        printf 'label: gpt\nsize=40MiB, type=A19D880F-05FC-4D3B-A006-743F0F84911E\nsize=32MiB, type=A19D880F-05FC-4D3B-A006-743F0F84911E\n' \
            | sfdisk -q "$d/disk-$n.img"
    fi
    l="$(attach "$d/disk-$n.img")"
    raid5+=("${l}p1")
    [ "$n" -lt 2 ] || raid1+=("${l}p2")
done
udevadm settle || true
mdadm --create /dev/md122 --run --level=5 --raid-devices=4 --metadata=1.2 --chunk=64 "${raid5[@]}" </dev/null
mds+=(/dev/md122)
mdadm --create /dev/md123 --run --level=1 --raid-devices=2 --metadata=1.2 "${raid1[@]}" </dev/null
mds+=(/dev/md123)
mdadm --wait /dev/md122 /dev/md123 >/dev/null 2>&1 || true
pvcreate "${LVM[@]}" -q /dev/md122 /dev/md123
vgcreate "${LVM[@]}" -q -s 4M vgshr /dev/md122 /dev/md123
vgs+=(vgshr)
lvcreate "${LVM[@]}" -q -y -l 100%FREE -n lv vgshr
fill /dev/vgshr/lv lv "$d"
mdadm --wait /dev/md122 /dev/md123 >/dev/null 2>&1 || true
finish vgshr
for m in "${mds[@]}"; do mdadm --stop "$m" >/dev/null; done
mds=()

# --- two-mdas: a second metadata copy at the end of each PV -------------
d="$out/two-mdas"
mkdir -p "$d"
pvs=()
for n in 0 1; do
    truncate -s 24M "$d/pv-$n.img"
    pvs+=("$(attach "$d/pv-$n.img")")
done
pvcreate "${LVM[@]}" -q --pvmetadatacopies 2 "${pvs[@]}"
vgcreate "${LVM[@]}" -q -s 1M oracle-two-mdas "${pvs[@]}"
vgs+=(oracle-two-mdas)
lvcreate "${LVM[@]}" -q -y -l 100%FREE -n m oracle-two-mdas
fill /dev/oracle-two-mdas/m m "$d"
finish oracle-two-mdas

# --- raid: dm-raid and dm-mirror segments --------------------------------
d="$out/raid"
mkdir -p "$d"
pvs=()
for n in 0 1 2 3 4; do
    truncate -s 32M "$d/pv-$n.img"
    pvs+=("$(attach "$d/pv-$n.img")")
done
pvcreate "${LVM[@]}" -q "${pvs[@]}"
vgcreate "${LVM[@]}" -q -s 1M oracle-raid "${pvs[@]}"
vgs+=(oracle-raid)
lvcreate "${LVM[@]}" -q -y --type raid1 -m 1 -L 8M -n r1 oracle-raid
lvcreate "${LVM[@]}" -q -y --type raid5 -i 2 -L 8M -n r5 oracle-raid
lvcreate "${LVM[@]}" -q -y --type raid6 -i 3 -L 12M -n r6 oracle-raid
lvcreate "${LVM[@]}" -q -y --type raid10 -i 2 -m 1 -L 8M -n r10 oracle-raid
lvcreate "${LVM[@]}" -q -y --type mirror -m 1 --mirrorlog core -L 8M -n m1 oracle-raid
# synced LV: wait until the kernel reports every copy and parity in step,
# so what is read back is what the kernel keeps on every member.
synced() {
    local lv="$1" i pct
    for i in $(seq 1 120); do
        pct="$(lvs "${LVM[@]}" --noheadings -o sync_percent "oracle-raid/$lv" | tr -d ' ')"
        [ "$pct" = "100.00" ] && return 0
        sleep 1
    done
    echo "oracle-raid/$lv did not finish syncing ($pct%); this script does not skip" >&2
    exit 1
}
for lv in r1 r5 r6 r10 m1; do
    synced "$lv"
    fill "/dev/oracle-raid/$lv" "$lv" "$d"
    synced "$lv"
done
# What the kernel was told: the metadata lvm2 wrote, and each device-mapper
# table, whose dm-raid lines carry the level, layout, chunk and data offset.
vgcfgbackup "${LVM[@]}" -q -f "$d/vg.txt" oracle-raid
dmsetup table | grep '^oracle--raid-' > "$d/dm.table"
finish oracle-raid

for l in "${loops[@]}"; do losetup -d "$l"; done
loops=()
chmod -R a+rX "$out"
