//! Realistic user scenarios: a corpus of many file types created and read by
//! both rar-rs and WinRAR, in both directions.
//!
//! The feature-focused suites elsewhere pin one switch or one member kind at
//! a time; this module drives the ordinary "archive a directory tree" flow
//! over a deliberately varied corpus (empty files, no-extension files,
//! dotfiles, spaces/quotes/hashes in names, CJK and emoji names, 200-byte
//! names, deeply nested trees, an empty directory, many small files,
//! compressible text, incompressible random data, zeros and structured
//! JSON/XML) — the shapes a real user's folder has. Both tools create, both
//! tools read, and every extraction must reproduce the source tree exactly.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use crate::support::{file_sha256, rar_bin, run, temp_dir, unrar_bin, unrar_test};

/// A file to materialize: relative path plus its bytes.
struct FileSpec {
    rel: &'static str,
    data: Vec<u8>,
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x = seed;
    while v.len() < len {
        x = x.wrapping_mul(7).wrapping_add(13);
        v.push(x);
    }
    v
}

fn repeat(unit: &[u8], len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        v.extend_from_slice(unit);
    }
    v.truncate(len);
    v
}

fn json(len: usize) -> Vec<u8> {
    repeat(
        br#"{"id": 12345, "name": "item", "tags": ["a", "b"], "value": 3.14159},
"#,
        len,
    )
}

fn xml(len: usize) -> Vec<u8> {
    repeat(
        b"<item id=\"1\"><name>thing</name><value>42</value></item>\n",
        len,
    )
}

fn pe_like(len: usize) -> Vec<u8> {
    // Many E8 bytes so the x86 filter has something to chew on.
    let mut v = pattern(len, 3);
    for (i, b) in v.iter_mut().enumerate() {
        if i % 4 == 0 {
            *b = 0xE8;
        }
    }
    v
}

/// The corpus: ordinary file types and names only (no attributes or
/// decomposed Unicode — those are covered by the divergence tests below, so
/// this one can assert full parity).
fn corpus() -> Vec<FileSpec> {
    let mut files = vec![
        FileSpec {
            rel: "empty.txt",
            data: Vec::new(),
        },
        FileSpec {
            rel: "one.txt",
            data: b"A".to_vec(),
        },
        FileSpec {
            rel: "text_ascii.txt",
            data: repeat(
                b"The quick brown fox jumps over the lazy dog 0123456789.\r\n",
                200_000,
            ),
        },
        FileSpec {
            rel: "text_cjk.txt",
            data: repeat("中文测试内容。你好，世界！\n".as_bytes(), 40_000),
        },
        FileSpec {
            rel: "random.bin",
            data: pattern(100_000, 1),
        },
        FileSpec {
            rel: "zeros.bin",
            data: vec![0u8; 300_000],
        },
        FileSpec {
            rel: "noext",
            data: repeat(b"no extension\n", 500),
        },
        FileSpec {
            rel: ".hidden",
            data: b"dotfile\n".to_vec(),
        },
        FileSpec {
            rel: "with space.txt",
            data: b"spaces\n".to_vec(),
        },
        FileSpec {
            rel: "with-dash.txt",
            data: b"dash\n".to_vec(),
        },
        FileSpec {
            rel: "with'quote.txt",
            data: b"quote\n".to_vec(),
        },
        FileSpec {
            rel: "with#hash.txt",
            data: b"hash\n".to_vec(),
        },
        FileSpec {
            rel: "with&amp.txt",
            data: b"amp\n".to_vec(),
        },
        FileSpec {
            rel: "with[bracket].txt",
            data: b"bracket\n".to_vec(),
        },
        FileSpec {
            rel: "cjk_中文文件名.txt",
            data: "中文内容\n".as_bytes().to_vec(),
        },
        FileSpec {
            rel: "emoji_\u{1F389}.txt",
            data: b"party\n".to_vec(),
        },
        FileSpec {
            rel: "all_bytes.bin",
            data: (0u16..=255)
                .map(|b| b as u8)
                .cycle()
                .take(256 * 200)
                .collect(),
        },
        FileSpec {
            rel: "prog.exe",
            data: pe_like(120_000),
        },
        FileSpec {
            rel: "data.json",
            data: json(150_000),
        },
        FileSpec {
            rel: "data.xml",
            data: xml(150_000),
        },
        FileSpec {
            rel: "fake.jpg",
            data: pattern(180_000, 4),
        },
        FileSpec {
            rel: "UPPER.TXT",
            data: b"upper\n".to_vec(),
        },
        FileSpec {
            rel: "upper.txt",
            data: b"lower\n".to_vec(),
        },
        FileSpec {
            rel: "a/b/c/d/deep.txt",
            data: repeat(b"deep\n", 5_000),
        },
        FileSpec {
            rel: "a/b/same.txt",
            data: b"same one\n".to_vec(),
        },
        FileSpec {
            rel: "a/same.txt",
            data: b"same two\n".to_vec(),
        },
    ];
    for i in 0..60 {
        files.push(FileSpec {
            rel: Box::leak(format!("src/mod_{i:02}.rs").into_boxed_str()),
            data: repeat(format!("// module {i}\n").as_bytes(), 400),
        });
    }
    for i in 0..40 {
        files.push(FileSpec {
            rel: Box::leak(format!("src/small_{i:03}.txt").into_boxed_str()),
            data: format!("tiny {i}\n").into_bytes(),
        });
    }
    // A 200-byte file name (well under the 255-byte component limit).
    files.push(FileSpec {
        rel: Box::leak(format!("{}.txt", "x".repeat(200)).into_boxed_str()),
        data: b"long name\n".to_vec(),
    });
    files
}

