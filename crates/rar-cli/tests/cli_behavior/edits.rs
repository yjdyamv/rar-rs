use crate::support::{RAR_CLI, UNRAR_CLI, make_temp_dir};
/// RAR4 multi-volume sets support the header-level edits official `rar`
/// supports: rename (and lock) rewrite each volume in place, while delete is
/// refused exactly like WinRAR's "Cannot modify volume".
#[test]
fn cli_rar4_multivolume_rename_and_lock() {
    let dir = make_temp_dir();
    let mut content = Vec::new();
    let mut seed = 7u32;
    while content.len() < 90_000 {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        content.push((seed >> 16) as u8);
    }
    std::fs::write(dir.path().join("a.bin"), &content).unwrap();
    std::fs::write(dir.path().join("b.bin"), vec![0x5Au8; 30_000]).unwrap();

    // Our RAR4 writer names the set `.partNN.rar` (WinRAR's default).
    let base = dir.path().join("mv.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-v20k", "-idq"])
        .arg(&base)
        .args(["a.bin", "b.bin"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "create a RAR4 volume set");
    assert!(
        dir.path().join("mv.part2.rar").exists(),
        "expected a second volume"
    );
    // Edits address the set's first volume, like WinRAR.
    let first = dir.path().join("mv.part1.rar");

    // Rename: the set must stay readable under the new name.
    let status = std::process::Command::new(RAR_CLI)
        .args(["rn", "-idq"])
        .arg(&first)
        .args(["a.bin", "renamed.bin"])
        .status()
        .unwrap();
    assert!(status.success(), "rename in a RAR4 volume set");

    let mut reader = rar_rs::ArchiveReader::open(&first).unwrap();
    let names: Vec<String> = reader.entries().map(|e| e.name().to_string()).collect();
    assert!(names.iter().any(|n| n == "renamed.bin"), "{names:?}");
    let id = reader.unique_entry("renamed.bin").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), content);

    // Delete is refused (official parity), not silently ignored.
    let delete = std::process::Command::new(RAR_CLI)
        .args(["d", "-idq"])
        .arg(&first)
        .arg("renamed.bin")
        .output()
        .unwrap();
    assert!(
        !delete.status.success(),
        "delete on a RAR4 volume set must fail"
    );

    // Lock patches the first volume's main header; a later edit is refused.
    let status = std::process::Command::new(RAR_CLI)
        .args(["k", "-idq"])
        .arg(&first)
        .status()
        .unwrap();
    assert!(status.success(), "lock a RAR4 volume set");
    let rename_after_lock = std::process::Command::new(RAR_CLI)
        .args(["rn", "-idq"])
        .arg(&first)
        .args(["b.bin", "c.bin"])
        .output()
        .unwrap();
    assert!(
        !rename_after_lock.status.success(),
        "a locked RAR4 volume set must refuse edits"
    );
}

/// Archive comments are header-level too: official `rar c` puts the `CMT`
/// block after the first volume's main header (and no other volume changes);
/// set/replace/clear must keep the set valid.
#[test]
fn cli_rar4_multivolume_archive_comment_roundtrips() {
    let dir = make_temp_dir();
    for i in 1u8..=3 {
        std::fs::write(dir.path().join(format!("t{i}.txt")), vec![b'a' + i; 9000]).unwrap();
    }
    let base = dir.path().join("cmt.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-m0", "-v20k", "-idq"])
        .arg(&base)
        .args(["t1.txt", "t2.txt", "t3.txt"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "create a RAR4 volume set");
    assert!(
        dir.path().join("cmt.part2.rar").exists(),
        "expected a second volume"
    );
    // Edits address the set's first volume, like WinRAR.
    let first = dir.path().join("cmt.part1.rar");

    let comment = dir.path().join("comment.txt");
    let set_comment = |body: &[u8]| {
        std::fs::write(&comment, body).unwrap();
        std::process::Command::new(RAR_CLI)
            .args(["c", "-idq"])
            .arg(format!("-z{}", comment.display()))
            .arg(&first)
            .status()
            .unwrap()
            .success()
    };
    let read_comment = || {
        let out = std::process::Command::new(RAR_CLI)
            .args(["cw", "-idq"])
            .arg(&first)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    assert!(set_comment(b"multi-volume comment\n"));
    assert_eq!(read_comment(), "multi-volume comment");
    assert!(set_comment(b"replaced\n"));
    assert_eq!(read_comment(), "replaced");

    // `cw <archive> <file>` writes the comment to the file.
    let written = dir.path().join("comment-out.txt");
    let status = std::process::Command::new(RAR_CLI)
        .args(["cw", "-idq"])
        .arg(&first)
        .arg(&written)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(&written).unwrap(), b"replaced\n");

    assert!(set_comment(b""));
    assert_eq!(read_comment(), "");

    let reader = rar_rs::ArchiveReader::open(&first).unwrap();
    assert_eq!(reader.entries().count(), 3);
}

