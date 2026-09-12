//! Shared fixtures for the CLI behavior suites: temp dirs, the built
//! binary paths and the helpers that drive them.
use rar_rs::{CompressionLevel, EntryWriteOptions};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// Serializes tests that (a) write `rarfiles.lst` next to the rar binary
/// (`cli_rarfiles_lst_orders_solid_members`) with (b) tests whose member
/// order would be corrupted if a stray `rarfiles.lst` were present
/// (`cli_se_preserves_input_order`, and the `-s` round-trip test). Without
/// it the parallel test threads race on `target/debug/rarfiles.lst` and the
/// order-sensitive tests flake.
pub(crate) fn rarfiles_lst_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) fn make_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}
pub(crate) const RAR_CLI: &str = env!("CARGO_BIN_EXE_rar");

pub(crate) fn make_tree(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("f1.txt"), b"one").unwrap();
    std::fs::write(dir.join("f2.tmp"), b"two").unwrap();
    std::fs::write(dir.join("sub/f3.txt"), b"three").unwrap();
    std::fs::write(dir.join("sub/f4.bin"), b"four").unwrap();
}

pub(crate) fn create_duplicate_archive(path: &Path) {
    let mut rar =
        rar_rs::ArchiveWriter::create_with(path, rar_rs::WriterOptions::default()).unwrap();
    rar.add_bytes(
        "same.bin",
        b"first payload",
        EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
    )
    .unwrap();
    rar.add_bytes(
        "same.bin",
        b"second payload",
        EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0).unwrap()),
    )
    .unwrap();
    rar.finish().unwrap();
}

pub(crate) fn cli_names(archive: &std::path::Path) -> Vec<String> {
    let rar = rar_rs::ArchiveReader::open(archive).unwrap();
    let mut names: Vec<String> = rar
        .entries()
        .map(|e| e.name().to_string())
        .map(|n| n.trim_end_matches('/').to_string())
        .collect();
    names.sort();
    names
}

pub(crate) const UNRAR_CLI: &str = env!("CARGO_BIN_EXE_unrar");

/// Set a file's mtime to `secs_ago` seconds in the past.
pub(crate) fn set_mtime_ago(path: &Path, secs_ago: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(t))
        .unwrap();
}

/// Write a 32 MiB file of repeated text (compressible, uniform head so the
/// incompressibility probe does not misfire).
pub(crate) fn write_rep_text(path: &Path, size: usize) {
    let block = b"The quick brown fox jumps over the lazy dog 0123456789.\r\n";
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        data.extend_from_slice(block);
    }
    data.truncate(size);
    std::fs::write(path, data).unwrap();
}

pub(crate) fn entry_dict_log(archive: &Path, name: &str) -> u8 {
    let rar = rar_rs::ArchiveReader::open(archive).unwrap();
    rar.entry(rar.unique_entry(name).unwrap())
        .unwrap()
        .comp_dict_size()
}

/// Deterministic pseudo-random bytes (LCG) — incompressible.
pub(crate) fn pseudo_random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

pub(crate) fn created_time(path: &Path) -> Option<std::time::SystemTime> {
    #[cfg(windows)]
    {
        std::fs::metadata(path).ok()?.created().ok()
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}
