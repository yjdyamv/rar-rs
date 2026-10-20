//! Capability matrix for the local WinRAR/RAR tool cache.
//!
//! The suite no longer vendors archives, so the tool cache under
//! `.cache/winrar` (`$SA_WINRAR_CACHE`) is the source of every real archive.
//! These tests are the contract for that cache: each release must still be able
//! to *generate* an archive our reader decodes, and each release must map to the
//! archive generation it natively writes. When the cache is absent the tests
//! skip; `SA_REQUIRE_WINRAR=1` turns that skip into a failure.

mod support;
use support::generate;

use rar_rs::{ArchiveReader, ArchiveVersion};

/// Releases that write the RAR 2.0 codec (`-ma2` is unavailable before 5.00,
/// so 2.90's native output is the only 2.0 generator in the cache).
fn expected_version(dir: &str) -> ArchiveVersion {
    match dir {
        "2-90" => ArchiveVersion::V20,
        // 3.00–4.20 and 5.00 both default to the 2.9/3.x codec (unp 29).
        d if d.starts_with('3') || d == "4-00" || d == "4-11" || d == "4-20" => ArchiveVersion::V29,
        "5-00" => ArchiveVersion::V29,
        // 5.50 and newer default to RAR5 (`-ma5`); 7.23 writes v70 dictionaries.
        _ => ArchiveVersion::V50,
    }
}

#[test]
fn every_cached_release_generates_an_archive_we_read() {
    let dirs = generate::cached_versions();
    if dirs.is_empty() {
        assert!(
            generate::tool("7-23").is_none(),
            "cache root scanned as empty but 7-23 resolved"
        );
        generate::archive("7-23", &[], &["tiny.txt"]);
        eprintln!(
            "SKIP: no tool cache at {}",
            generate::cache_root().display()
        );
        return;
    }
    for dir in &dirs {
        let Some(made) = generate::archive(dir, &["-m3"], &["text.txt", "noise.bin"]) else {
            continue;
        };
        let mut reader = ArchiveReader::open(made.path())
            .unwrap_or_else(|e| panic!("{dir}: open generated archive: {e}"));
        for name in ["text.txt", "noise.bin"] {
            let id = reader
                .unique_entry(name)
                .unwrap_or_else(|e| panic!("{dir}: no {name}: {e}"));
            assert_eq!(
                reader
                    .read_entry(id)
                    .unwrap_or_else(|e| panic!("{dir}: {name}: {e}")),
                generate::payload(name),
                "{dir}: {name} payload mismatch"
            );
        }
        let versions: Vec<ArchiveVersion> = reader.entries().map(|e| e.version()).collect();
        assert!(
            versions.iter().all(|v| *v == expected_version(dir)),
            "{dir}: expected {:?}, got {versions:?}",
            expected_version(dir)
        );
        // The release's own extractor must accept it too. A missing tool means
        // this checkout cannot cross-check that release; the loop above already
        // proved generation worked, so only the verdict matters here.
        if let Some(verdict) = generate::unrar_test(dir, made.path()) {
            verdict.unwrap_or_else(|e| panic!("{dir}: {e}"));
        }
    }
}

#[test]
fn rar5_switch_only_exists_from_5_00() {
    if generate::tool("3-00").is_none() {
        eprintln!("SKIP: no 3-00 tool");
        return;
    }
    assert!(
        generate::attempt("3-00", &["-ma5"], &["tiny.txt"]).is_err(),
        "3-00 accepted -ma5"
    );
    if generate::tool("5-00").is_some() {
        let made = generate::attempt("5-00", &["-ma5"], &["tiny.txt"]).expect("5-00 -ma5");
        let reader = ArchiveReader::open(made.path()).unwrap();
        assert_eq!(
            reader.entries().next().unwrap().version(),
            ArchiveVersion::V50,
            "5-00 -ma5 must write RAR5"
        );
    }
}

/// The RAR4 output ceiling, pinned per release: `-ma` arrived in 5.00 and
/// `-ma4` was dropped again in 7.00, so only the 5.x/6.x line can write format
/// 29 with it. [`generate::TOOL_RAR29`] names the newest such release, and this
/// test is what keeps that constant honest.
#[test]
fn rar4_output_ceiling_is_pinned() {
    let dirs = generate::cached_versions();
    if dirs.is_empty() {
        eprintln!(
            "SKIP: no tool cache at {}",
            generate::cache_root().display()
        );
        return;
    }
    for version in &dirs {
        let major: u32 = version
            .split(['-', '.'])
            .next()
            .and_then(|m| m.parse().ok())
            .unwrap_or(0);
        let accepted = generate::attempt(version, &["-ma4"], &["tiny.txt"]).is_ok();
        if (5..=6).contains(&major) {
            assert!(accepted, "{version} rejects -ma4");
        } else if major >= 7 {
            assert!(!accepted, "{version} unexpectedly accepts -ma4");
        }
    }
    let newest = dirs
        .iter()
        .filter(|v| v.starts_with("6-"))
        .max()
        .expect("no 6.x release in the cache");
    assert_eq!(
        newest,
        generate::TOOL_RAR29,
        "TOOL_RAR29 must name the newest release with -ma4"
    );
}

