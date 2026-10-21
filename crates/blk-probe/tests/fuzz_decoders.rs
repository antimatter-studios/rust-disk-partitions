//! The stable-toolchain half of the fuzzing setup: replay the corpus, then
//! mutate it, and refuse if the probe panics, hangs, or if the suite quietly
//! stopped doing any work.
//!
//! # Why there are two halves
//!
//! `fuzz/` holds `cargo-fuzz` targets. Those are the explorer: they run for as
//! long as you give them and find inputs nobody thought of. They cannot be a
//! required check, because how long they ran decides what they found, and a
//! fresh discovery would fail whichever unrelated pull request happened to be
//! open.
//!
//! This suite is the gate. Deterministic, on the stable toolchain, in every
//! pull request, reading the same `fuzz/corpus/` the explorer does. Anything
//! the explorer finds is committed there and replayed here from then on --
//! that is the whole reason the two tiers share one corpus directory.
//!
//! # Why the corpus is whole disks other tools wrote
//!
//! This crate is the first thing to touch an unknown device. It decides what
//! the whole thing is from the magic at offset 0, and whatever it decides
//! sends the bytes to a container reader, a partition walker and a filesystem
//! sniffer in turn. A corpus this crate wrote would only prove it agrees with
//! itself, so the seeds are images `sgdisk`, `sfdisk`, `mkfs.ext4`,
//! `mksquashfs` and `qemu-img` produced, built by
//! `scripts/make-fuzz-corpus.sh`.
//!
//! They are also an ORACLE, not just fuel. `every_committed_image_is_read_as
//! _what_the_tool_that_wrote_it_says` holds each one to the container and the
//! table its maker put there, on a machine with none of those tools
//! installed. Without it a seed that stopped probing would go on being
//! mutated and go on not failing -- a mutation of an unreadable image is also
//! unreadable -- and the corpus would still be there, testing nothing.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

// The two probe entry points, shared verbatim with the explorer. See
// fuzz/shared/probe.rs for why it is included rather than depended on.
include!("../fuzz/shared/probe.rs");

/// Distinct starting points for the mutation stream. Fixed, so a failure
/// reproduces from the message alone.
const SEEDS: u64 = 4;

/// Mutated cases per (corpus file, seed) pair. Measured at 6.8 seconds for
/// the whole suite on an aarch64 Linux host, which is inside what a gate in
/// every pull request can spend -- a case here opens a container, walks a
/// partition table and sniffs every partition it found, so it is not free.
const CASES_PER_SEED: usize = 384;

/// Below this, the suite is not doing its job. MEASURED: 15,360 cases -- ten
/// seeds across two targets, four mutation streams each, 384 cases per
/// stream. The floor is a little under that so an added seed does not have to
/// move it, and it only ever goes UP: lowered to make a run pass, it would
/// have stopped measuring anything.
const CASE_FLOOR: usize = 15_000;

/// Long enough that a loaded machine is never the reason, short enough that a
/// genuine hang is reported rather than left to the job timeout.
const DEADLINE: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------- targets

struct Target {
    corpus: &'static str,
    name: &'static str,
    run: fn(&[u8]),
}

fn targets() -> Vec<Target> {
    vec![
        Target {
            corpus: "probe",
            name: "probe",
            run: probe,
        },
        Target {
            corpus: "spliced",
            name: "spliced",
            run: spliced,
        },
    ]
}

// ---------------------------------------------------------------- corpus

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus")
}

