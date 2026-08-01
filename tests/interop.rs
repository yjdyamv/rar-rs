//! Integration tests for archive creation, extraction, and WinRAR interop.
//!
//! `tests/fixtures/winrar5_multiple_files.rar` is a RAR5 archive created by
//! WinRAR, vendored from the libarchive test suite
//! (`test_read_format_rar5_multiple_files.rar`, BSD-2-Clause licensed,
//! https://github.com/libarchive/libarchive).

use rar5::RarArchive;

const FIXTURE: &str = "tests/fixtures/winrar5_multiple_files.rar";
const FIXTURE_FILES: [(&str, &str); 4] = [
    (
        "test1.bin",
        "7d89f86f9f69d744ffff3fc043e15bf89fc3ffc134ffcbb31d164a99bb8b67b0",
    ),
    (
        "test2.bin",
        "f81e6fceeeab366306b23466bf6bb3aac2875e0906dc20a8652be0696ceb15a2",
    ),
    (
        "test3.bin",
        "5e621f2b6ce8fed758c3df8221f994eda55d1e432c7cc4349c34a30ec2e1c43d",
    ),
    (
        "test4.bin",
        "2627f40180217252956edb9a426e8d3e344adaf89019d3bccbe04f6c3416dcdd",
    ),
];

fn sha256(data: &[u8]) -> String {
    // sha2 is a dependency of the library; expose it via the already-linked
    // crates by re-hashing through the std-only path: build hex manually is
    // overkill, so use `sha2` from the dependency graph via `use sha2::Digest`.
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

fn make_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[test]
fn reads_winrar5_fixture_and_extracts_byte_identical_data() {
    let mut rar = RarArchive::open(FIXTURE).expect("open fixture");
    let entries = rar.list();
    assert_eq!(entries.len(), 4);
    for (name, _) in FIXTURE_FILES {
        assert!(
            entries.iter().any(|e| e.name() == name),
            "missing entry {name}"
        );
    }

    for (name, expected_sha) in FIXTURE_FILES {
        let data = rar.read(name).expect("read entry");
        assert_eq!(sha256(&data), *expected_sha, "content mismatch for {name}");
    }
}

#[test]
fn create_read_roundtrip_matches_input() {
    let dir = make_temp_dir();
    let path = dir.path().join("rt.rar");
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();

    {
        let mut rar = RarArchive::create(&path).expect("create");
        rar.add_bytes("data.bin", &payload, 5).expect("add");
        rar.add_bytes("note.txt", b"hello", 5).expect("add");
        rar.close().expect("close");
    }

    let mut rar = RarArchive::open(&path).expect("open");
    assert_eq!(rar.list().len(), 2);
    let out = rar.read("data.bin").expect("read");
    assert_eq!(out, payload);
    assert_eq!(rar.read("note.txt").expect("read"), b"hello");
}

#[test]
fn encrypted_archive_roundtrip() {
    let dir = make_temp_dir();
    let path = dir.path().join("enc.rar");
    let payload = b"classified content".repeat(1000);

    {
        let mut rar =
            RarArchive::create_with_password(&path, "hunter2").expect("create encrypted");
        rar.add_bytes("secret.txt", &payload, 3).expect("add");
        rar.close().expect("close");
    }

    // Without the password the entry must refuse to decrypt.
    let mut rar = RarArchive::open(&path).expect("open");
    assert!(
        rar.read("secret.txt").is_err(),
        "reading an encrypted entry without a password must fail"
    );

    // With the password it must round-trip.
    let mut rar = RarArchive::open_with_password(&path, "hunter2").expect("open encrypted");
    assert_eq!(rar.read("secret.txt").expect("read"), payload);
}

#[test]
fn multivolume_creation_roundtrip() {
    let dir = make_temp_dir();
    let base = dir.path().join("vol.rar");
    // Incompressible payload so the volumes actually fill up.
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut payload = vec![0u8; 500_000];
    for b in payload.iter_mut() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        *b = (state.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8;
    }

    {
        let mut rar = RarArchive::create_multivolume(&base, 262_144).expect("create volumes");
        rar.add_bytes("big.bin", &payload, 5).expect("add");
        rar.close().expect("close");
    }

    let volumes: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("vol.part"))
        .collect();
    assert!(
        volumes.len() >= 2,
        "expected multiple volume files, got {:?}",
        volumes.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );

    let mut rar = RarArchive::open(&base).expect("open first volume");
    assert_eq!(rar.read("big.bin").expect("read"), payload);
}

