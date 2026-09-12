use std::process::Command;

use rar_rs::{
    ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions, OpenOptions,
    WriterOptions,
};

use crate::support::{
    file_sha256, file_sha256_bytes, run, temp_dir, unrar_bin, unrar_extract, unrar_test,
    write_correlated_pcm,
};

/// The RAR4 write side can emit legacy RAR 1.5 (unp_ver 15) and RAR 2.x
/// (unp_ver 20) members via `ArchiveVersion::V15`/`V20`. Real WinRAR
/// UnRAR still carries the old unpack tables (versions 15..36), so it must
/// test and extract such members byte-for-byte — an external
/// cross-validation of the rars encoder ports beyond our own decoders.
#[test]
fn winrar_validates_our_legacy_version_writers() {
    let Some(_unrar) = unrar_bin() else {
        eprintln!("skipped: WinRAR not found");
        return;
    };
    let dir = temp_dir();
    // Repetitive text, a long literal run (RAR 1.5 `st` run mode) and
    // correlated PCM (RAR 2.x audio blocks: m2+ enable try_audio).
    let text = b"legacy writer interop payload ".repeat(4000);
    let run = vec![b'a'; 64_000];
    let pcm_path = dir.path().join("pcm.bin");
    write_correlated_pcm(&pcm_path, 2, 20_000);
    let pcm = std::fs::read(&pcm_path).unwrap();

    for (name, version) in [
        ("legacy15.rar", ArchiveVersion::V15),
        ("legacy20.rar", ArchiveVersion::V20),
    ] {
        let arc = dir.path().join(name);
        {
            let mut rar =
                ArchiveWriter::create_with(&arc, WriterOptions::default().compression(version))
                    .unwrap();
            let opts = EntryWriteOptions::new()
                .compression_level(CompressionLevel::try_from(3u8).unwrap());
            rar.add_bytes("text.txt", &text, opts).unwrap();
            rar.add_bytes("run.bin", &run, opts).unwrap();
            rar.add_bytes("pcm.bin", &pcm, opts).unwrap();
            rar.finish().unwrap();
        }

        // Members report the requested version and actually compress.
        let ar = ArchiveReader::open(&arc).unwrap();
        for e in ar.entries() {
            assert_eq!(e.version(), version, "{name}: {}", e.name());
            assert_ne!(
                e.metadata().method(),
                0,
                "{name}: {} did not compress",
                e.name()
            );
        }

        let (ok, out) = unrar_test(&arc, None);
        assert!(ok, "WinRAR rejected {name}:\n{out}");
        let dest = dir.path().join(format!("out-{name}"));
        std::fs::create_dir_all(&dest).unwrap();
        let (ok, out) = unrar_extract(&arc, &dest, None);
        assert!(ok, "WinRAR failed to extract {name}:\n{out}");
        assert_eq!(
            file_sha256(&dest.join("text.txt")),
            file_sha256_bytes(&text)
        );
        assert_eq!(file_sha256(&dest.join("run.bin")), file_sha256_bytes(&run));
        assert_eq!(file_sha256(&dest.join("pcm.bin")), file_sha256_bytes(&pcm));
    }
}

