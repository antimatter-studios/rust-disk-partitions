//! A container reader that refuses an image has already consumed the handle
//! it was given, and the probe must not close it again (#32).
//!
//! All four `*_open_on_device` functions reclaim the handle box on entry,
//! before they parse anything, so a NULL return means the inner device is
//! already freed. Closing it on that path is a double free: a truncated VHDX
//! turned into a SIGBUS in `Arc::drop` under `fs_core_device_close`.
//!
//! `tests/fuzz_decoders.rs` replays that image, but only through
//! `probe_bytes`, over a callback device. The crash was found through the
//! binary, which opens a FILE and goes through `probe_path` -- and nothing
//! drove that path over an image a reader refuses. This file does, for every
//! container, and repeats each refusal so that a double free has more than
//! one allocation to trip over. A regression aborts the test binary, which
//! fails the tier; it cannot pass quietly.

use std::path::PathBuf;

use blk_probe::{probe_bytes, probe_path, Container, ProbeError};

/// How many times each refusal is repeated. One double free can land on a
/// block the allocator has not reused yet and go unnoticed; a run of them
/// cannot.
const ROUNDS: usize = 64;

fn corpus(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fuzz/corpus/probe")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// An image each reader is handed explicitly and refuses.
///
/// `vhdx-head.img` is the image #32 was found with: the first 320 KiB of a
/// real VHDX, whose log region reaches past the end of the file. The other
/// three are real images of their kind cut to one sector, so each reader
/// recognises what it is given and then fails on the rest.
fn refused_images() -> Vec<(Container, &'static str, Vec<u8>)> {
    let head = |name: &str| corpus(name)[..512].to_vec();
    vec![
        (Container::Vhdx, "vhdx-head.img", corpus("vhdx-head.img")),
        (Container::Qcow2, "gpt.qcow2[..512]", head("gpt.qcow2")),
        (
            Container::Vhd,
            "gpt-fixed.vhd[..512]",
            head("gpt-fixed.vhd"),
        ),
        (Container::Vmdk, "gpt.vmdk[..512]", head("gpt.vmdk")),
    ]
}

fn assert_refused_by_reader(
    what: &str,
    kind: Container,
    got: Result<blk_probe::Report, ProbeError>,
) {
    let want = format!("{}_open_on_device: ", kind.label());
    match got {
        Err(ProbeError::Open(msg)) if msg.starts_with(&want) => {}
        Err(e) => panic!(
            "{what}: expected the {} reader to refuse it, got {e}",
            kind.label()
        ),
        Ok(_) => panic!(
            "{what}: expected the {} reader to refuse it, it probed",
            kind.label()
        ),
    }
}

#[test]
fn a_file_the_reader_refuses_is_not_closed_twice() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("consumed_handle");
    std::fs::create_dir_all(&dir).unwrap();
    for (kind, what, bytes) in refused_images() {
        let path = dir.join(format!("refused.{}", kind.label()));
        std::fs::write(&path, &bytes).unwrap();
        let path = path.to_str().expect("temp path is UTF-8");
        for _ in 0..ROUNDS {
            assert_refused_by_reader(what, kind, probe_path(path, Some(kind)));
        }
    }
}

#[test]
fn a_buffer_the_reader_refuses_is_not_closed_twice() {
    for (kind, what, bytes) in refused_images() {
        for _ in 0..ROUNDS {
            assert_refused_by_reader(what, kind, probe_bytes(&bytes, Some(kind), what));
        }
    }
}
