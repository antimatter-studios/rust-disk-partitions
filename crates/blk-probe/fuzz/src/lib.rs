//! Shared setup for the fuzz targets.
//!
//! The probe entry points live in `fuzz/shared/probe.rs`, which
//! `tests/fuzz_decoders.rs` includes too -- see that file for why it is
//! shared textually rather than as a dependency.

include!("../shared/probe.rs");
