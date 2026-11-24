//! Shared fixtures for the CLI behavior suites: temp dirs, the built
//! binary paths and the helpers that drive them.
use rar_rs::{CompressionLevel, EntryWriteOptions};
use std::path::Path;

pub(crate) fn make_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// Run a command that execs a **freshly copied** binary, retrying Linux's
/// `ETXTBSY` window.
///
/// Linux refuses `execve` while any process holds a write descriptor for the
/// image (`deny_write_access`). A copy closes its descriptor before the spawn,
/// but on a busy box the kernel can still surface `ETXTBSY` for a just-written
/// image: it was seen once in a full `--all-features` workspace run on a
/// 16-core WSL2 box and never on a quiet run of the same binary. Go's
/// `os/exec` retries the same error for the same reason; the window is
/// microseconds, so a few retries are plenty.
pub(crate) fn status_retrying_busy(
    command: &mut std::process::Command,
) -> std::process::ExitStatus {
    for attempt in 0..10u64 {
        match command.status() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(5 * (attempt + 1)));
            }
            other => return other.expect("spawn command"),
        }
    }
    panic!("spawning {command:?} kept failing with ETXTBSY");
}
pub(crate) const RAR_CLI: &str = env!("CARGO_BIN_EXE_rar");

/// A **release** `rar-sfx` stub, built once per test process.
///
/// The debug ELF carries ~40 MiB of DWARF, which does not fit the reader's
/// 8 MiB SFX scan window (the archive sits right after the stub), while the
/// stripped release build is a few MiB. LTO is disabled for this one build so
/// the stub appears quickly; `strip` still keeps it small.
///
/// Built for the test suite's own target: CI runs this suite under
/// `--target i686-unknown-linux-gnu`, where a host-architecture stub would
/// neither be found (cargo nests it under the triple directory) nor run.
pub(crate) fn sfx_stub() -> &'static str {
    static STUB: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STUB.get_or_init(|| {
        let debug_bin = std::path::Path::new(env!("CARGO_BIN_EXE_rar"));
        let profile_dir = debug_bin
            .parent()
            .expect("the rar binary's debug directory");
        // `<target>/<triple>/debug` under an explicit `--target`, `<target>/debug`
        // otherwise; only the former needs the nested build to repeat the triple.
        let cross = profile_dir
            .parent()
            .and_then(std::path::Path::file_name)
            .is_some_and(|name| name == std::ffi::OsStr::new(env!("RAR_SFX_TARGET")));
        let mut command = std::process::Command::new(env!("CARGO"));
        command.args([
            "build",
            "--release",
            "--locked",
            "--config",
            "profile.release.lto=false",
            "-p",
            "rar-cli",
            "--bin",
            "rar-sfx",
        ]);
        if cross {
            command.args(["--target", env!("RAR_SFX_TARGET")]);
        }
        let status = command
            .status()
            .expect("run cargo build for the rar-sfx stub");
        assert!(status.success(), "building the release rar-sfx stub failed");
        let target = profile_dir
            .parent()
            .expect("target directory above the debug binaries");
        target
            .join("release")
            .join(format!("rar-sfx{}", std::env::consts::EXE_SUFFIX))
            .to_string_lossy()
            .into_owned()
    })
}

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
