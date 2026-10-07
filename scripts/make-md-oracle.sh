#!/usr/bin/env bash
# Build md arrays with the kernel and record what the kernel reads back,
# for tests/oracle_md.rs to compare against.
#
# Each case is a directory under OUT holding:
#
#   member-<slot>.img     the member image files, detached from the kernel
#   member-<slot>.examine `mdadm --examine` of that member, while assembled
#   array.bin             every byte of /dev/mdX, after random data was
#                         written through it and the array went clean
#   case                  `level=... members=... metadata=... layout=...`
#
# The kernel lays the array out, computes the parity and writes the
# superblocks; this repository only reads the result. That is what makes
# it an oracle rather than this crate checking itself.
#
# NEEDS ROOT AND THE md DRIVER: loop devices and `mdadm --create`. CI runs
# it under sudo in the `oracle (external tools)` job; nothing here skips
# when either is missing.
#
# Usage: sudo scripts/make-md-oracle.sh OUT
set -euo pipefail

out="${1:?usage: make-md-oracle.sh OUT}"
command -v mdadm >/dev/null || { echo "mdadm not found (apt-get install mdadm); this script does not skip" >&2; exit 1; }
# Named here so a missing personality says which, rather than surfacing
# as a create error further down.
modprobe -a raid0 raid1 raid456 raid10 || { echo "cannot load raid0, raid1, raid456 or raid10; this script does not skip" >&2; exit 1; }

rm -rf "$out"
mkdir -p "$out"

MEMBER_MIB=24
next_md=100
loops=()
md=""
cleanup() {
    [ -n "$md" ] && mdadm --stop "$md" >/dev/null 2>&1 || true
    for l in "${loops[@]}"; do losetup -d "$l" 2>/dev/null || true; done
}
trap cleanup EXIT

# make_case NAME LEVEL MEMBERS METADATA [mdadm options...]
#
# Every member is MEMBER_MIB unless SIZES lists one size in MiB per
# member, in slot order.
make_case() {
    local name="$1" level="$2" n="$3" meta="$4"
    shift 4
    local dir="$out/$name"
    local sizes
    read -r -a sizes <<<"${SIZES:-}"
    mkdir -p "$dir"
    loops=()
    for ((s = 0; s < n; s++)); do
        truncate -s "${sizes[$s]:-$MEMBER_MIB}M" "$dir/member-$s.img"
        loops+=("$(losetup --find --show "$dir/member-$s.img")")
    done
    md=/dev/md$next_md
    next_md=$((next_md + 1))
    # Members are listed in slot order, so member-<s> is slot s. Not
    # --quiet: it also silences why mdadm refused a geometry, and a create
    # that fails must say why.
    mdadm --create "$md" --run --level="$level" --raid-devices="$n" \
        --metadata="$meta" "$@" "${loops[@]}" </dev/null
    # Let the initial resync finish, so parity is the kernel's own.
    mdadm --wait "$md" >/dev/null 2>&1 || true
    local bytes
    bytes="$(blockdev --getsize64 "$md")"
    dd if=/dev/urandom of="$md" bs=1M count=$((bytes / 1048576)) iflag=fullblock oflag=direct status=none
    if [ $((bytes % 1048576)) -ne 0 ]; then
        dd if=/dev/urandom of="$md" bs=512 seek=$((bytes / 1048576 * 2048)) \
            count=$(((bytes % 1048576) / 512)) oflag=direct status=none
    fi
    sync
    mdadm --wait "$md" >/dev/null 2>&1 || true
    dd if="$md" of="$dir/array.bin" bs=1M iflag=direct status=none
    for ((s = 0; s < n; s++)); do
        mdadm --examine "${loops[$s]}" >"$dir/member-$s.examine"
    done
    mdadm --stop "$md" >/dev/null
    md=""
    for l in "${loops[@]}"; do losetup -d "$l"; done
    loops=()
    echo "level=$level members=$n metadata=$meta options=$*" >"$dir/case"
    chmod -R a+rX "$dir"
    echo "md oracle: $name ($bytes bytes)"
}

make_case raid1-v1.2 1 2 1.2
make_case raid1-v1.1 1 2 1.1
make_case raid1-v1.0 1 2 1.0
make_case raid1-v0.90 1 2 0.90
make_case raid0-v1.2 0 3 1.2 --chunk=64
make_case raid4-v1.2 4 3 1.2 --chunk=64
make_case raid5-ls-v1.2 5 4 1.2 --chunk=64 --layout=left-symmetric
make_case raid5-la-v1.2 5 3 1.2 --chunk=64 --layout=left-asymmetric
make_case raid5-rs-v1.2 5 3 1.2 --chunk=128 --layout=right-symmetric
make_case raid5-ra-v1.2 5 3 1.2 --chunk=64 --layout=right-asymmetric
make_case raid5-pf-v1.2 5 3 1.2 --chunk=64 --layout=parity-first
make_case raid5-pl-v1.2 5 3 1.2 --chunk=64 --layout=parity-last
make_case raid5-ls-v1.0 5 3 1.0 --chunk=64
make_case raid5-ls-v0.90 5 3 0.90 --chunk=64
make_case raid6-ls-v1.2 6 5 1.2 --chunk=64
make_case raid10-n2-v1.2 10 4 1.2 --chunk=64 --layout=n2
make_case raid10-n2-odd-v1.2 10 3 1.2 --chunk=64 --layout=n2
make_case raid10-n2-v0.90 10 4 0.90 --chunk=64 --layout=n2
make_case raid10-f2-v1.2 10 4 1.2 --chunk=64 --layout=f2
make_case raid10-f2-odd-v1.2 10 3 1.2 --chunk=32 --layout=f2
make_case raid10-o2-v1.2 10 3 1.2 --chunk=64 --layout=o2
make_case raid10-n3-v1.2 10 4 1.2 --chunk=64 --layout=n3
SIZES="40 16 24" make_case raid0-zones-v1.2 0 3 1.2 --chunk=64 --layout=alternate