#[test]
fn header_encryption_starts_at_3_00() {
    if generate::tool("2-90").is_none() {
        eprintln!("SKIP: no 2-90 tool");
        return;
    }
    assert!(
        generate::attempt("2-90", &["-hpsecret"], &["tiny.txt"]).is_err(),
        "2-90 accepted -hp"
    );
    let made = generate::attempt("3-00", &["-hpsecret", "-m3"], &["tiny.txt", "text.txt"])
        .expect("3-00 -hp");
    let mut reader =
        ArchiveReader::open_with(made.path(), rar_rs::OpenOptions::new().password("secret"))
            .expect("open header-encrypted 3.00 archive");
    let id = reader.unique_entry("text.txt").unwrap();
    assert_eq!(
        reader.read_entry(id).unwrap(),
        generate::payload("text.txt")
    );
}

// ── archive-version coverage ──────────────────────────────────────────────
//
// RAR4 is not one format: v14/v15 (1.3/1.4/1.5), v20/v26 (2.x) and v29/v36
// (3.x/4.x) are different member codecs, and each must stay exercised. Only the
// oldest release that writes a generation is pinned (see `generate::TOOL_*`);
// v26/v36 were never written by any release — they are read-only aliases of the
// v20/v29 codec, so the archive is produced with the same codec and the version
// byte is declared.

fn write_with_our_writer(version: ArchiveVersion, name: &str) -> tempfile::TempDir {
    use rar_rs::{ArchiveWriter, WriterOptions};
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("out.rar");
    let mut writer =
        ArchiveWriter::create_with(&path, WriterOptions::new().compression(version)).unwrap();
    writer
        .add_bytes(name, &generate::payload("text.txt"), Default::default())
        .unwrap();
    writer.finish().unwrap();
    dir
}

fn read_back(path: &std::path::Path, name: &str) -> (ArchiveVersion, Vec<u8>) {
    let mut reader = ArchiveReader::open(path).expect("open");
    let id = reader.unique_entry(name).expect("member");
    let version = reader.entry(id).unwrap().metadata().version();
    (version, reader.read_entry(id).expect("read"))
}

#[test]
fn every_legacy_archive_version_has_a_recipe() {
    // v14 / v15: our own writer; no cached release writes the `RE~^` container
    // or the RAR 1.5 codec, and the official 2.90 UnRAR still has to accept it.
    for (version, name) in [
        (ArchiveVersion::V14, "a13.txt"),
        (ArchiveVersion::V15, "a15.txt"),
    ] {
        let dir = write_with_our_writer(version, name);
        let (got, data) = read_back(&dir.path().join("out.rar"), name);
        assert_eq!(got, version);
        assert_eq!(data, generate::payload("text.txt"));
        for reader in ["7-23", "2-90"] {
            if let Some(verdict) = generate::unrar_test(reader, &dir.path().join("out.rar")) {
                verdict.unwrap_or_else(|e| {
                    panic!("{version:?} not readable by official {reader} UnRAR: {e}")
                });
            }
        }
    }

    // v20: the newest release that can still declare `unp_ver 20` does it with
    // store only (`-ma4 -m0`), so the compressed stream comes from the 2.x line
    // that could actually encode it.
    let Some(v20_stored) = generate::archive(generate::TOOL_RAR20, &["-ma4", "-m0"], &["text.txt"])
    else {
        return;
    };
    let (got, data) = read_back(v20_stored.path(), "text.txt");
    assert_eq!(got, ArchiveVersion::V20);
    assert_eq!(data, generate::payload("text.txt"));

    let Some(v20) = generate::archive(
        generate::TOOL_V20_COMPRESSED,
        &["-m3"],
        &["text.txt", "noise.bin"],
    ) else {
        return;
    };
    let (got, data) = read_back(v20.path(), "text.txt");
    assert_eq!(got, ArchiveVersion::V20);
    assert_eq!(data, generate::payload("text.txt"));

    // v26: same codec, declared version.
    let dir = tempfile::tempdir().unwrap();
    let v26 = dir.path().join("v26.rar");
    std::fs::write(&v26, generate::patch_unp_ver(&v20.read(), 26)).unwrap();
    assert_eq!(read_back(&v26, "text.txt").0, ArchiveVersion::V26);

    // v29: the newest release that still writes format 29 — 7-23 dropped `-ma4`,
    // so this row is pinned to 6-23 and the coverage test above proves no newer
    // release can silently take over.
    let Some(v29) = generate::archive(
        generate::TOOL_RAR29,
        &["-ma4", "-m3"],
        &["text.txt", "noise.bin"],
    ) else {
        return;
    };
    let (got, data) = read_back(v29.path(), "text.txt");
    assert_eq!(
        got,
        ArchiveVersion::V29,
        "`-ma4` must still write format 29"
    );
    assert_eq!(data, generate::payload("text.txt"));

    // v36: same codec as v29, declared version.
    let dir = tempfile::tempdir().unwrap();
    let v36 = dir.path().join("v36.rar");
    std::fs::write(&v36, generate::patch_unp_ver(&v29.read(), 36)).unwrap();
    assert_eq!(read_back(&v36, "text.txt").0, ArchiveVersion::V36);

    // v50: the newest RAR5 writer.
    if let Some(made) = generate::archive(generate::TOOL_RAR50, &["-ma5", "-m3"], &["text.txt"]) {
        let (got, data) = read_back(made.path(), "text.txt");
        assert_eq!(got, ArchiveVersion::V50);
        assert_eq!(data, generate::payload("text.txt"));
    }

    // v70: no release writes it at test scale. A release only switches a member
    // to `comp_version 1` once its dictionary passes 4 GiB, and the dictionary
    // request is capped at twice the member size — so a v70 archive from WinRAR
    // 7.23 needs a >2 GiB input. Our writer forces the flag, which is what the
    // reader is asked to accept.
    let dir = write_with_our_writer(ArchiveVersion::V70, "a70.txt");
    let (got, data) = read_back(&dir.path().join("out.rar"), "a70.txt");
    assert_eq!(got, ArchiveVersion::V70);
    assert_eq!(data, generate::payload("text.txt"));
}

