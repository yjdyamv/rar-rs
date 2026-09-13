use std::path::{Path, PathBuf};
use std::process::Command;

use rar_rs::ArchiveReader;

use crate::support::{rar4_623_bin, run, temp_dir, unrar_bin};

/// Official 6.23 creates a RAR4 volume set (`.partN.rar` naming, which is
/// not what our writer emits); our rename rewrites each volume in place and
/// UnRAR still validates the set.
#[test]
fn we_rename_members_in_a_winrar_rar4_volume_set() {
    let Some(rar623) = rar4_623_bin() else {
        eprintln!("skipped: WinRAR 6.23 not found");
        return;
    };
    let dir = temp_dir();
    let mut content = Vec::new();
    let mut seed = 9u32;
    while content.len() < 90_000 {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        content.push((seed >> 16) as u8);
    }
    std::fs::write(dir.path().join("a.bin"), &content).unwrap();
    std::fs::write(dir.path().join("b.bin"), vec![0x3Cu8; 30_000]).unwrap();

    let base = dir.path().join("wv.rar");
    let mut create = Command::new(&rar623);
    create
        .args(["a", "-ma4", "-v20k", "-idq"])
        .arg(&base)
        .args(["a.bin", "b.bin"])
        .current_dir(dir.path());
    let (ok, out) = run(&mut create);
    assert!(ok, "6.23 could not create the volume set:\n{out}");
    let first = dir.path().join("wv.part1.rar");
    assert!(first.exists(), "expected .partN.rar naming from 6.23");

    let mut rename = Command::new(env!("CARGO_BIN_EXE_rar"));
    rename
        .args(["rn", "-idq"])
        .arg(&first)
        .args(["a.bin", "renamed.bin"]);
    let (ok, out) = run(&mut rename);
    assert!(ok, "our rename failed on a 6.23 volume set:\n{out}");

    let mut reader = ArchiveReader::open(&first).unwrap();
    let id = reader.unique_entry("renamed.bin").unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), content);

    if let Some(unrar) = unrar_bin() {
        let mut test = Command::new(&unrar);
        test.args(["t", "-idq"]).arg(&first);
        let (ok, out) = run(&mut test);
        assert!(ok, "UnRAR t after our rename failed:\n{out}");

        let mut list = Command::new(&unrar);
        list.args(["lb"]).arg(&first);
        let (ok, out) = run(&mut list);
        assert!(
            ok && out.contains("renamed.bin"),
            "UnRAR list missing the rename:\n{out}"
        );
    }
}

/// Official 6.23 creates the volume set and its archive comment; we read it,
/// replace it, and 6.23 reads our replacement back. UnRAR still validates.
#[test]
fn we_edit_the_comment_of_a_winrar_rar4_volume_set() {
    let Some(rar623) = rar4_623_bin() else {
        eprintln!("skipped: WinRAR 6.23 not found");
        return;
    };
    let dir = temp_dir();
    for i in 1u8..=3 {
        std::fs::write(dir.path().join(format!("t{i}.txt")), vec![b'a' + i; 9000]).unwrap();
    }
    let base = dir.path().join("cw.rar");
    let mut create = Command::new(&rar623);
    create
        .args(["a", "-ma4", "-m0", "-v20k", "-idq"])
        .arg(&base)
        .args(["t1.txt", "t2.txt", "t3.txt"])
        .current_dir(dir.path());
    let (ok, out) = run(&mut create);
    assert!(ok, "6.23 could not create the volume set:\n{out}");
    let first = dir.path().join("cw.part1.rar");
    assert!(first.exists(), "expected .partN.rar naming from 6.23");

    let winrar_comment = dir.path().join("winrar-comment.txt");
    std::fs::write(&winrar_comment, b"winrar volume comment\n").unwrap();
    let mut set_comment = Command::new(&rar623);
    set_comment
        .args(["c", "-idq"])
        .arg(format!("-z{}", winrar_comment.display()))
        .arg(&first);
    let (ok, out) = run(&mut set_comment);
    assert!(ok, "6.23 c failed:\n{out}");

    // We read 6.23's comment...
    let mut cw = Command::new(env!("CARGO_BIN_EXE_rar"));
    cw.args(["cw"]).arg(&first);
    let (ok, out) = run(&mut cw);
    assert!(
        ok && out.contains("winrar volume comment"),
        "our cw:\n{out}"
    );

    // ...replace it...
    let our_comment = dir.path().join("our-comment.txt");
    std::fs::write(&our_comment, b"our volume comment\n").unwrap();
    let mut ours = Command::new(env!("CARGO_BIN_EXE_rar"));
    ours.args(["c", "-idq"])
        .arg(format!("-z{}", our_comment.display()))
        .arg(&first);
    let (ok, out) = run(&mut ours);
    assert!(ok, "our c failed:\n{out}");

    // ...and 6.23 reads ours back.
    let mut cw623 = Command::new(&rar623);
    cw623.args(["cw"]).arg(&first);
    let (ok, out) = run(&mut cw623);
    assert!(ok && out.contains("our volume comment"), "6.23 cw:\n{out}");

    if let Some(unrar) = unrar_bin() {
        let mut test = Command::new(&unrar);
        test.args(["t", "-idq"]).arg(&first);
        let (ok, out) = run(&mut test);
        assert!(ok, "UnRAR t after the comment edit failed:\n{out}");
    }
}