/// We create a RAR4 (`-ma4`) archive containing a directory tree — nested
/// directories, an empty directory, and a non-ASCII directory name — and
/// both our extractor and WinRAR's UnRAR must see the same tree: the empty
/// directory must come back as a real directory (RAR4 encodes directories
/// in the FILE_HEAD window bits, not just the host attribute), and every
/// member's bytes must match the source.
#[test]
fn we_create_rar4_directory_trees_winrar_valid() {
    let dir = temp_dir();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("sub/deep")).unwrap();
    std::fs::create_dir(src.join("sub/emptydir")).unwrap();
    std::fs::create_dir(src.join("资料")).unwrap();
    std::fs::write(src.join("top.txt"), b"top-level file").unwrap();
    std::fs::write(src.join("sub/mid.txt"), b"mid level").unwrap();
    std::fs::write(src.join("sub/deep/leaf.txt"), b"leaf content here").unwrap();
    std::fs::write(src.join("资料/note.txt"), b"unicode note").unwrap();

    let arc = dir.path().join("tree4.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-m3", "-idq"])
        .arg(&arc)
        .arg("src")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 (directory tree) failed:\n{out}");

    // Our own reader: exact UTF-8 names, directory flags, contents.
    {
        let mut ar = ArchiveReader::open(&arc).unwrap();
        let mut names: Vec<String> = ar.entries().map(|e| e.name().to_string()).collect();
        names.sort();
        let mut expected: Vec<String> = [
            "src",
            "src/sub",
            "src/sub/deep",
            "src/sub/emptydir",
            "src/sub/deep/leaf.txt",
            "src/sub/mid.txt",
            "src/top.txt",
            "src/资料",
            "src/资料/note.txt",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        expected.sort();
        assert_eq!(names, expected);
        for name in ["src", "src/sub/emptydir", "src/资料"] {
            assert!(
                ar.entry(ar.unique_entry(name).unwrap()).unwrap().is_dir(),
                "{name} must be a dir"
            );
        }
        for (name, bytes) in [
            ("src/top.txt", b"top-level file".as_slice()),
            ("src/sub/deep/leaf.txt", b"leaf content here".as_slice()),
            ("src/资料/note.txt", b"unicode note".as_slice()),
        ] {
            assert_eq!(
                &ar.read_entry(ar.unique_entry(name).unwrap()).unwrap(),
                bytes
            );
        }
    }

    // WinRAR's UnRAR must accept the archive and reproduce the tree.
    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&arc));
        assert!(ok, "UnRAR t rejected our -ma4 directory archive:\n{out}");

        let out_dir = dir.path().join("out_unrar");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x failed on our -ma4 directory archive:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("src/sub/deep/leaf.txt")),
            file_sha256(&src.join("sub/deep/leaf.txt"))
        );
        assert_eq!(
            file_sha256(&out_dir.join("src/资料/note.txt")),
            file_sha256(&src.join("资料/note.txt"))
        );
        assert!(
            out_dir.join("src/sub/emptydir").is_dir(),
            "the empty directory must extract as a directory"
        );
        assert!(
            out_dir.join("src/资料").is_dir(),
            "the unicode directory must extract as a directory"
        );
    }
}

/// We create a RAR4 archive with member-level encryption (`-ma4 -p`) and
/// WinRAR's UnRAR must decrypt it: `t` and `x` with the password succeed and
/// reproduce the source bytes, while a wrong or missing password fails.
#[test]
fn we_create_rar4_encrypted_members_winrar_valid() {
    let dir = temp_dir();
    let src = dir.path().join("secret.bin");
    let mut content = Vec::with_capacity(200_000);
    let mut seed = 12345u32;
    while content.len() < 200_000 {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        content.push((seed >> 16) as u8);
    }
    std::fs::write(&src, &content).unwrap();

    let arc = dir.path().join("enc.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-psecret", "-m3", "-idq"])
        .arg(&arc)
        .arg("secret.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 -p failed:\n{out}");

    // Our own reader decrypts with the password and rejects the wrong one.
    {
        let mut ar = ArchiveReader::open_with(&arc, OpenOptions::new().password("secret")).unwrap();
        assert_eq!(
            ar.read_entry(ar.unique_entry("secret.bin").unwrap())
                .unwrap(),
            content
        );
        let mut ar = ArchiveReader::open_with(&arc, OpenOptions::new().password("wrong")).unwrap();
        assert!(
            ar.read_entry(ar.unique_entry("secret.bin").unwrap())
                .is_err(),
            "wrong password must fail"
        );
    }

    // WinRAR's UnRAR must decrypt byte-identically.
    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar)
            .args(["t", "-idq", "-psecret"])
            .arg(&arc));
        assert!(ok, "UnRAR t -psecret failed:\n{out}");
        let (ok, _) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&arc));
        assert!(!ok, "UnRAR t without password must fail");

        let out_dir = dir.path().join("out_unrar");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y", "-psecret"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x -psecret failed:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("secret.bin")),
            file_sha256(&src),
            "WinRAR decrypted different bytes"
        );
    }
}

