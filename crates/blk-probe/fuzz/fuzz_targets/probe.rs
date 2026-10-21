#![no_main]
//! An unknown device, probed from the first byte.
//!
//! This crate runs before anything about a device has been established: it
//! reads the magic at offset 0, decides what the whole thing is, and whatever
//! it decides sends the bytes to a container reader, a partition-table walker
//! and a filesystem sniffer in turn. It is also the component most likely to
//! be pointed at something that is not a disk image at all.
//!
//! So the target is the whole probe over arbitrary bytes, not a header
//! parser: the interesting failures are in what the second step does with
//! what the first step decided.
use rust_blk_probe_fuzz::probe;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    probe(data);
});
