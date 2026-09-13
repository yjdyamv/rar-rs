//! RAR 1.3/1.4 (`RE~^`) read interop: UnRAR 7.23 decodes the same fixtures
//! and every extracted member must be byte-identical to ours.

use std::process::Command;

use crate::support::{run, temp_dir, unrar_bin};

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rar/tests/fixtures/rar13")
        .join(name)
}

#[test]
fn rar13_extraction_matches_unrar() {
    let Some(unrar) = unrar_bin() else {
        eprintln!("skipped: UnRAR not found");
        return;
    };
    let dir = temp_dir();
    let ours = dir.path().join("ours");
    let theirs = dir.path().join("theirs");
    std::fs::create_dir_all(&ours).unwrap();
    std::fs::create_dir_all(&theirs).unwrap();

    // Solid, plain compressed and old-style multi-volume sets, plus the
    // archive with a member comment.
    let cases = [
        ("SOLID.RAR", &["BIG80K.TXT", "HELLO.TXT", "TINY.TXT"][..]),
        ("README.RAR", &["README"]),
        ("CMULTIV.RAR", &["CMULTI.TXT"]),
        ("FCOMM.RAR", &["HELLO.TXT"]),
        ("SFXSRC.EXE", &["HELLO.TXT"]),
    ];
    for (file, members) in cases {
        let archive = fixture(file);
        let our_out = ours.join(file);
        let their_out = theirs.join(file);
        std::fs::create_dir_all(&our_out).unwrap();
        std::fs::create_dir_all(&their_out).unwrap();

        let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
            .args(["x", "-y", "-idq", "--dest"])
            .arg(&our_out)
            .arg(&archive));
        assert!(ok, "our extraction of {file} failed:\n{out}");

        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-y", "-idq"])
            .arg(&archive)
            .arg(&their_out));
        assert!(ok, "UnRAR extraction of {file} failed:\n{out}");

        for member in members {
            let ours = std::fs::read(our_out.join(member));
            let theirs = std::fs::read(their_out.join(member));
            assert_eq!(ours.as_ref().ok(), theirs.as_ref().ok(), "{file}: {member}");
            assert_eq!(
                ours.unwrap(),
                theirs.unwrap(),
                "{file}: {member} bytes differ"
            );
        }
    }
}

#[test]
fn rar13_encrypted_extraction_matches_unrar() {
    let Some(unrar) = unrar_bin() else {
        eprintln!("skipped: UnRAR not found");
        return;
    };
    let dir = temp_dir();
    let ours = dir.path().join("ours");
    let theirs = dir.path().join("theirs");
    std::fs::create_dir_all(&ours).unwrap();
    std::fs::create_dir_all(&theirs).unwrap();

    let archive = fixture("README_password=password.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["x", "-ppassword", "-y", "-idq", "--dest"])
        .arg(&ours)
        .arg(&archive));
    assert!(ok, "our encrypted extraction failed:\n{out}");

    let (ok, out) = run(Command::new(&unrar)
        .args(["x", "-ppassword", "-y", "-idq"])
        .arg(&archive)
        .arg(&theirs));
    assert!(ok, "UnRAR encrypted extraction failed:\n{out}");

    assert_eq!(
        std::fs::read(ours.join("README")).unwrap(),
        std::fs::read(theirs.join("README")).unwrap()
    );
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