/// RAR 2.x (`-ma2`, unp_ver 20) member encryption with a password longer
/// than the 16-byte cipher block: the key schedule chains blocks, which the
/// old 8-byte-password fixtures never exercised. WinRAR cannot write RAR2.x,
/// so the oracle is UnRAR reading our output.
#[test]
fn we_create_rar2_long_password_members_winrar_valid() {
    const PW: &str = "abcdefghijklmnopqrst"; // 20 bytes > one cipher block
    let dir = temp_dir();
    let src = dir.path().join("longpw.bin");
    let content = b"rar2 long password payload\n".repeat(2000);
    std::fs::write(&src, &content).unwrap();

    let arc = dir.path().join("rar2-longpw.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma2", "-m3", "-idq"])
        .arg(format!("-p{PW}"))
        .arg(&arc)
        .arg("longpw.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma2 -p failed:\n{out}");

    // Our own reader decrypts with the long password and rejects a wrong one.
    {
        let mut ar = ArchiveReader::open_with(&arc, OpenOptions::new().password(PW)).unwrap();
        assert_eq!(
            ar.read_entry(ar.unique_entry("longpw.bin").unwrap())
                .unwrap(),
            content
        );
        let mut ar = ArchiveReader::open_with(&arc, OpenOptions::new().password("wrong")).unwrap();
        assert!(
            ar.read_entry(ar.unique_entry("longpw.bin").unwrap())
                .is_err(),
            "wrong password must fail"
        );
    }

    // UnRAR must reproduce the source bytes from the >16-byte key schedule.
    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar)
            .args(["t", "-idq"])
            .arg(format!("-p{PW}"))
            .arg(&arc));
        assert!(ok, "UnRAR t with the long password failed:\n{out}");

        let out_dir = dir.path().join("out_longpw");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y"])
            .arg(format!("-p{PW}"))
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x with the long password failed:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("longpw.bin")),
            file_sha256(&src),
            "UnRAR decrypted different bytes"
        );
    }
}

/// We create a RAR4 archive with header encryption (`-ma4 -hp`) and WinRAR's
/// UnRAR must decrypt the headers: `t`/`x` with the password succeed and
/// reproduce the source bytes, while a wrong or missing password fails even
/// to list (the member headers are encrypted, so the scan needs the key).
#[test]
fn we_create_rar4_header_encrypted_winrar_valid() {
    let dir = temp_dir();
    let src = dir.path().join("classified.bin");
    let content = b"top-secret payload guarded by -hp header encryption\n".repeat(4000);
    std::fs::write(&src, &content).unwrap();

    let arc = dir.path().join("hpenctest.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-hpsword", "-m3", "-idq"])
        .arg(&arc)
        .arg("classified.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 -hp failed:\n{out}");

    // Our own reader decrypts the headers with the password.
    {
        let mut ar = ArchiveReader::open_with(&arc, OpenOptions::new().password("sword")).unwrap();
        assert_eq!(
            ar.read_entry(ar.unique_entry("classified.bin").unwrap())
                .unwrap(),
            content
        );
        // Wrong password fails at open (the header scan cannot decrypt).
        assert!(ArchiveReader::open_with(&arc, OpenOptions::new().password("wrong")).is_err());
        assert!(ArchiveReader::open(&arc).is_err());
    }

    // WinRAR's UnRAR must decrypt and verify byte-identically.
    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar)
            .args(["t", "-idq", "-psword"])
            .arg(&arc));
        assert!(ok, "UnRAR t -hp -psword failed:\n{out}");
        let (ok, _) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&arc));
        assert!(!ok, "UnRAR t -hp without password must fail");

        let out_dir = dir.path().join("out_unrar_hp");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y", "-psword"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x -hp -psword failed:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("classified.bin")),
            file_sha256(&src),
            "WinRAR decrypted different bytes"
        );
    }
}

