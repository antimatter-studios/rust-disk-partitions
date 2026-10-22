#![no_main]
//! A real image wearing the wrong format's signature.
//!
//! Container detection is eight bytes of evidence deciding which parser gets
//! a whole disk. A random fuzzer reaches that decision only by accident --
//! `QFI\xfb` is four exact bytes -- so this target writes a magic on purpose
//! and spends its budget on what happens afterwards: a qcow2 reader handed a
//! raw GPT disk, a VHDX reader handed a squashfs.
use rust_blk_probe_fuzz::spliced;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    spliced(data);
});
