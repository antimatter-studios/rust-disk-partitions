// Shared by both fuzzing tiers, included textually rather than depended on.
//
// `tests/fuzz_decoders.rs` and `fuzz/src/lib.rs` both `include!` this file. A
// crate dependency would have been tidier, but the fuzz crate depends on
// `libfuzzer-sys`, which builds libFuzzer's C++ runtime, and making the gate
// depend on the fuzz crate would drag that into every pull request build on
// the stable toolchain.
//
// What matters is that the two tiers probe an image identically, so a
// reproducer from one reproduces in the other.

/// The four container signatures, in the order the probe tests them, for the
/// splicing target to choose from.
///
/// They are named here rather than read from `blk_probe::magic` on purpose:
/// this is the fuzzer's idea of what a container magic is, and if the probe's
/// list and this one ever disagree the splicing target starts producing
/// images no container claims -- which is a weaker input, not a failure, and
/// nothing would say so. `the_spliced_magics_are_the_probes_own` in the gate
/// compares them.
pub const SPLICE_MAGICS: [&[u8]; 4] = [b"QFI\xfb", b"vhdxfile", b"KDMV", b"conectix"];

/// One whole probe of an image held in memory.
///
/// This is the entire sequence a hostile image meets -- decide the container
/// from the magic at offset 0 (or the footer at the end), stack that
/// container's reader over the bytes, walk the partition table, sniff each
/// partition -- because each step decides what the next one is handed. A
/// target that only parsed the header would never reach the walk.
///
/// The result is discarded. A crafted image is *supposed* to be refused; what
/// it may not do is panic, hang, or read memory that is not the image.
pub fn probe(image: &[u8]) {
    let _ = blk_probe::probe_bytes(image, None, "fuzz");
}

/// A valid image of one type wearing another type's magic.
///
/// The first byte of `data` chooses which signature is written over the start
/// of the rest. This is the input a purely random fuzzer almost never
/// produces -- eight bytes have to be exactly right before the probe hands
/// the image to a container reader at all -- and it is the one that crosses
/// the formats: a qcow2 reader given a raw GPT disk, a VHDX reader given a
/// squashfs.
///
/// A short input is still probed. The probe is called with whatever there is,
/// because "two bytes" is a thing a device can be.
pub fn spliced(data: &[u8]) {
    let Some((selector, rest)) = data.split_first() else {
        probe(data);
        return;
    };
    let magic = SPLICE_MAGICS[usize::from(*selector) % SPLICE_MAGICS.len()];
    let mut image = rest.to_vec();
    let take = magic.len().min(image.len());
    image[..take].copy_from_slice(&magic[..take]);
    probe(&image);
}