/// We create a RAR4 m5 archive on word-random text and WinRAR's UnRAR must
/// decode the PPMd blocks: modern WinRAR (5.x/6.x) no longer *produces*
/// RAR4 PPMd, but its RAR3 decoder still reads it, so this is the one-way
/// interop check that our PPMd member encoding is real RAR3 PPMd. The m5
/// member must also be markedly smaller than the m3 LZ-only member on the
/// same text (the PPMd pass wins on context-rich data).
#[test]
fn we_create_rar4_ppmd_text_winrar_valid() {
    let dir = temp_dir();
    let words = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
        "juliet", "kilo", "lima", "mike", "november", "oscar", "papa",
    ];
    let mut content = Vec::with_capacity(500_000);
    let mut seed = 12345u32;
    let mut n = 0u32;
    while content.len() < 450_000 {
        let mut line = format!("record {n:06}: ").into_bytes();
        for _ in 0..10 {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            line.extend_from_slice(words[(seed >> 27) as usize % words.len()].as_bytes());
            line.push(b' ');
        }
        line.push(b'\n');
        content.extend_from_slice(&line);
        n += 1;
    }
    let src = dir.path().join("textmix.txt");
    std::fs::write(&src, &content).unwrap();

    let lz_arc = dir.path().join("lz3.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-m3", "-idq"])
        .arg(&lz_arc)
        .arg("textmix.txt")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 -m3 failed:\n{out}");

    let arc = dir.path().join("ppmd5.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-m5", "-idq"])
        .arg(&arc)
        .arg("textmix.txt")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 -m5 failed:\n{out}");

    let lz_size = std::fs::metadata(&lz_arc).unwrap().len();
    let m5_size = std::fs::metadata(&arc).unwrap().len();
    assert!(
        m5_size * 3 < lz_size * 2,
        "m5 PPMd must beat m3 LZSS on text: LZ={lz_size} m5={m5_size}"
    );

    // Our own reader round-trips the PPMd member.
    {
        let mut ar = ArchiveReader::open(&arc).unwrap();
        assert_eq!(ar.entries().next().unwrap().method(), 5);
        assert_eq!(
            ar.read_entry(ar.unique_entry("textmix.txt").unwrap())
                .unwrap(),
            content
        );
    }

    // WinRAR's UnRAR decodes the PPMd blocks byte-identically.
    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&arc));
        assert!(ok, "UnRAR t rejected our PPMd archive:\n{out}");

        let out_dir = dir.path().join("out_ppmd");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x failed on our PPMd archive:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("textmix.txt")),
            file_sha256(&src),
            "WinRAR decoded different bytes from our PPMd member"
        );
    }
}

