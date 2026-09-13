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