fn build_corpus(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    for f in corpus() {
        let p = root.join(f.rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut fh = std::fs::File::create(&p).unwrap();
        fh.write_all(&f.data).unwrap();
    }
    std::fs::create_dir_all(root.join("emptydir")).unwrap();
}

/// Relative-path → (size, sha256) for every file, plus a marker for
/// directories. Directories compare by presence only.
fn manifest(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if p.is_dir() {
                out.insert(format!("{rel}/"), "dir".into());
                walk(root, &p, out);
            } else {
                let size = p.metadata().unwrap().len();
                out.insert(rel, format!("{size}:{}", file_sha256(&p)));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn diff(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    for k in a.keys().chain(b.keys()) {
        match (a.get(k), b.get(k)) {
            (Some(x), Some(y)) if x == y => {}
            (x, y) => out.push(format!("{k}: {x:?} vs {y:?}")),
        }
    }
    out.sort();
    out.dedup();
    out
}

fn create_from(binary: &Path, switches: &[&str], src: &Path, arc: &Path) {
    let (ok, out) = run(Command::new(binary)
        .arg("a")
        .arg("-idq")
        .arg("-r")
        .args(switches)
        .arg(arc)
        .arg(".")
        .current_dir(src));
    assert!(ok, "create with {} failed:\n{out}", binary.display());
}

/// The first file of a possibly multi-volume set (`base.partN.rar`),
/// otherwise the base path itself.
fn first_volume(base: &Path) -> std::path::PathBuf {
    let stem = base.file_stem().unwrap().to_string_lossy().into_owned();
    let Some(parent) = base.parent() else {
        return base.to_path_buf();
    };
    let mut parts: Vec<_> = std::fs::read_dir(parent)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|q| {
            let n = q.file_name().unwrap().to_string_lossy().into_owned();
            n.starts_with(&stem) && n.contains(".part") && n.ends_with(".rar")
        })
        .collect();
    parts.sort();
    parts
        .into_iter()
        .next()
        .unwrap_or_else(|| base.to_path_buf())
}

fn extract_with(reader: &Path, arc: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).unwrap();
    let (ok, out) = run(Command::new(reader)
        .arg("x")
        .arg("-idq")
        .arg("-o+")
        .arg("-y")
        .arg(first_volume(arc))
        .arg(format!("{}{}", dest.display(), std::path::MAIN_SEPARATOR)));
    assert!(ok, "extract with {} failed:\n{out}", reader.display());
}

/// The whole varied corpus, created by each tool and read back by both, must
/// reproduce the source tree byte-for-byte. This is the "simulate what a user
/// actually archives" case; the switch matrix lives in the other modules.
#[test]
fn varied_corpus_round_trips_through_both_tools() {
    let Some(official_rar) = rar_bin() else {
        return;
    };
    let Some(official_unrar) = unrar_bin() else {
        return;
    };
    let ours_rar = Path::new(env!("CARGO_BIN_EXE_rar"));
    let ours_unrar = Path::new(env!("CARGO_BIN_EXE_unrar"));

    let dir = temp_dir();
    let src = dir.path().join("corpus");
    build_corpus(&src);
    let want = manifest(&src);
    assert!(
        want.len() > 100,
        "corpus should be broad: {} entries",
        want.len()
    );

    for (switches, tag) in [
        (&[][..], "default"),
        (&["-m0"][..], "m0"),
        (&["-m5"][..], "m5"),
        (&["-m3", "-s"][..], "solid"),
        (&["-m3", "-v100k"][..], "volumes"),
    ] {
        let case = dir.path().join(tag);
        std::fs::create_dir_all(&case).unwrap();
        let ours_arc = case.join("ours.rar");
        let off_arc = case.join("off.rar");
        create_from(ours_rar, switches, &src, &ours_arc);
        create_from(official_rar.as_path(), switches, &src, &off_arc);

        for (label, arc, reader) in [
            ("ours/ours", &ours_arc, ours_unrar),
            ("ours/off", &ours_arc, official_unrar.as_path()),
            ("off/ours", &off_arc, ours_unrar),
            ("off/off", &off_arc, official_unrar.as_path()),
        ] {
            let dest = case.join(format!("x_{}", label.replace('/', "_")));
            extract_with(reader, arc, &dest);
            let got = manifest(&dest);
            assert_eq!(
                diff(&want, &got),
                Vec::<String>::new(),
                "case {tag} {label}"
            );
        }
    }
}

/// A canonically decomposed file name (NFD, `e` + U+0301) must survive our
/// own create→extract round trip byte-exactly.
///
/// This is also where our RAR5 metadata choice shows: our archives declare
/// the Unix host, and WinRAR's *Windows* reader NFC-composes names from a
/// Unix-host archive, so it extracts `combining_é.txt` (U+00E9) for the
/// name WinRAR itself would keep as NFD. We keep the exact bytes; matching
/// WinRAR's Windows reader would mean writing Windows host metadata on
/// Windows (see PLAN.md「已知小差异」).
#[test]
fn decomposed_unicode_names_survive_our_round_trip() {
    let ours_rar = Path::new(env!("CARGO_BIN_EXE_rar"));
    let ours_unrar = Path::new(env!("CARGO_BIN_EXE_unrar"));
    let dir = temp_dir();
    let src = dir.path().join("nfd");
    std::fs::create_dir_all(&src).unwrap();
    let nfd = "combining_e\u{0301}.txt";
    std::fs::write(src.join(nfd), b"decomposed\n").unwrap();

    let arc = dir.path().join("nfd.rar");
    let (ok, out) = run(Command::new(ours_rar)
        .args(["a", "-idq"])
        .arg(&arc)
        .arg(nfd)
        .current_dir(&src));
    assert!(ok, "create failed:\n{out}");

    let dest = dir.path().join("out");
    std::fs::create_dir_all(&dest).unwrap();
    let (ok, out) = run(Command::new(ours_unrar)
        .args(["x", "-idq", "-o+", "-y", "--dest"])
        .arg(&dest)
        .arg(&arc));
    assert!(ok, "extract failed:\n{out}");
    let names: Vec<String> = std::fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![nfd.to_string()],
        "the NFD name must be preserved"
    );
}