/// Header-encrypted RAR5 archives now support rename and archive-comment
/// edits: rewritten headers are re-encrypted through the encrypting writer.
/// Create-time `-z` is supported too; `-k`/lock stays refused, as do delete
/// and recovery-record edits (which keep working).
#[test]
fn cli_header_encrypted_rar5_edits_work() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("f.txt"), b"one").unwrap();
    std::fs::write(dir.path().join("g.txt"), b"two").unwrap();
    std::fs::write(dir.path().join("note.txt"), b"note").unwrap();
    let archive = dir.path().join("hp.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-hpsecret", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .arg("g.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    // Archive comment: written, re-encrypted and read back.
    let status = std::process::Command::new(RAR_CLI)
        .args(["c", "-psecret", "-idq", "-znote.txt"])
        .arg(&archive)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "comment edit on an -hp archive");
    let cw = std::process::Command::new(RAR_CLI)
        .args(["cw", "-psecret"])
        .arg(&archive)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(cw.status.success());
    assert_eq!(String::from_utf8_lossy(&cw.stdout).trim_end(), "note");

    // Rename: the re-serialized header is re-encrypted.
    let status = std::process::Command::new(RAR_CLI)
        .args(["rn", "-psecret", "-idq"])
        .arg(&archive)
        .arg("f.txt")
        .arg("zz.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rename on an -hp archive");
    let list = std::process::Command::new(RAR_CLI)
        .args(["lb", "-psecret"])
        .arg(&archive)
        .current_dir(dir.path())
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&list.stdout);
    assert!(names.contains("zz.txt"), "renamed member: {names}");
    assert!(!names.contains("f.txt"), "old name must be gone: {names}");

    // The official tool reads the edited archive.
    let test = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-psecret", "-idq"])
        .arg(&archive)
        .output()
        .unwrap();
    assert!(
        test.status.success(),
        "official unrar must read the edited -hp archive:\n{}",
        String::from_utf8_lossy(&test.stderr)
    );

    // Create-time `-z` is supported; `-k` is still refused before writing.
    let with_comment = dir.path().join("nz.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-hpsecret", "-idq", "-znote.txt"])
        .arg(&with_comment)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "a -hp -z must now succeed");
    assert!(with_comment.exists());

    let locked = dir.path().join("nk.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-hpsecret", "-idq", "-k"])
        .arg(&locked)
        .arg("f.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(!status.success(), "a -hp -k must still be rejected");
    assert!(!locked.exists(), "nk.rar must not be created");

    // Delete still works on the encrypted archive.
    let status = std::process::Command::new(RAR_CLI)
        .args(["d", "-psecret", "-idq"])
        .arg(&archive)
        .arg("zz.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-psecret", "-idq"])
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
}

/// Deleting from a header-encrypted multi-volume RAR5 set now works: the
/// re-split blocks (and the per-volume encryption header) are re-encrypted
/// and the set stays valid.
#[test]
fn cli_header_encrypted_multivolume_delete_works() {
    let dir = make_temp_dir();
    let big: Vec<u8> = (0..60_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    std::fs::write(dir.path().join("big.bin"), &big).unwrap();
    std::fs::write(dir.path().join("a.txt"), b"member").unwrap();

    let archive = dir.path().join("hp-mv.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-m0", "-v20k", "-hppw", "-idq"])
        .arg(&archive)
        .arg("big.bin")
        .arg("a.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "create the encrypted volume set");

    let first = dir.path().join("hp-mv.part1.rar");
    let delete = std::process::Command::new(RAR_CLI)
        .args(["d", "-ppw", "-idq"])
        .arg(&first)
        .arg("a.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        delete.status.success(),
        "deleting from a header-encrypted volume set:\n{}",
        String::from_utf8_lossy(&delete.stderr)
    );

    // The deleted member is gone and the set still verifies.
    let list = std::process::Command::new(RAR_CLI)
        .args(["lb", "-ppw"])
        .arg(&first)
        .current_dir(dir.path())
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&list.stdout);
    assert!(names.contains("big.bin"), "kept member: {names}");
    assert!(!names.contains("a.txt"), "deleted member: {names}");

    let test = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-ppw", "-idq"])
        .arg(&first)
        .output()
        .unwrap();
    assert!(
        test.status.success(),
        "the set must still verify:\n{}",
        String::from_utf8_lossy(&test.stderr)
    );
}