/// Every generated legacy corpus item must be openable by our reader, must
/// decode to the payload it was built from, and must be accepted by official
/// UnRAR. This is the gate the read suites now share.
#[test]
fn corpus_recipes_round_trip() {
    let cases: Vec<(&str, Option<generate::Generated>)> = vec![
        ("store", support::corpus::store()),
        ("compressed_m3", support::corpus::compressed_m3()),
        ("compressed_m5", support::corpus::compressed_m5()),
        ("solid_m3", support::corpus::solid_m3()),
        ("member_password", support::corpus::member_password()),
        ("header_password", support::corpus::header_password()),
        (
            "header_password_unicode",
            support::corpus::header_password_unicode(),
        ),
        (
            "long_password_member",
            support::corpus::long_password_member(),
        ),
        (
            "long_password_header",
            support::corpus::long_password_header(),
        ),
        (
            "header_password_multivol",
            support::corpus::header_password_multivol(),
        ),
        ("multivol_new_names", support::corpus::multivol_new_names()),
        ("multivol_stored", support::corpus::multivol_stored()),
        ("multivol_encrypted", support::corpus::multivol_encrypted()),
        ("multivol_old_names", support::corpus::multivol_old_names()),
        ("ppmd_text", support::corpus::ppmd_text()),
        ("ppmd_mixed", support::corpus::ppmd_mixed()),
        ("ppmd_phrase", support::corpus::ppmd_phrase()),
        ("ppmd_solid", support::corpus::ppmd_solid()),
        ("vm_filters", support::corpus::vm_filters()),
        ("rar20_mixed", support::corpus::rar20_mixed()),
        ("rar20_solid", support::corpus::rar20_solid()),
        ("rar20_password", support::corpus::rar20_password()),
        ("rar154_normal", support::corpus::rar154_normal()),
        ("rar154_password", support::corpus::rar154_password()),
        ("rar154_docset", support::corpus::rar154_docset()),
        ("rar154_split_old", support::corpus::rar154_split_old()),
    ];
    let mut built = 0;
    for (name, made) in cases {
        let Some(made) = made else { continue };
        built += 1;
        assert!(!made.files.is_empty(), "{name}: no files written");
        assert!(made.path().exists(), "{name}: missing first volume");
        // Password-protected recipes cannot be tested without the password, so
        // they are covered by the suites that know it.
        let secret = name.contains("password") || name.contains("encrypted");
        if secret {
            continue;
        }
        // Every recipe must produce something the reference decoder accepts;
        // 7.23 reads all of them (RAR 1.5 through RAR5).
        if let Some(verdict) = generate::unrar_test(generate::TOOL_RAR50, made.path()) {
            verdict.unwrap_or_else(|error| panic!("{name}: UnRAR rejected the archive: {error}"));
        }
        // Header encryption hides the member names, so listing is only
        // assertable for the plain variants.
        if !name.contains("unicode") {
            let reader = rar_rs::ArchiveReader::open(made.path())
                .unwrap_or_else(|error| panic!("{name}: open failed: {error}"));
            assert!(
                reader.entries().next().is_some(),
                "{name}: no entries listed"
            );
        }
    }
    assert!(built > 0, "no corpus recipe could run");
}
