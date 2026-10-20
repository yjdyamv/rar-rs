//! RAR 1.3/1.4 (`RE~^`) interop: no cached WinRAR release still writes the
//! container, so the read side runs on archives our `-ma14` writer produced and
//! every extracted member must be byte-identical to UnRAR 7.23's.

use std::process::Command;

use rar_rs::{ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

use crate::support::{run, temp_dir, unrar_bin, unrar_extract, unrar_test};

/// Build a RAR 1.3/1.4 fixture with our own `-ma14` writer: no cached
/// official release can still write the `RE~^` container, so these read-side
/// checks run on archives our writer produced and UnRAR independently decodes.
fn build14(
    dir: &std::path::Path,
    name: &str,
    switches: &[&str],
    members: &[&str],
) -> std::path::PathBuf {
    let archive = dir.join(name);
    let mut command = Command::new(env!("CARGO_BIN_EXE_rar"));
    command
        .args(["a", "-ma14", "-idq"])
        .args(switches)
        .arg(&archive);
    for member in members {
        command.arg(member);
    }
    command.current_dir(dir);
    let (ok, out) = run(&mut command);
    assert!(
        ok,
        "our -ma14 create of {name} failed:
{out}"
    );
    archive
}

/// Read interop: our extraction of RAR 1.3/1.4 archives (stored, compressed,
/// solid, commented, encrypted and split) must be byte-identical to UnRAR 7.23.
#[test]
fn rar13_extraction_matches_unrar() {
    let Some(unrar) = unrar_bin() else {
        eprintln!("skipped: UnRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let text = "the quick brown fox jumps over the lazy dog. ".repeat(4096);
    let mut state = 0x1234_5678u32;
    let binary: Vec<u8> = (0..70_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        })
        .collect();
    std::fs::write(src.join("big.txt"), &text).unwrap();
    std::fs::write(src.join("data.bin"), &binary).unwrap();
    std::fs::write(
        src.join("small.txt"),
        b"tiny member
",
    )
    .unwrap();
    std::fs::write(src.join("secret.txt"), b"secret payload ".repeat(300)).unwrap();
    std::fs::write(
        src.join("note.txt"),
        b"interop archive comment
",
    )
    .unwrap();
    let members = ["big.txt", "data.bin", "small.txt", "secret.txt"];
    let comment = format!("-z{}", src.join("note.txt").display());

    let cases: [(&str, Vec<&str>, Option<&str>); 5] = [
        ("rar13-store.rar", vec!["-m0"], None),
        ("rar13-m5.rar", vec!["-m5"], None),
        ("rar13-solid.rar", vec!["-m5", "-s"], None),
        ("rar13-comment.rar", vec!["-m5", comment.as_str()], None),
        ("rar13-pw.rar", vec!["-m5", "-s", "-ppw"], Some("pw")),
    ];

    for (name, switches, password) in cases {
        let archive = build14(&src, name, &switches, &members);
        let our_out = dir.path().join(name).join("ours");
        let their_out = dir.path().join(name).join("theirs");
        std::fs::create_dir_all(&our_out).unwrap();
        std::fs::create_dir_all(&their_out).unwrap();

        let mut our_command = Command::new(env!("CARGO_BIN_EXE_rar"));
        our_command
            .args(["x", "-y", "-idq", "--dest"])
            .arg(&our_out);
        if let Some(pw) = password {
            our_command.arg(format!("-p{pw}"));
        }
        let (ok, out) = run(our_command.arg(&archive));
        assert!(
            ok,
            "our extraction of {name} failed:
{out}"
        );

        let mut their_command = Command::new(&unrar);
        their_command.args(["x", "-y", "-idq"]);
        if let Some(pw) = password {
            their_command.arg(format!("-p{pw}"));
        }
        let (ok, out) = run(their_command.arg(&archive).arg(&their_out));
        assert!(
            ok,
            "UnRAR extraction of {name} failed:
{out}"
        );

        for member in members {
            assert_eq!(
                std::fs::read(our_out.join(member)).unwrap(),
                std::fs::read(their_out.join(member)).unwrap(),
                "{name}: {member} bytes differ"
            );
        }
    }
}

/// Write interop: archives created by our `-ma14` writer (stored,
/// compressed, solid, commented and password-protected) must be accepted by
/// official UnRAR 7.23, whose extracted members are byte-identical to ours.
#[test]
fn rar13_created_archives_roundtrip_through_unrar() {
    let Some(unrar) = unrar_bin() else {
        eprintln!("skipped: UnRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let text = "the quick brown fox jumps over the lazy dog. ".repeat(4096);
    let binary: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    std::fs::write(src.join("big.txt"), &text).unwrap();
    std::fs::write(src.join("data.bin"), &binary).unwrap();
    std::fs::write(src.join("small.txt"), b"tiny member\r\n").unwrap();
    std::fs::write(src.join("secret.txt"), b"secret payload ".repeat(300)).unwrap();
    std::fs::write(src.join("note.txt"), b"interop archive comment\r\n").unwrap();
    let members = ["big.txt", "data.bin", "small.txt", "secret.txt"];

    let comment_source = src.join("note.txt").display().to_string();
    let z_comment = format!("-z{comment_source}");
    let cases: [(&str, Vec<&str>, Option<&str>); 4] = [
        ("rar13-store.rar", vec!["-m0"], None),
        ("rar13-m5.rar", vec!["-m5"], None),
        ("rar13-solid.rar", vec!["-m5", "-s"], None),
        (
            "rar13-pw.rar",
            vec!["-m5", "-ppw", z_comment.as_str()],
            Some("pw"),
        ),
    ];

    for (name, extra, password) in cases {
        let archive = src.join(name);
        let mut command = Command::new(env!("CARGO_BIN_EXE_rar"));
        command.args(["a", "-ma14", "-idq"]).args(&extra);
        command.arg(&archive);
        for member in members {
            command.arg(member);
        }
        command.current_dir(&src);
        let (ok, out) = run(&mut command);
        assert!(ok, "our -ma14 create of {name} failed:\n{out}");

        let our_out = dir.path().join(name).join("ours");
        let their_out = dir.path().join(name).join("theirs");
        std::fs::create_dir_all(&our_out).unwrap();
        std::fs::create_dir_all(&their_out).unwrap();

        let mut our_command = Command::new(env!("CARGO_BIN_EXE_rar"));
        our_command
            .args(["x", "-y", "-idq", "--dest"])
            .arg(&our_out);
        if let Some(pw) = password {
            our_command.arg(format!("-p{pw}"));
        }
        let (ok, out) = run(our_command.arg(&archive));
        assert!(ok, "our extraction of {name} failed:\n{out}");

        let mut their_command = Command::new(&unrar);
        their_command.args(["x", "-y", "-idq"]);
        if let Some(pw) = password {
            their_command.arg(format!("-p{pw}"));
        }
        let (ok, out) = run(their_command.arg(&archive).arg(&their_out));
        assert!(ok, "UnRAR extraction of {name} failed:\n{out}");

        for member in members {
            assert_eq!(
                std::fs::read(our_out.join(member)).unwrap(),
                std::fs::read(their_out.join(member)).unwrap(),
                "{name}: {member} bytes differ"
            );
        }

        if password.is_some() {
            // `-idq` suppresses the comment block too, so list verbosely.
            let (ok, out) = run(Command::new(&unrar).args(["l"]).arg(&archive));
            assert!(
                out.contains("interop archive comment"),
                "{name}: UnRAR listing must show the archive comment:\n{out}"
            );
            assert!(ok, "{name}: UnRAR listing failed");
        }
    }
}

/// Write interop: `-ma14` volume sets (stored, compressed solid and
/// encrypted) are accepted by official UnRAR 7.23, which reassembles the
/// split members byte-identically to ours.
#[test]
fn rar13_created_volume_sets_roundtrip_through_unrar() {
    let Some(unrar) = unrar_bin() else {
        eprintln!("skipped: UnRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let text = "the quick brown fox jumps over the lazy dog. ".repeat(4096);
    let mut state = 0x1234_5678u32;
    let binary: Vec<u8> = (0..70_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        })
        .collect();
    std::fs::write(src.join("big.txt"), &text).unwrap();
    std::fs::write(src.join("data.bin"), &binary).unwrap();
    std::fs::write(src.join("small.txt"), b"tiny member\r\n").unwrap();
    let members = ["big.txt", "data.bin", "small.txt"];

    let cases: [(&str, Vec<&str>, Option<&str>); 3] = [
        (
            "rar13-set-store.rar",
            vec!["-m0", "--volume-size=24k"],
            None,
        ),
        (
            "rar13-set-solid.rar",
            vec!["-m5", "-s", "--volume-size=24k"],
            None,
        ),
        (
            "rar13-set-pw.rar",
            vec!["-m5", "-s", "-ppw", "--volume-size=24k"],
            Some("pw"),
        ),
    ];

    for (name, extra, password) in cases {
        let archive = src.join(name);
        let mut command = Command::new(env!("CARGO_BIN_EXE_rar"));
        command.args(["a", "-ma14", "-idq"]).args(&extra);
        command.arg(&archive);
        for member in members {
            command.arg(member);
        }
        command.current_dir(&src);
        let (ok, out) = run(&mut command);
        assert!(ok, "our -ma14 volume-set create of {name} failed:\n{out}");

        // The set really is multi-volume.
        let volumes = rar_rs::discover_volumes(&archive);
        assert!(volumes.len() >= 2, "{name}: {} volumes", volumes.len());

        let our_out = dir.path().join(name).join("ours");
        let their_out = dir.path().join(name).join("theirs");
        std::fs::create_dir_all(&our_out).unwrap();
        std::fs::create_dir_all(&their_out).unwrap();

        let mut our_command = Command::new(env!("CARGO_BIN_EXE_rar"));
        our_command
            .args(["x", "-y", "-idq", "--dest"])
            .arg(&our_out);
        if let Some(pw) = password {
            our_command.arg(format!("-p{pw}"));
        }
        let (ok, out) = run(our_command.arg(&archive));
        assert!(ok, "our extraction of {name} failed:\n{out}");

        let mut their_command = Command::new(&unrar);
        their_command.args(["x", "-y", "-idq"]);
        if let Some(pw) = password {
            their_command.arg(format!("-p{pw}"));
        }
        let (ok, out) = run(their_command.arg(&archive).arg(&their_out));
        assert!(ok, "UnRAR extraction of {name} failed:\n{out}");

        for member in members {
            assert_eq!(
                std::fs::read(our_out.join(member)).unwrap(),
                std::fs::read(their_out.join(member)).unwrap(),
                "{name}: {member} bytes differ"
            );
        }
    }
}

/// Write-side interop: a RAR 1.3/1.4 member at or above the streaming
/// threshold is compressed in bounded memory into a spill and then split
/// across volumes, with one cipher stream continuing across its fragments.
/// UnRAR 7.23 must extract it byte-identically. `#[ignore]`d: needs a 66 MiB
/// member.
#[test]
#[ignore = "slow: 66 MiB member"]
fn rar13_streamed_compressed_matches_unrar() {
    let Some(_unrar) = unrar_bin() else {
        eprintln!("skipped: UnRAR not found");
        return;
    };
    let dir = temp_dir();
    let src = dir.path().join("big.txt");
    let block = b"streamed rar13 multivolume payload 0123456789abcdef\n";
    let size = 66 * 1024 * 1024usize;
    let mut payload = Vec::with_capacity(size);
    while payload.len() < size {
        payload.extend_from_slice(block);
    }
    payload.truncate(size);
    std::fs::write(&src, &payload).unwrap();

    let arc = dir.path().join("big.rar");
    let volumes = {
        let mut writer = ArchiveWriter::create_with(
            &arc,
            WriterOptions::new()
                .compression(ArchiveVersion::V14)
                .password("pw")
                .volume_size(128 * 1024),
        )
        .unwrap();
        writer
            .add_path(
                &src,
                EntryWriteOptions::new()
                    .compression_level(CompressionLevel::try_from(5u8).unwrap()),
            )
            .unwrap();
        writer.finish().unwrap().volume_paths().len()
    };
    assert!(
        volumes > 1,
        "the streamed member must span several volumes, got {volumes}"
    );

    let (ok, out) = unrar_test(&arc, Some("pw"));
    assert!(ok, "UnRAR rejected our streamed RAR13 set:\n{out}");
    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    let (ok, out) = unrar_extract(&arc, &dest, Some("pw"));
    assert!(
        ok,
        "UnRAR extraction of our streamed RAR13 set failed:\n{out}"
    );
    assert_eq!(std::fs::read(dest.join("big.txt")).unwrap(), payload);
}
