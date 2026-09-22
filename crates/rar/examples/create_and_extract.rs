//! The ordinary library flow: create an archive, inspect it, read one member,
//! extract it, and verify the result.
//!
//! Run with `cargo run --example create_and_extract`.

use rar_rs::{ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, ExtractOptions};

fn main() -> rar_rs::RarResult<()> {
    let dir = std::env::temp_dir().join("rar-rs-create-and-extract");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(dir.join("src/main.rs"), b"fn main() {}\n")?;
    std::fs::write(dir.join("README.md"), b"# example\n")?;

    // Create: per-archive options on the writer, per-member options on each
    // `add_*` call. `finish()` commits; dropping the writer instead abandons
    // the archive (nothing is left at the final path).
    let archive = dir.join("example.rar");
    let mut writer = ArchiveWriter::create(&archive)?;
    let stored = EntryWriteOptions::new().compression_level(CompressionLevel::STORE);
    let normal = EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL);
    writer.add_path(dir.join("src"), normal)?;
    writer.add_bytes("README.md", b"# example\n", stored)?;
    let report = writer.finish()?;
    println!("wrote {}", report.primary_path().display());

    // Read: entries are addressed by `EntryId`, which a rewrite invalidates;
    // `unique_entry(name)` is the shortcut for "the one member called X".
    let mut reader = ArchiveReader::open(&archive)?;
    for reference in reader.entries() {
        let entry = reference.metadata();
        println!(
            "  {} ({} bytes packed, {} unpacked, {})",
            entry.name(),
            entry.compressed_size(),
            entry.size(),
            entry.method_name()
        );
    }
    let id = reader.unique_entry("README.md")?;
    println!("README.md starts with {:?}", &reader.read_entry(id)?[..7]);

    // Extract: streaming into a caller-provided writer keeps memory bounded for
    // a huge member, and `extract_all_with_options` writes the tree.
    let mut sink = Vec::new();
    reader.copy_entry_to(id, &mut sink)?;
    assert_eq!(sink, b"# example\n");
    let out = dir.join("out");
    let extracted = reader.extract_all_with_options(
        &out,
        ExtractOptions {
            threads: Some(2),
            ..Default::default()
        },
    )?;
    println!(
        "extracted {} members to {}",
        extracted.written_count(),
        out.display()
    );

    // Verify: re-read every member and check its stored integrity record.
    let verification = reader.verify()?;
    println!(
        "verified: {} ok, {} failed",
        verification.passed(),
        verification.failed()
    );
    Ok(())
}