/// Pre-RAR3 solid archives (official 6.23 `-ma1`/`-ma2 -s`) survive our
/// solid repack edits: delete and append keep the member generation and
/// UnRAR 7.23 validates the result.
#[test]
fn legacy_solid_repack_interop_with_winrar_623() {
    let Some(rar623) = rar4_623_bin() else {
        eprintln!("skipped: WinRAR 6.23 not found");
        return;
    };
    let dir = temp_dir();
    let p1: Vec<u8> = b"legacy solid chain payload a ".repeat(500);
    let p2: Vec<u8> = b"legacy solid chain payload b ".repeat(400);
    std::fs::write(dir.path().join("a.txt"), &p1).unwrap();
    std::fs::write(dir.path().join("b.txt"), &p2).unwrap();

    for ma in ["1", "2"] {
        let archive = dir.path().join(format!("solid{ma}.rar"));
        let mut create = Command::new(&rar623);
        create
            .args(["a", &format!("-ma{ma}"), "-s", "-idq"])
            .arg(&archive)
            .args(["a.txt", "b.txt"])
            .current_dir(dir.path());
        let (ok, out) = run(&mut create);
        assert!(
            ok,
            "6.23 -ma{ma} could not create the solid archive:\n{out}"
        );

        // Solid delete repacks the whole chain.
        let mut delete = Command::new(env!("CARGO_BIN_EXE_rar"));
        delete
            .args(["d", "-idq"])
            .arg(&archive)
            .arg("b.txt")
            .current_dir(dir.path());
        let (ok, out) = run(&mut delete);
        assert!(ok, "our solid delete failed on -ma{ma}:\n{out}");

        let mut reader = ArchiveReader::open(&archive).unwrap();
        let names: Vec<String> = reader.entries().map(|e| e.name().to_string()).collect();
        assert_eq!(names, ["a.txt"], "-ma{ma}");
        let id = reader.unique_entry("a.txt").unwrap();
        assert_eq!(reader.read_entry(id).unwrap(), p1, "-ma{ma}");
        drop(reader);

        if let Some(unrar) = unrar_bin() {
            let mut test = Command::new(&unrar);
            test.args(["t", "-idq"]).arg(&archive);
            let (ok, out) = run(&mut test);
            assert!(ok, "UnRAR t after our -ma{ma} delete failed:\n{out}");
        }

        // Append continues the same generation through the deferred repack.
        let mut append = Command::new(env!("CARGO_BIN_EXE_rar"));
        append
            .args(["a", "-idq"])
            .arg(&archive)
            .arg("b.txt")
            .current_dir(dir.path());
        let (ok, out) = run(&mut append);
        assert!(ok, "our append failed on -ma{ma}:\n{out}");

        let mut reader = ArchiveReader::open(&archive).unwrap();
        let names: Vec<String> = reader.entries().map(|e| e.name().to_string()).collect();
        assert_eq!(names, ["a.txt", "b.txt"], "-ma{ma}");
        let id = reader.unique_entry("b.txt").unwrap();
        assert_eq!(reader.read_entry(id).unwrap(), p2, "-ma{ma}");
        drop(reader);

        if let Some(unrar) = unrar_bin() {
            let mut test = Command::new(&unrar);
            test.args(["t", "-idq"]).arg(&archive);
            let (ok, out) = run(&mut test);
            assert!(ok, "UnRAR t after our -ma{ma} append failed:\n{out}");
        }
    }
}

