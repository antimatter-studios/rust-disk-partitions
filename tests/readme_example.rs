//! The README's "Use" example is the compiled `examples/readme.rs`.
//!
//! The README is text, so nothing compiled it, and it drifted: it iterated
//! over `probe`'s result after `probe` began returning `(TableKind,
//! Vec<Partition>)` (#146). Now the example is a file `cargo clippy
//! --all-targets` builds, and this holds the README's ```rust block to it.

use std::path::PathBuf;

fn read(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The body of the first fenced block opened with exactly "```rust".
fn rust_block(markdown: &str) -> String {
    let mut lines = markdown.lines().skip_while(|l| l.trim_end() != "```rust");
    assert!(lines.next().is_some(), "README.md has no ```rust block");
    let body: Vec<&str> = lines.take_while(|l| l.trim_end() != "```").collect();
    body.join("\n") + "\n"
}

#[test]
fn the_readme_rust_example_is_the_compiled_example() {
    let example = read("examples/readme.rs");
    let code: String = example
        .lines()
        .skip_while(|l| l.starts_with("//"))
        .skip_while(|l| l.is_empty())
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(
        rust_block(&read("README.md")),
        code,
        "README.md's ```rust block differs from examples/readme.rs below its header; \
         change both together"
    );
}