fn seeds(corpus: &str) -> Vec<(String, Vec<u8>)> {
    let dir = corpus_root().join(corpus);
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading the corpus directory {}: {e}", dir.display()))
        .map(|entry| {
            let path = entry.expect("corpus directory entry").path();
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("reading the seed {}: {e}", path.display()));
            let name = path
                .file_name()
                .expect("seed file name")
                .to_string_lossy()
                .into_owned();
            (name, bytes)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------- mutation

/// xorshift64*. Small, deterministic, and not a dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

/// One mutation of a real image, PRESERVING LENGTH.
///
/// A hostile image controls what is in a block; it does not control how many
/// bytes the device hands back. Feeding short buffers to a probe that is only
/// ever called with a whole device produces noise instead of findings -- the
/// read is refused before any of the parsing is reached.
///
/// The `header` bias exists because a disk is mostly payload: a uniformly
/// random offset in a 512 KiB image lands in a data block nine times out of
/// ten, where nothing parses it. Half the mutations are aimed at the first
/// 8 KiB, which is where the container header, the protective MBR, the GPT
/// header and the start of the entry array are.
fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        return out;
    }
    let metadata_end = out.len().min(8192);
    let region = if rng.next() & 1 == 0 {
        metadata_end
    } else {
        out.len()
    };

    match rng.below(5) {
        0 => {
            for _ in 0..=rng.below(8) {
                let at = rng.below(region);
                out[at] ^= 1u8 << rng.below(8);
            }
        }
        1 => {
            let at = rng.below(region);
            let len = 1 + rng.below(16.min(out.len() - at));
            let fill = if rng.next() & 1 == 0 { 0x00 } else { 0xff };
            out[at..at + len].fill(fill);
        }
        2 => {
            let width = [2usize, 4, 8][rng.below(3)];
            if out.len() >= width {
                let at = rng.below(region.saturating_sub(width) + 1) & !(width - 1);
                if at + width <= out.len() {
                    let value: u64 = match rng.below(4) {
                        0 => 0,
                        1 => 1,
                        2 => u64::MAX,
                        _ => rng.next(),
                    };
                    // Little-endian: every multi-byte field in a partition
                    // table and in three of the four container headers is.
                    out[at..at + width].copy_from_slice(&value.to_le_bytes()[..width]);
                }
            }
        }
        3 => {
            if out.len() >= 8 {
                let a = rng.below(region / 4) * 4;
                let b = rng.below(region / 4) * 4;
                if a + 4 <= out.len() && b + 4 <= out.len() {
                    for i in 0..4 {
                        out.swap(a + i, b + i);
                    }
                }
            }
        }
        _ => {
            if out.len() >= 4 {
                let at = rng.below(region / 4) * 4;
                if at + 4 <= out.len() {
                    let word = u32::from_le_bytes(out[at..at + 4].try_into().expect("4 bytes"));
                    let delta = [1i64, -1, 2, -2, 255, -255][rng.below(6)];
                    let changed = (i64::from(word).wrapping_add(delta)) as u32;
                    out[at..at + 4].copy_from_slice(&changed.to_le_bytes());
                }
            }
        }
    }
    out
}