/// The cached official UnRAR of a `.cache/winrar/<version>` tool release,
/// found relative to the CLI crate. `None` skips the official check.
fn cached_unrar(version: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "UnRAR.exe" } else { "unrar" };
    [
        format!("../../.cache/winrar/{version}/{exe}"),
        format!("../.cache/winrar/{version}/{exe}"),
        format!(".cache/winrar/{version}/{exe}"),
    ]
    .iter()
    .map(|rel| Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
    .find(|bin| bin.exists())
}

/// `rar cf` must write the member comment in the official standalone
/// `COMM_HEAD` layout: official UnRAR 6.23 and 7.23 both test and extract
/// the result cleanly, and our reader reads the comment back.
#[test]
fn member_comment_is_accepted_by_official_unrar() {
    let dir = temp_dir();
    let payload = b"commented member payload\n".repeat(200);
    std::fs::write(dir.path().join("c.txt"), &payload).unwrap();
    let archive = dir.path().join("cf.rar");

    let mut create = Command::new(env!("CARGO_BIN_EXE_rar"));
    create
        .args(["a", "-ma4", "-idq"])
        .arg(&archive)
        .arg("c.txt")
        .current_dir(dir.path());
    let (ok, out) = run(&mut create);
    assert!(ok, "our create failed:\n{out}");

    std::fs::write(dir.path().join("note.txt"), b"member comment body").unwrap();
    let mut cf = Command::new(env!("CARGO_BIN_EXE_rar"));
    cf.args(["cf", "-idq", "--comment-file"])
        .arg(dir.path().join("note.txt"))
        .arg(&archive)
        .arg("c.txt");
    let (ok, out) = run(&mut cf);
    assert!(ok, "our cf failed:\n{out}");

    // Our own read-back.
    let mut reader = ArchiveReader::open(&archive).unwrap();
    assert_eq!(
        reader.entries().next().unwrap().comment(),
        Some(&b"member comment body"[..])
    );
    assert_eq!(
        reader
            .read_entry(reader.unique_entry("c.txt").unwrap())
            .unwrap(),
        payload
    );
    drop(reader);

    // Official UnRAR must both test and extract the archive; missing cached
    // tools just skip the official half (our read-back above already ran).
    for version in ["6-23", "7-23"] {
        let Some(unrar) = cached_unrar(version) else {
            eprintln!("skipped: official UnRAR {version} not found");
            continue;
        };
        let mut test = Command::new(&unrar);
        test.args(["t", "-idq"]).arg(&archive);
        let (ok, out) = run(&mut test);
        assert!(ok, "UnRAR {version} t failed:\n{out}");

        let dest = dir.path().join(format!("out-{version}"));
        std::fs::create_dir_all(&dest).unwrap();
        let mut extract = Command::new(&unrar);
        extract
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&archive)
            .arg(&dest);
        let (ok, out) = run(&mut extract);
        assert!(ok, "UnRAR {version} x failed:\n{out}");
        assert_eq!(
            std::fs::read(dest.join("c.txt")).unwrap(),
            payload,
            "UnRAR {version} extracted bytes"
        );
    }
}
