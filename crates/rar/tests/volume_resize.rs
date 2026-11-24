//! Reshaping an existing archive into a set of a requested volume size
//! (WinRAR `-v` on an existing archive; the official tools refuse it).

use rar_rs::version::ArchiveVersion;
use rar_rs::{AppendOptions, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

fn store() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn report(label: &str, dir: &std::path::Path, path: &std::path::Path) -> Vec<std::path::PathBuf> {
    eprintln!("--- {label}");
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| {
            let m = e.metadata().unwrap();
            (e.file_name().to_string_lossy().into_owned(), m.len())
        })
        .collect();
    entries.sort();
    for (name, len) in &entries {
        eprintln!("    {name} {len}");
    }
    let volumes = rar_rs::discover_volumes(path);
    eprintln!("    discover_volumes -> {} ({:?})", volumes.len(), volumes);
    volumes
}

#[test]
fn rar5_single_file_becomes_a_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("single.rar");
    {
        let mut writer = ArchiveWriter::create(&path).unwrap();
        writer
            .add_bytes("a.bin", &noise(120_000, 1), store())
            .unwrap();
        writer.finish().unwrap();
    }
    report("created", dir.path(), &path);

    let options = AppendOptions::new().volume_size(40 * 1024);
    let mut writer = ArchiveWriter::append_with(&path, options).unwrap();
    writer
        .add_bytes("b.bin", &noise(80_000, 2), store())
        .unwrap();
    writer.finish().unwrap();

    report("reshaped", dir.path(), &path);
    let volumes = rar_rs::discover_volumes(&path);
    assert!(volumes.len() > 1, "expected a volume set, got {volumes:?}");
}

#[test]
fn rar5_set_is_resized() {
    let dir = tempfile::tempdir().unwrap();
    let created = dir.path().join("set.rar");
    {
        let options = WriterOptions::new().volume_size(50 * 1024);
        let mut writer = ArchiveWriter::create_with(&created, options).unwrap();
        writer
            .add_bytes("a.bin", &noise(120_000, 3), store())
            .unwrap();
        writer.finish().unwrap();
    }
    let first = rar_rs::discover_volumes(&created)[0].clone();
    report("50k", dir.path(), &first);

    let options = AppendOptions::new().volume_size(100 * 1024);
    let mut writer = ArchiveWriter::append_with(&first, options).unwrap();
    writer
        .add_bytes("b.bin", &noise(80_000, 4), store())
        .unwrap();
    writer.finish().unwrap();

    report("100k", dir.path(), &first);
    let volumes = rar_rs::discover_volumes(&first);
    for volume in &volumes[..volumes.len() - 1] {
        assert_eq!(std::fs::metadata(volume).unwrap().len(), 100 * 1024);
    }
}

#[test]
fn rar4_single_file_becomes_a_set_with_new_numbering() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("one.rar");
    {
        let options = WriterOptions::new().compression(ArchiveVersion::V29);
        let mut writer = ArchiveWriter::create_with(&path, options).unwrap();
        writer
            .add_bytes("a.bin", &noise(120_000, 5), store())
            .unwrap();
        writer.finish().unwrap();
    }

    let options = AppendOptions::new().volume_size(40 * 1024);
    let mut writer = ArchiveWriter::append_with(&path, options).unwrap();
    writer
        .add_bytes("b.bin", &noise(80_000, 6), store())
        .unwrap();
    writer.finish().unwrap();

    let volumes = report("rar4 reshaped", dir.path(), &path);
    assert!(volumes.len() > 1, "expected a volume set, got {volumes:?}");
    assert!(
        dir.path().join("one.part1.rar").exists(),
        "a `-v` reshape of a single RAR4 file defaults to the modern names"
    );
    assert!(
        !dir.path().join("one.rar").exists(),
        "the replaced single file is retired"
    );
}

#[test]
fn rar4_set_is_resized() {
    let dir = tempfile::tempdir().unwrap();
    let created = dir.path().join("set4.rar");
    {
        let options = WriterOptions::new()
            .compression(ArchiveVersion::V29)
            .volume_size(50 * 1024);
        let mut writer = ArchiveWriter::create_with(&created, options).unwrap();
        writer
            .add_bytes("a.bin", &noise(120_000, 7), store())
            .unwrap();
        writer.finish().unwrap();
    }
    let first = rar_rs::discover_volumes(&created)[0].clone();
    assert!(report("rar4 50k", dir.path(), &first).len() > 1);

    let options = AppendOptions::new().volume_size(100 * 1024);
    let mut writer = ArchiveWriter::append_with(&first, options).unwrap();
    writer
        .add_bytes("b.bin", &noise(80_000, 8), store())
        .unwrap();
    writer.finish().unwrap();

    let volumes = report("rar4 100k", dir.path(), &first);
    for volume in &volumes[..volumes.len() - 1] {
        assert_eq!(std::fs::metadata(volume).unwrap().len(), 100 * 1024);
    }
}
