//! `blk.probe` — the command-line front of the probe.
//!
//! Everything this program does is in the library beside it: [`probe_path`]
//! opens a disk image (raw, or a qcow2 / VHD / VHDX / VMDK container), walks
//! its partition table and renders the JSON document. This file is argument
//! parsing, the exit-code table, and the decision to write to stdout.
//!
//! THE SPLIT EXISTS SO THE PROBE CAN BE CALLED BY SOMETHING THAT IS NOT A
//! SHELL. It was all in here, which meant a `#[cfg(test)]` module inside this
//! binary was the only thing that could reach it -- no fuzz target, no
//! integration test, no comparison against `blkid` over an image built by
//! `mkfs`. This crate decides what every other parser in the family is handed,
//! and it is the one most likely to be pointed at something that is not a disk
//! image at all (#16).
//!
//! Usage:
//!   blk.probe <path>
//!   blk.probe <path> --container=qcow2|vhd|vhdx|vmdk
//!   blk.probe --version
//!
//! `--version` prints `blk.probe (rust-blk-probe) <version>`, the line every
//! tool in the family answers with, and probes nothing (#47).
//!
//! Exit codes:
//!   0  — JSON written to stdout
//!   1  — argument / option error
//!   2  — file open / container layer error
//!   3  — partition probe error: the table could not be read
//!
//! A disk with *no* partition table is not exit 3. That is a normal answer —
//! a whole-device filesystem — and it exits 0 with `"table":"none"`. Exit 3 is
//! the opposite case: a table is there and this program could not read it, so
//! nothing is written to stdout. The JSON shape itself is documented on the
//! library crate.

use blk_probe::{probe_path, Container, ProbeError};
use fs_core::cli::{version, Family};

const USAGE: &str = "usage: blk.probe <path> [--container=qcow2|vhd|vhdx|vmdk] | --version";

/// The name a person types; the cargo target is `blk_probe` only because
/// cargo refuses a dot (see Cargo.toml).
const TOOL: &str = "blk.probe";

/// What `--version` reports, in the shape the family's shared CLI plumbing
/// defines. No dotted tools are listed: this binary does not use the
/// multi-call dispatch, only the identifying line.
static FAMILY: Family = Family {
    repo: env!("CARGO_PKG_NAME"),
    crate_name: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    about: env!("CARGO_PKG_DESCRIPTION"),
    install_hints: &[
        "the release tarball at https://github.com/antimatter-studios/rust-disk-partitions/releases (tags blk-probe-v*)",
    ],
    tools: &[],
};

// The exit-code table, named. It is published in three places — the module
// doc-comment above, the README, and here — and the point of naming the
// codes is that the `die` call sites say which contract line they are.
const EXIT_ARG_ERROR: i32 = 1;
const EXIT_OPEN_ERROR: i32 = 2;
const EXIT_PROBE_ERROR: i32 = 3;

fn die(code: i32, msg: &str) -> ! {
    eprintln!("blk.probe: {msg}");
    std::process::exit(code);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Anywhere on the line, and before anything else is looked at, so a
    // version check never depends on a device being there.
    if args.iter().any(|a| a == "--version") {
        println!("{}", version::line(&FAMILY, TOOL));
        std::process::exit(0);
    }
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        std::process::exit(0);
    }
    let mut path: Option<String> = None;
    let mut explicit: Option<Container> = None;
    for a in &args {
        if let Some(rest) = a.strip_prefix("--container=") {
            match Container::parse(rest) {
                Some(c) => explicit = Some(c),
                None => die(EXIT_ARG_ERROR, &format!("unknown container kind: {rest}")),
            }
        } else if a.starts_with("--") {
            die(EXIT_ARG_ERROR, &format!("unknown flag: {a}\n{USAGE}"));
        } else if path.is_none() {
            path = Some(a.clone());
        } else {
            die(EXIT_ARG_ERROR, &format!("unexpected positional: {a}"));
        }
    }
    let path = path.unwrap_or_else(|| die(EXIT_ARG_ERROR, USAGE));

    match probe_path(&path, explicit) {
        Ok(report) => {
            // Before the document, in the order they happened, which is where
            // they were printed when this was one function: a reader watching
            // stderr sees the warning and then the line it qualifies.
            for warning in &report.warnings {
                eprintln!("blk.probe: {warning}");
            }
            println!("{}", report.json);
        }
        Err(e @ ProbeError::Open(_)) => die(EXIT_OPEN_ERROR, &e.to_string()),
        Err(e @ ProbeError::Probe(_)) => die(EXIT_PROBE_ERROR, &e.to_string()),
    }
}
