#!/usr/bin/env bash
# Rebuild tests/images/4kn-gpt.img from a table `sfdisk` wrote at 4096
# bytes per sector, and record what sfdisk says about it.
#
# WHY THE IMAGE IS COMMITTED RATHER THAN BUILT BY THE TEST. A 4096-byte
# -sector (4Kn) GPT is the one table shape no tool on an ordinary runner
# will build into a plain file. `sfdisk --sector-size 4096` writes one,
# and that option arrived in util-linux **2.40**; ubuntu-latest ships
# 2.39.3 and rejects it outright. `sgdisk` has no equivalent at all and
# assumes 512 for a regular file, and the script format's
# `sector-size:` header is not a substitute -- measured on util-linux
# 2.41, a script carrying it is written at 512 bytes per sector
# regardless. The remaining route is a loop device opened with
# `losetup --sector-size 4096`, which needs root and would cost the
# oracle the property that makes it runnable anywhere: no loop device,
# no root, every tool reading a plain file (#123).
#
# So the tool that made the fixture is RECORDED rather than REQUIRED at
# test time. That is the same arrangement `scripts/make-fuzz-corpus.sh`
# already uses for the disks in `fuzz/corpus/`: real tables real tools
# wrote, replayed on machines that have none of those tools.
#
# `tests/images/4kn-gpt.json` is `sfdisk --json`'s own description of
# the image, plus the version of the tool that produced it. It is the
# oracle: `tests/sector_size.rs` compares the bytes and this crate's
# behaviour against what sfdisk said, not against what this crate
# thinks.
#
# Usage: scripts/make-4kn-fixture.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
images="$here/tests/images"

command -v sfdisk >/dev/null || {
    echo "sfdisk not found: it ships in util-linux, and 2.40 or later is" >&2
    echo "needed for --sector-size. This script does not skip." >&2
    exit 1
}

version="$(sfdisk --version)"
# `--sector-size` is the whole reason for the version floor, so ask the
# tool rather than parsing its version string: an option that is there
# answers, and one that is not says so.
if ! sfdisk --help 2>&1 | grep -q -- '--sector-size'; then
    echo "this sfdisk has no --sector-size, so it cannot write a 4Kn table:" >&2
    echo "  $version" >&2
    echo "util-linux 2.40 or later is needed. Debian trixie, Ubuntu 24.10 and" >&2
    echo "Fedora 40 carry it; ubuntu-latest on GitHub does not, which is why" >&2
    echo "the image this script writes is committed rather than rebuilt." >&2
    exit 1
fi

# THE TOOL IS RUN FROM THE IMAGE'S OWN DIRECTORY, so the `device` and
# `node` fields of the record below name `4kn-gpt.img` rather than
# whichever absolute path the person regenerating it happened to have.
# A record that differs per machine is a record nobody can diff.
mkdir -p "$images"
cd "$images"
img="4kn-gpt.img"
rm -f "$img"

# 256 KiB is 64 LBAs of 4096 bytes, which is a whole disk as far as a
# partition table is concerned: the entry array is 16 KiB (4 LBAs) at
# each end, so the usable range still has room for the two partitions
# below. Small, and it compresses to nothing in the repository.
truncate -s 256K "$img"

# Two partitions, of two different types, with names -- enough for the
# refusal under test to be about a table that really describes
# something, and enough for the record below to have fields in it.
printf 'label: gpt\n%s\n%s\n' \
    'start=12, size=20, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="root"' \
    'start=32, size=10, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="esp"' \
    | sfdisk --sector-size 4096 "$img" >/dev/null

# THE RECORD. `sfdisk --json` describes the table it just wrote; the
# version line says which tool produced the description. Both are read
# by tests/sector_size.rs.
{
    printf '{\n  "produced_by": "%s",\n  "sfdisk": ' "$version"
    sfdisk --sector-size 4096 --json "$img" | sed 's/^/  /' | sed '1s/^  //'
    printf '}\n'
} > "4kn-gpt.json"

echo "wrote $images/$img and $images/4kn-gpt.json with $version"
