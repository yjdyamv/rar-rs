//! Regression: a bare relative archive name (`rar a -v1k set.rar payload`)
//! must commit its staged volume set.
//!
//! `Path::new("set.rar").parent()` is `Some("")`, and the staged multi-volume
//! commit fsyncs that parent on Unix. `File::open("")` fails with ENOENT, so
//! every volume was staged and then rolled back: the command errored and no
//! archive appeared. Windows/WASI no-op the directory fsync, which is why the
//! bug only surfaced on Unix hosts (the existing suites always pass absolute
//! archive paths). The parent is normalized to `.` now.

use std::path::Path;

const RAR_CLI: &str = env!("CARGO_BIN_EXE_rar");

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
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

/// Run `rar a <switches> <bare name> payload.bin` from inside `dir` (so the
/// archive name reaches the library without a directory component) and
/// return every file name that landed next to it.
fn create_with_bare_name(dir: &Path, name: &str, switches: &[&str]) -> Vec<String> {
    std::fs::write(dir.join("payload.bin"), pseudo_random(20 * 1024, 7)).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq"])
        .args(switches)
        .arg(name)
        .arg("payload.bin")
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "{name}: bare relative create must succeed"
    );

    let mut files: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect();
    files.sort();
    files
}

#[test]
fn bare_relative_name_creates_a_rar5_volume_set() {
    let dir = tempfile::tempdir().unwrap();
    let files = create_with_bare_name(dir.path(), "set.rar", &["-m0", "-v1k"]);
    let volumes: Vec<&String> = files.iter().filter(|name| name.ends_with(".rar")).collect();
    assert!(volumes.len() >= 2, "expected multiple volumes: {files:?}");
    assert!(
        volumes.iter().all(|name| name.starts_with("set.part")),
        "{files:?}"
    );
    assert!(
        !files.contains(&"set.rar".to_string()),
        "an unpadded single file means the set was not committed: {files:?}"
    );

    let first = dir.path().join(volumes[0]);
    let reader = rar_rs::ArchiveReader::open(&first).unwrap();
    let names: Vec<String> = reader.entries().map(|e| e.name().to_string()).collect();
    assert!(names.iter().any(|n| n == "payload.bin"), "{names:?}");
}

#[test]
fn bare_relative_name_creates_rar4_and_rar13_volume_sets() {
    // RAR4 defaults to WinRAR's modern `.partNN.rar` naming; RAR13 keeps the
    // old `.rar`/`.rNN` scheme.
    for (switch, label, first, second) in [
        ("-ma4", "RAR4", "set.part1.rar", "set.part2.rar"),
        ("-ma14", "RAR13", "set.rar", "set.r00"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let files = create_with_bare_name(dir.path(), "set.rar", &[switch, "-m0", "-v8k"]);
        assert!(
            files.contains(&first.to_string()),
            "{label}: first volume missing: {files:?}"
        );
        assert!(
            files.contains(&second.to_string()),
            "{label}: expected a second volume: {files:?}"
        );

        let reader = rar_rs::ArchiveReader::open(dir.path().join(first)).unwrap();
        let names: Vec<String> = reader.entries().map(|e| e.name().to_string()).collect();
        assert!(
            names.iter().any(|n| n == "payload.bin"),
            "{label}: {names:?}"
        );
    }
}