/// RAR4 auto filter (`-ma4`): a delta-transformable member (16-bit stereo
/// samples) must fire the RAR3 DELTA filter record, and WinRAR's UnRAR must
/// decode the filtered member byte-identically. (WinRAR 6.23's RAR4 writer
/// no longer emits VM filters, so this is a one-way interop check.)
#[test]
fn we_create_rar4_delta_filtered_member_winrar_valid() {
    let dir = temp_dir();
    // 16-bit stereo random-walk samples: channels = 4 (2 ch x 2 bytes).
    let mut content = Vec::with_capacity(600_000);
    let mut l = 0i16;
    let mut r = 0i16;
    let mut seed = 42u32;
    while content.len() < 600_000 {
        l = l.wrapping_add(((seed >> 16) & 0x3f) as i16 - 30);
        r = r.wrapping_add(((seed >> 8) & 0x3f) as i16 - 20);
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        content.extend_from_slice(&l.to_le_bytes());
        content.extend_from_slice(&r.to_le_bytes());
    }
    let src = dir.path().join("audio.bin");
    std::fs::write(&src, &content).unwrap();

    let arc = dir.path().join("delta.rar");
    let (ok, out) = run(Command::new(env!("CARGO_BIN_EXE_rar"))
        .args(["a", "-ma4", "-m3", "-idq"])
        .arg(&arc)
        .arg("audio.bin")
        .current_dir(dir.path()));
    assert!(ok, "our rar -ma4 (delta) failed:\n{out}");

    // The filter must have won decisively: the archive is far smaller than
    // the raw samples (the filter-record bytes sit unaligned in the
    // bitstream, so size is the reliable fingerprint).
    let raw = std::fs::read(&arc).unwrap();
    assert!(
        (raw.len() as u64) * 3 < content.len() as u64,
        "auto delta filter must have fired (member barely compressed)"
    );

    // Our own reader round-trips the filtered member.
    {
        let mut ar = ArchiveReader::open(&arc).unwrap();
        assert_eq!(
            ar.read_entry(ar.unique_entry("audio.bin").unwrap())
                .unwrap(),
            content
        );
    }

    // WinRAR decodes it byte-identically.
    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&arc));
        assert!(
            ok,
            "UnRAR t rejected our delta-filtered RAR4 archive:\n{out}"
        );
        let out_dir = dir.path().join("out_delta");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x failed on our delta-filtered archive:\n{out}");
        assert_eq!(
            file_sha256(&out_dir.join("audio.bin")),
            file_sha256(&src),
            "WinRAR decoded different bytes from our delta-filtered member"
        );
    }
}

/// RAR4 solid m5 on a run of near-identical text files: the run is coded
/// with a shared PPMd model (members 2.. continue it), and WinRAR's UnRAR
/// must decode every member byte-identically. WinRAR 6.23's RAR4 writer
/// never produced PPMd, so this is one-way interop.
#[test]
fn we_create_rar4_solid_ppmd_text_winrar_valid() {
    let dir = temp_dir();
    let mut content = Vec::new();
    for chapter in 1..=4u8 {
        let mut body = Vec::with_capacity(240_000);
        for line in 0..2200u32 {
            body.extend_from_slice(
                format!(
                    "chapter {chapter} line {line:05}: shared boilerplate that repeats across every chapter of this archive body body body tail tail\n"
                )
                .as_bytes(),
            );
        }
        let src = dir.path().join(format!("chap{chapter}.txt"));
        std::fs::write(&src, &body).unwrap();
        content.push((format!("chap{chapter}.txt"), body));
    }

    let arc = dir.path().join("solidppmd.rar");
    let args = vec!["a", "-s", "-ma4", "-m5", "-idq"];
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rar"));
    cmd.args(&args).arg(&arc).current_dir(dir.path());
    for i in 1..=4 {
        cmd.arg(format!("chap{i}.txt"));
    }
    let (ok, out) = run(&mut cmd);
    assert!(ok, "our rar -s -ma4 -m5 failed:\n{out}");

    // Our own reader round-trips the chain.
    {
        let mut ar = ArchiveReader::open(&arc).unwrap();
        for (name, body) in &content {
            assert_eq!(
                &ar.read_entry(ar.unique_entry(name).unwrap()).unwrap(),
                body,
                "{name} solid-PPMd mismatch"
            );
        }
    }

    if let Some(unrar) = unrar_bin() {
        let (ok, out) = run(Command::new(&unrar).args(["t", "-idq"]).arg(&arc));
        assert!(ok, "UnRAR t rejected our solid-PPMd RAR4 archive:\n{out}");
        let out_dir = dir.path().join("out_solidppmd");
        std::fs::create_dir_all(&out_dir).unwrap();
        let (ok, out) = run(Command::new(&unrar)
            .args(["x", "-idq", "-o+", "-y"])
            .arg(&arc)
            .arg(&out_dir));
        assert!(ok, "UnRAR x failed on our solid-PPMd archive:\n{out}");
        for (name, body) in &content {
            let got = std::fs::read(out_dir.join(name)).unwrap();
            assert_eq!(&got, body, "WinRAR decoded {name} differently");
        }
    }
}