/// Flat extraction (`e`) of a tree with colliding basenames must leave the
/// same files as WinRAR. Both readers run over the *same* archive, so this
/// isolates reader behavior: the corpus has `a/same.txt`, `a/b/same.txt`,
/// `UPPER.TXT` and `upper.txt`, which all collapse into one directory.
#[test]
fn flat_extraction_with_collisions_matches_winrar() {
    let Some(official_rar) = rar_bin() else {
        return;
    };
    let Some(official_unrar) = unrar_bin() else {
        return;
    };
    let ours_unrar = Path::new(env!("CARGO_BIN_EXE_unrar"));
    let dir = temp_dir();
    let src = dir.path().join("corpus");
    build_corpus(&src);

    let off_arc = dir.path().join("off.rar");
    let (ok, out) = run(Command::new(&official_rar)
        .args(["a", "-idq", "-r"])
        .arg(&off_arc)
        .arg(".")
        .current_dir(&src));
    assert!(ok, "WinRAR create failed:\n{out}");

    let ours_dest = dir.path().join("flat_ours");
    let off_dest = dir.path().join("flat_off");
    std::fs::create_dir_all(&ours_dest).unwrap();
    std::fs::create_dir_all(&off_dest).unwrap();
    let (ok, out) = run(Command::new(ours_unrar)
        .args(["e", "-idq", "-o+", "-y", "--dest"])
        .arg(&ours_dest)
        .arg(&off_arc));
    assert!(ok, "our flat extract failed:\n{out}");
    let (ok, out) = run(Command::new(&official_unrar)
        .args(["e", "-idq", "-o+", "-y"])
        .arg(&off_arc)
        .arg(format!(
            "{}{}",
            off_dest.display(),
            std::path::MAIN_SEPARATOR
        )));
    assert!(ok, "WinRAR flat extract failed:\n{out}");

    let ours = manifest(&ours_dest);
    let theirs = manifest(&off_dest);
    assert!(
        ours.values().all(|v| v != "dir"),
        "flat extraction must create no directories: {ours:?}"
    );
    assert_eq!(
        diff(&theirs, &ours),
        Vec::<String>::new(),
        "flat extraction"
    );
}

