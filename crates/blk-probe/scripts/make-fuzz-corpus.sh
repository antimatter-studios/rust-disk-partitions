#!/usr/bin/env bash
# Rebuild fuzz/corpus/ from images third-party tools wrote.
#
# WHY THE SEEDS ARE OTHER PEOPLE'S IMAGES. This crate decides what an unknown
# device IS, and whatever it decides sends the bytes to some other parser. A
# corpus this crate wrote would only prove it agrees with itself; `sgdisk`,
# `sfdisk`, `mkfs.ext4`, `mksquashfs`, `mkfs.xfs`, `mkfs.btrfs`, `mkfs.erofs`
# and `qemu-img` have no stake in our
# reading of the formats, so a seed that stops probing is evidence about us.
#
# The same images are the oracle in tests/fuzz_decoders.rs: every committed
# file has a row there naming the container and the table the tool that wrote
# it put in it, so a seed cannot quietly stop being a disk and go on being
# mutated -- a mutation of an unprobeable image is also unprobeable, and the
# corpus would still be there, testing nothing.
#
# IMAGES ARE DELIBERATELY SMALL. A GPT needs 33 sectors at either end and
# nothing in between has to be real, so 128 KiB is a whole disk as far as a
# partition table is concerned. The one exception is noted where it is made.
#
# Usage: scripts/make-fuzz-corpus.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/blk.probe-fuzz-corpus.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# NOTHING SKIPS. A missing tool fails this script naming the tool, rather
# than producing a corpus that is quietly one seed short.
for tool in sgdisk sfdisk mkfs.ext4 mksquashfs mkfs.xfs mkfs.btrfs mkfs.erofs \
    qemu-img truncate; do
    command -v "$tool" >/dev/null || {
        echo "$tool not found; it writes part of the corpus" >&2
        exit 1
    }
done

probe="$here/fuzz/corpus/probe"
spliced="$here/fuzz/corpus/spliced"
rm -rf "$here/fuzz/corpus"
mkdir -p "$probe" "$spliced"

# --- a GPT disk, from sgdisk ----------------------------------------------
# The GUIDs are pinned so that rebuilding the corpus produces the same bytes;
# left to sgdisk they are fresh random ones and every rebuild is a diff.
truncate -s 128K "$probe/gpt.img"
sgdisk -o \
    -U 00000000-1111-2222-3333-444444444444 \
    -n 1:34:100  -t 1:8300 -c 1:root -u 1:11111111-1111-1111-1111-111111111111 \
    -n 2:101:200 -t 2:8200 -c 2:swap -u 2:22222222-2222-2222-2222-222222222222 \
    "$probe/gpt.img" >/dev/null 2>&1

# --- an MBR disk, from sfdisk ---------------------------------------------
# Two primaries. The type bytes are 0x83 (Linux) and 0x82 (swap), so the
# `type_byte` field in the document has something in it that means something.
# 128 KiB is 256 sectors, so the partitions live in the low hundreds rather
# than at the 1 MiB alignment a real disk would use -- sfdisk refuses a start
# past the end of the image, which is the honest constraint of a disk this
# small. `label-id` is pinned for the same reason the GPT GUIDs are.
truncate -s 128K "$probe/mbr.img"
sfdisk --no-tell-kernel "$probe/mbr.img" >/dev/null 2>&1 <<'SFDISK'
label: dos
label-id: 0x12345678
start=64, size=64, type=83
start=128, size=64, type=82
SFDISK

# --- the same GPT disk in each container format ---------------------------
# THE CONTAINER IS THE FIRST DECISION THIS CRATE MAKES, and it is made from
# the magic at offset 0 -- except for a fixed VHD, which is a raw image with a
# 512-byte footer glued on the end and is therefore the one format whose
# signature is not at the start. Both shapes are here on purpose.
qemu-img convert -O qcow2 "$probe/gpt.img" "$probe/gpt.qcow2"
qemu-img convert -O vmdk  "$probe/gpt.img" "$probe/gpt.vmdk"
qemu-img convert -O vpc -o subformat=fixed "$probe/gpt.img" "$probe/gpt-fixed.vhd"

