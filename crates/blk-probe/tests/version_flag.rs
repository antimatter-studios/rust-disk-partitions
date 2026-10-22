//! `blk.probe --version` names the tool, its package and its version.
//!
//! Every tool in the family answers `--version` with one line,
//! `<tool> (<package>) <version>`, and that line is how an installer, a
//! package formula's test or a test suite tells this program from another
//! one of the same name on PATH, and checks it is the version it asked for.
//! `blk.probe` had no `--version` at all: it was an unknown flag, exit 1
//! (#47).
//!
//! The binary is run, not the formatting function, because what is checked
//! downstream is what the installed program prints.

use std::process::Command;

const EXPECTED: &str = concat!(
    "blk.probe (",
    env!("CARGO_PKG_NAME"),
    ") ",
    env!("CARGO_PKG_VERSION")
);

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_blk_probe"))
        .args(args)
        .output()
        .expect("run blk_probe")
}

#[test]
fn version_prints_the_tool_its_package_and_its_version() {
    let out = run(&["--version"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "--version exited {:?}; stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{EXPECTED}\n")
    );
    assert!(
        out.stderr.is_empty(),
        "--version wrote to stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn version_wins_over_a_path_and_other_flags() {
    // `--version` anywhere answers the version and probes nothing, so a
    // formula's test never depends on a device it does not have.
    let out = run(&["/nonexistent/disk.img", "--version", "--container=qcow2"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{EXPECTED}\n")
    );
}