/// The case in flight, readable even if the lock was poisoned by the panic we
/// are trying to describe.
fn describe(current: &Arc<Mutex<String>>) -> String {
    match current.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

// ---------------------------------------------------------------- tests

#[test]
fn every_target_has_a_corpus() {
    for target in targets() {
        assert!(
            !seeds(target.corpus).is_empty(),
            "the target {} reads fuzz/corpus/{}, which holds no seeds -- a target with an \
             empty corpus runs no cases and would pass in silence. Rebuild it with \
             scripts/make-fuzz-corpus.sh",
            target.name,
            target.corpus,
        );
    }
}

/// What each committed image is, according to the tool that wrote it.
///
/// `container`, `table`, and how many partitions the table holds. `None` for
/// the partition count means the probe is expected to REFUSE the image, which
/// is a real answer and the only honest one for the truncated VHDX -- see
/// scripts/make-fuzz-corpus.sh for why that seed is a head rather than a
/// whole file.
const WHAT_THE_TOOLS_WROTE: &[(&str, &str, &str, Option<usize>)] = &[
    // sgdisk wrote two partitions; qemu-img converted the same disk into each
    // container, so the four rows below it must all report the same table.
    ("gpt.img", "raw", "gpt", Some(2)),
    ("gpt.qcow2", "qcow2", "gpt", Some(2)),
    ("gpt.vmdk", "vmdk", "gpt", Some(2)),
    ("gpt-fixed.vhd", "vhd", "gpt", Some(2)),
    ("mbr.img", "raw", "mbr", Some(2)),
    // No partition table at all, which is an ANSWER and not a failure: these
    // are whole-device filesystems, and `blkid` calls them ext4 and squashfs.
    ("ext4.img", "raw", "none", Some(0)),
    ("squashfs.img", "raw", "none", Some(0)),
    // The heads of an XFS and a Btrfs, and a whole EROFS: the three whose
    // superblock rules this tool carried until they moved into
    // rust-disk-partitions' sniff (#150). Heads, because neither mkfs writes
    // anything small.
    ("xfs-head.img", "raw", "none", Some(0)),
    ("btrfs-head.img", "raw", "none", Some(0)),
    ("erofs.img", "raw", "none", Some(0)),
    // Recognised as a VHDX and then refused, because it is the first 320 KiB
    // of one.
    ("vhdx-head.img", "vhdx", "", None),
];

/// The whole-device sniff must name the filesystem its maker made, for the
/// seeds that are filesystems. These are the strings `blkid -o export`
/// reports as `TYPE=` for the same files.
const WHAT_BLKID_CALLS_THEM: &[(&str, &str)] = &[
    ("ext4.img", "ext4"),
    ("squashfs.img", "squashfs"),
    ("xfs-head.img", "xfs"),
    ("btrfs-head.img", "btrfs"),
    ("erofs.img", "erofs"),
];

/// Every committed image is read as what the tool that wrote it says it is.
///
/// This is the oracle-independence rule applied to the corpus: not one of
/// these bytes was written by this crate, and the expectations are what
/// `sgdisk -p`, `sfdisk -l`, `qemu-img info` and `blkid -o export` report for
/// the same files -- recorded when the corpus was built, and checked here on
/// a machine with none of those tools installed.
#[test]
fn every_committed_image_is_read_as_what_the_tool_that_wrote_it_says() {
    let images = seeds("probe");
    assert_eq!(
        images.len(),
        WHAT_THE_TOOLS_WROTE.len(),
        "the probe corpus holds {} images, not the {} scripts/make-fuzz-corpus.sh builds -- \
         a seed added without a row here is a seed nothing checks",
        images.len(),
        WHAT_THE_TOOLS_WROTE.len(),
    );

    for (name, bytes) in images {
        let (_, container, table, partitions) = WHAT_THE_TOOLS_WROTE
            .iter()
            .find(|(n, _, _, _)| *n == name)
            .unwrap_or_else(|| panic!("{name} is in the corpus and has no row saying what it is"));

        let outcome = blk_probe::probe_bytes(&bytes, None, &name);

        let Some(want_count) = partitions else {
            // The container is still identified from the magic; it is the
            // OPEN that has to fail, and with the container's name in it.
            assert_eq!(
                blk_probe::detect_container_in(&bytes).label(),
                *container,
                "{name}: the container magic is no longer recognised",
            );
            match outcome {
                Err(blk_probe::ProbeError::Open(detail)) => {
                    assert!(
                        detail.contains(container),
                        "{name}: refused, but the message does not name {container}: {detail}",
                    );
                }
                other => panic!("{name}: expected the open to fail, got {other:?}"),
            }
            continue;
        };

        let report =
            outcome.unwrap_or_else(|e| panic!("{name}: a real image would not probe: {e}"));
        let json = &report.json;
        assert!(
            json.contains(&format!("\"container\":\"{container}\"")),
            "{name}: not read as a {container} container: {json}",
        );
        assert!(
            json.contains(&format!("\"table\":\"{table}\"")),
            "{name}: not read as a {table} table: {json}",
        );
        assert_eq!(
            json.matches("\"index\":").count(),
            *want_count,
            "{name}: found a different number of partitions than the tool wrote: {json}",
        );

        if let Some((_, fs)) = WHAT_BLKID_CALLS_THEM.iter().find(|(n, _)| *n == name) {
            assert!(
                json.contains(&format!("\"device_fs_kind\":\"{fs}\"")),
                "{name}: blkid calls this {fs}; this probe does not: {json}",
            );
        }
    }
}

/// The signatures the splicing target writes are the ones the probe looks
/// for.
///
/// If the two lists drift, that target starts producing images no container
/// claims. Nothing would fail -- it would just stop testing the thing it
/// exists to test, which is the quietest way for a fuzz target to die.
#[test]
fn the_spliced_magics_are_the_probes_own() {
    let probes: Vec<&[u8]> = vec![
        blk_probe::magic::QCOW2,
        blk_probe::magic::VHDX,
        blk_probe::magic::VMDK,
        blk_probe::magic::VHD,
    ];
    assert_eq!(
        SPLICE_MAGICS.to_vec(),
        probes,
        "fuzz/shared/probe.rs splices signatures the probe does not look for",
    );
    // And each one really is recognised, so a typo in either list is caught
    // by more than a comparison of the two.
    for magic in SPLICE_MAGICS {
        let mut image = vec![0u8; 4096];
        image[..magic.len()].copy_from_slice(magic);
        assert_ne!(
            blk_probe::detect_container_in(&image).label(),
            "raw",
            "the magic {magic:?} is not recognised by the probe",
        );
    }
}

#[test]
fn deterministic_mutations_of_real_images_are_survived() {
    let cases = Arc::new(AtomicUsize::new(0));
    let current = Arc::new(Mutex::new(String::from("(not started)")));
    let (done_tx, done_rx) = mpsc::channel();

    let hook_current = Arc::clone(&current);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("\nfuzz gate: panicked at {}", describe(&hook_current));
        previous_hook(info);
    }));

    let worker_cases = Arc::clone(&cases);
    let worker_current = Arc::clone(&current);
    let worker = std::thread::spawn(move || {
        for target in targets() {
            for (seed_name, bytes) in seeds(target.corpus) {
                for start in 0..SEEDS {
                    let mut rng = Rng::new(start);
                    for case in 0..CASES_PER_SEED {
                        *worker_current.lock().expect("progress lock") =
                            format!("{} / {seed_name} / seed {start} / case {case}", target.name);
                        let mutated = mutate(&bytes, &mut rng);
                        (target.run)(&mutated);
                        worker_cases.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        let _ = done_tx.send(());
    });

    // A timeout means the worker is still running: a hang. A disconnect means
    // it panicked, and the panic is what is worth reporting.
    match done_rx.recv_timeout(DEADLINE) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Disconnected) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Written to the process's stderr rather than through `eprintln!`,
            // which the harness captures into a buffer it only prints when a
            // test finishes -- and exiting here means it never finishes.
            let _ = writeln!(
                std::io::stderr(),
                "\nhung: no progress for {:?} at {}\n\
                 A reader did not return. A container header whose block table \
                 points back at itself looks exactly like this.",
                DEADLINE,
                describe(&current),
            );
            let _ = std::io::stderr().flush();
            std::process::exit(1);
        }
    }

    let outcome = worker.join();
    let _ = std::panic::take_hook();
    if outcome.is_err() {
        panic!("the probe panicked at {}", describe(&current));
    }

    let total = cases.load(Ordering::Relaxed);
    assert!(
        total >= CASE_FLOOR,
        "only {total} mutated cases ran, below the floor of {CASE_FLOOR} -- the target list \
         or the corpus has collapsed, and a suite that runs nothing passes quickly",
    );
    eprintln!("{total} mutated cases");
}

#[test]
fn the_gate_covers_every_explorer_target() {
    // The two tiers drift apart the moment somebody adds a cargo-fuzz target
    // and forgets that nothing gates it on the stable toolchain.
    let manifest =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/Cargo.toml"))
            .expect("reading fuzz/Cargo.toml");

    let explorer: Vec<String> = manifest
        .lines()
        .filter_map(|line| line.strip_prefix("name = \""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .map(str::to_owned)
        .skip(1) // the package name is the first `name =` in the file
        .collect();

    assert!(
        !explorer.is_empty(),
        "fuzz/Cargo.toml declares no [[bin]] targets",
    );

    let gated: Vec<&str> = targets().iter().map(|t| t.name).collect();
    for name in &explorer {
        assert!(
            gated.contains(&name.as_str()),
            "fuzz/fuzz_targets/{name}.rs has no counterpart in this suite, so nothing replays \
             its corpus on the stable toolchain and anything it finds would only stay fixed \
             for as long as somebody keeps running the fuzzer by hand",
        );
    }
}