# --- a VHDX, truncated, and why -------------------------------------------
# qemu-img writes 9 MiB for a VHDX of a 128 KiB disk however it is asked: the
# format's log and metadata regions have a floor a small disk does not lower.
# Committing that is out of proportion to what it buys, so what is kept is the
# first 320 KiB -- the file identifier, both headers and both region tables,
# which is every structure a reader parses before it reaches payload.
#
# It is therefore a VHDX THAT CANNOT BE OPENED, and tests/fuzz_decoders.rs
# says so: the expectation on this seed is that the container is recognised
# and the open then fails. That is a real path -- a device that says it is a
# VHDX and is not one is exactly what this crate is pointed at -- and it is
# the header bytes, not the payload, that are worth mutating.
qemu-img convert -O vhdx "$probe/gpt.img" "$work/full.vhdx"
head -c 327680 "$work/full.vhdx" > "$probe/vhdx-head.img"

# --- whole-device filesystems, with no partition table at all -------------
# `"table":"none"` is not an error in this crate's contract, it is an answer,
# and it is the answer that reaches the whole-device sniff. Both of these are
# ones `blkid` recognises, so the expectations in the gate are checkable
# against a tool that is not us.
truncate -s 512K "$probe/ext4.img"
mkfs.ext4 -q -F -L probe-ext4 -U 11111111-2222-3333-4444-555555555555 \
    "$probe/ext4.img" 2>/dev/null

mkdir -p "$work/empty"
mksquashfs "$work/empty" "$probe/squashfs.img" -noappend -no-progress \
    -all-time 0 -mkfs-time 0 >/dev/null 2>&1

# --- XFS, Btrfs and EROFS, which this crate identifies itself -------------
# These three are recognised from their superblocks in src/superblock.rs,
# not by rust-disk-partitions, so they need seeds of their own for the gate to
# mutate. Every UUID and timestamp is pinned, as above.
#
# XFS AND BTRFS ARE HEADS, because neither mkfs writes anything small:
# mkfs.xfs refuses under 300 MiB and mkfs.btrfs under about 114 MiB. The XFS
# head is its first 4 KiB -- the superblock and the three AG headers behind
# it. The Btrfs head is 1 MiB, not the 68 KiB that reaches past its
# superblock, because blkid refuses to call anything shorter than 1 MiB a
# Btrfs, and tests/oracle_tools.rs holds these exact files to blkid's answer.
# Nearly all of it is zeroes and compresses to about 1 KiB.
truncate -s 320M "$work/xfs.img"
mkfs.xfs -q -f -m uuid=11111111-2222-3333-4444-555555555555 "$work/xfs.img"
head -c 4096 "$work/xfs.img" > "$probe/xfs-head.img"

truncate -s 128M "$work/btrfs.img"
mkfs.btrfs -q -f -U 11111111-2222-3333-4444-555555555555 \
    --device-uuid 22222222-3333-4444-5555-666666666666 "$work/btrfs.img"
head -c 1048576 "$work/btrfs.img" > "$probe/btrfs-head.img"

# EROFS is built from a directory, so an empty one makes a whole image of one
# block. The block size is pinned because it defaults to the page size.
mkfs.erofs --quiet -b 4096 -T 0 -U 11111111-2222-3333-4444-555555555555 \
    "$probe/erofs.img" "$work/empty"

# --- the spliced corpus ----------------------------------------------------
# The second target's input is a selector byte followed by an image: the byte
# picks a container magic, which is written over the first bytes of the image.
# A valid disk wearing another format's signature is the input that makes this
# crate hand a qcow2 reader a raw GPT, and it is the one shape a purely random
# fuzzer almost never produces -- four bytes have to be exactly right before
# anything interesting happens.
i=0
for base in gpt.img squashfs.img; do
    printf "$(printf '\\x%02x' "$i")" > "$spliced/$base"
    cat "$probe/$base" >> "$spliced/$base"
    i=$(( i + 1 ))
done

echo "corpus rebuilt:"
find "$here/fuzz/corpus" -type f | sort | while read -r f; do
    printf '  %8d  %s\n' "$(wc -c < "$f")" "${f#"$here"/}"
done
