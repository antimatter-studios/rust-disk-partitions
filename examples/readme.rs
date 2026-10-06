// The README's "Use" example, verbatim below this header.
//
// It lives here so that it is compiled: the README's copy is text, and it
// iterated over `probe`'s result after `probe` began returning the table
// kind beside the partitions, with nothing to notice (#146). `cargo clippy
// --all-targets` builds this file, and tests/readme_example.rs fails if the
// README's block and the code below this header differ.
//
//   cargo run --example readme
use disk_partitions::{probe, sniff, FileBlock};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dev = FileBlock::open("disk.img")?;
    let (table, parts) = probe(&dev)?;
    println!("{table:?}");
    for p in &parts {
        let kind = sniff(&dev, p)?;
        println!("{} bytes @ {} -> {:?}", p.length, p.start, kind);
    }
    Ok(())
}