#[test]
fn add_as_uses_custom_archive_name() {
    let dir = make_temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("sub")).expect("mkdir");
    std::fs::write(src.join("a.txt"), b"aaa").expect("write");

    let path = dir.path().join("named.rar");
    {
        let mut rar = RarArchive::create(&path).expect("create");
        rar.add_as(src.join("a.txt"), "docs/renamed.txt", 3).expect("add");
        rar.add_as(src, "root", 3).expect("add");
        rar.close().expect("close");
    }

    let mut rar = RarArchive::open(&path).expect("open");
    let names: Vec<String> = rar.list().iter().map(|e| e.name().to_string()).collect();
    assert!(
        names.iter().any(|n| n == "docs/renamed.txt"),
        "missing renamed entry: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "root/"),
        "missing dir entry: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "root/sub/"),
        "missing nested dir entry: {names:?}"
    );
    assert_eq!(
        rar.read("docs/renamed.txt").expect("read"),
        b"aaa".to_vec()
    );
}

#[test]
fn add_directory_only_writes_dir_entries_without_children() {
    let dir = make_temp_dir();
    let src = dir.path().join("tree");
    std::fs::create_dir_all(src.join("empty")).expect("mkdir");
    std::fs::write(src.join("empty", "ignored.txt"), b"x").expect("write");
    std::fs::write(src.join("top.txt"), b"y").expect("write");

    let path = dir.path().join("dironly.rar");
    {
        let mut rar = RarArchive::create(&path).expect("create");
        // Directory entry only — the child must NOT be pulled in.
        rar.add_directory_only(&src, "tree").expect("add dir");
        rar.add_as(src.join("top.txt"), "tree/top.txt", 3).expect("add file");
        rar.close().expect("close");
    }

    let mut rar = RarArchive::open(&path).expect("open");
    let names: Vec<String> = rar.list().iter().map(|e| e.name().to_string()).collect();
    assert!(names.iter().any(|n| n == "tree/"), "missing dir: {names:?}");
    assert!(
        names.iter().any(|n| n == "tree/empty/"),
        "missing empty dir: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("ignored.txt")),
        "child leaked in: {names:?}"
    );
    assert_eq!(rar.read("tree/top.txt").expect("read"), b"y".to_vec());
}

#[test]
fn progress_callback_reports_monotonic_progress() {
    let dir = make_temp_dir();
    let path = dir.path().join("prog.rar");
    let payload: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();

    let events: std::sync::Arc<std::sync::Mutex<Vec<(u64, u64)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let mut rar = RarArchive::create(&path).expect("create");
        let sink = events.clone();
        let cb: Box<dyn FnMut(u64, u64) + Send> = Box::new(move |done, total| {
            sink.lock().expect("lock").push((done, total));
        });
        rar.set_progress_callback(Some(cb));
        rar.add_bytes("data.bin", &payload, 5).expect("add");
        rar.close().expect("close");
    }

    let events: Vec<(u64, u64)> = events.lock().expect("lock").iter().copied().collect();

    assert!(!events.is_empty(), "no progress events emitted");
    for w in events.windows(2) {
        assert!(w[0].0 <= w[1].0, "progress went backwards");
        assert_eq!(w[0].1, w[1].1, "total changed mid-stream");
    }
    let (last_done, last_total) = *events.last().expect("events");
    assert_eq!(last_done, last_total);
    assert_eq!(last_total, payload.len() as u64);
}