/// A Unicode archive comment set by our CLI must read back identically
/// through WinRAR (and a comment WinRAR sets must read back through us).
#[test]
fn unicode_archive_comment_matches_winrar() {
    let Some(official_rar) = rar_bin() else {
        return;
    };
    let ours_rar = Path::new(env!("CARGO_BIN_EXE_rar"));
    let dir = temp_dir();
    let src = dir.path().join("c");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("f.txt"), b"body\n").unwrap();
    let comment = "第一行注释\nsecond line\n";
    let cmt_file = dir.path().join("c.txt");
    std::fs::write(&cmt_file, comment.as_bytes()).unwrap();

    // Ours: the comment rides on `a -z<file>`.
    let ours_arc = dir.path().join("ours.rar");
    let (ok, out) = run(Command::new(ours_rar)
        .args(["a", "-idq"])
        .arg(format!("-z{}", cmt_file.display()))
        .arg(&ours_arc)
        .arg("f.txt")
        .current_dir(&src));
    assert!(ok, "our a -z failed:\n{out}");

    // WinRAR: create, then set the comment from stdin like its console `c`.
    let off_arc = dir.path().join("off.rar");
    let (ok, out) = run(Command::new(&official_rar)
        .args(["a", "-idq"])
        .arg(&off_arc)
        .arg("f.txt")
        .current_dir(&src));
    assert!(ok, "WinRAR a failed:\n{out}");
    let mut child = Command::new(&official_rar)
        .arg("c")
        .arg(&off_arc)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(comment.as_bytes()).unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "WinRAR c failed");

    // Both archives must expose the comment through WinRAR and through us.
    for (arc, tag) in [(&ours_arc, "ours"), (&off_arc, "off")] {
        let (ok, out) = run(Command::new(&official_rar).arg("cw").arg(arc));
        assert!(ok, "WinRAR cw {tag} failed:\n{out}");
        assert!(
            out.contains("第一行注释") && out.contains("second line"),
            "WinRAR did not see {tag}'s comment, got {out:?}"
        );
        let (ok, out) = run(Command::new(ours_rar).arg("cw").arg(arc));
        assert!(ok, "our cw {tag} failed:\n{out}");
        assert!(
            out.contains("第一行注释") && out.contains("second line"),
            "we did not see {tag}'s comment, got {out:?}"
        );
        let (ok, out) = unrar_test(arc, None);
        assert!(ok, "WinRAR rejected {tag}:\n{out}");
    }
}
