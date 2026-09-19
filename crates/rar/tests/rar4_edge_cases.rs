//! RAR 1.5–4.x edge cases: comment-bearing fixtures, first-volume targets,
//! streamed edits, whole-set erase, rejection rules, STORE fallback inside a
//! solid chain and encrypted solid repack.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

mod rar4_edit_embedded_comment {
    //! Official-tool regression for RR-bearing edits on RAR 1.5–2.9 archives
    //! whose archive comment is embedded in the main header.
    //!
    //! The fixture (`fixtures/rar40/rar2/comment_nopsw.rar`) is WinRAR 2.02's
    //! own output and carries three embedded comments. Official UnRAR reports
    //! one silent error per comment-bearing header and exits 3 even on the
    //! untouched file (see the fixture README), while every member still tests
    //! `OK`. The check here is therefore: after our `rar rr` edit the members
    //! still test `OK` with the same unrar exit code and no new corruption
    //! message — before the fix the main header was truncated to 13 bytes and
    //! every following block parsed 38 bytes late.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rar40/rar2/comment_nopsw.rar")
    }

    /// `SA_OFFICIAL_UNRAR`, else the project's WinRAR cache (6.23 first: the
    /// last RAR4-producing release named in the bug report). `None` skips the
    /// official check.
    fn official_unrar() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("SA_OFFICIAL_UNRAR") {
            return Some(PathBuf::from(path));
        }
        let exe = if cfg!(windows) { "UnRAR.exe" } else { "unrar" };
        [
            "../../.cache/winrar/6-23",
            "../.cache/winrar/6-23",
            ".cache/winrar/6-23",
            "../../.cache/winrar/7-23",
            "../.cache/winrar/7-23",
            ".cache/winrar/7-23",
        ]
        .iter()
        .map(|dir| Path::new(env!("CARGO_MANIFEST_DIR")).join(dir).join(exe))
        .find(|bin| bin.exists())
    }

    /// `unrar t` result: exit code and the merged stdout+stderr text.
    fn unrar_test(unrar: &Path, archive: &Path) -> (i32, String) {
        let output = Command::new(unrar)
            .arg("t")
            .arg(archive)
            .output()
            .expect("spawn official unrar");
        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.code().unwrap_or(-1), combined)
    }

    /// Standard (IEEE, reflected) CRC-32, used to hand-build the 7-byte
    /// ENDARC block the fixture lacks.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = 0u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    /// A plain ENDARC_HEAD (0x7b, flags 0, head_size 7).
    fn endarc_block() -> [u8; 7] {
        let mut block = [0u8; 7];
        block[2] = 0x7b;
        block[5..7].copy_from_slice(&7u16.to_le_bytes());
        let crc = (crc32(&block[2..]) & 0xffff) as u16;
        block[0..2].copy_from_slice(&crc.to_le_bytes());
        block
    }

    /// The edited copy must keep every member testable with official UnRAR:
    /// same exit code as the untouched fixture copy and an `OK` per member.
    #[test]
    fn rr_edit_keeps_official_unrar_members_ok() {
        let Some(unrar) = official_unrar() else {
            eprintln!("SKIP: official UnRAR not found (set SA_OFFICIAL_UNRAR)");
            return;
        };
        let dir = tempfile::tempdir().unwrap();

        // The fixture predates the optional end-of-archive marker; our editor
        // requires one. Both the baseline and the edited copy carry it.
        let mut bytes = std::fs::read(fixture()).unwrap();
        bytes.extend_from_slice(&endarc_block());
        let baseline = dir.path().join("baseline.rar");
        std::fs::write(&baseline, &bytes).unwrap();

        let (baseline_exit, baseline_out) = unrar_test(&unrar, &baseline);
        assert!(
            baseline_out.contains("FILE1.TXT") && baseline_out.contains("FILE2.TXT"),
            "precondition: unrar must see both members:\n{baseline_out}"
        );

        // `rar rr` on the embedded-comment archive.
        let edited = dir.path().join("edited.rar");
        std::fs::copy(&baseline, &edited).unwrap();
        {
            let mut editor = rar_rs::ArchiveEditor::open(&edited).unwrap();
            editor
                .apply(rar_rs::EditPlan::new().set_recovery(5))
                .unwrap();
        }
        // Our own reader agrees the members extract.
        {
            let mut reader = rar_rs::ArchiveReader::open(&edited).unwrap();
            let file1 = reader.unique_entry("FILE1.TXT").unwrap();
            assert_eq!(reader.read_entry(file1).unwrap(), b"file1\r\n");
            let file2 = reader.unique_entry("FILE2.TXT").unwrap();
            assert_eq!(reader.read_entry(file2).unwrap(), b"file2\r\n");
        }

        let (edited_exit, edited_out) = unrar_test(&unrar, &edited);
        eprintln!("unrar baseline exit={baseline_exit}:\n{baseline_out}");
        eprintln!("unrar edited exit={edited_exit}:\n{edited_out}");
        assert!(
            edited_out.contains("FILE1.TXT") && edited_out.contains("FILE2.TXT"),
            "the edited archive must still list both members:\n{edited_out}"
        );
        assert!(
            edited_out.matches("OK").count() >= 2,
            "the edited archive must still test both members OK:\n{edited_out}"
        );
        assert!(
            !edited_out.contains("Unexpected end of archive")
                && !edited_out.contains("is not RAR archive"),
            "the edit introduced unrar-visible corruption:\n{edited_out}"
        );
        assert_eq!(
            edited_exit, baseline_exit,
            "the edit must not change unrar's verdict (the fixture itself exits {baseline_exit} \
         on its embedded comments):\n{edited_out}"
        );
    }
}

mod rar4_edit_first_volume {
    //! Archive comments and locking on a multi-volume RAR4 set must target the
    //! set's first volume regardless of which part was opened, and the comment
    //! must read back from any part.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::{
        ArchiveEditor, ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EditPlan,
        EntryWriteOptions, RarError, WriterOptions,
    };

    fn stored() -> EntryWriteOptions {
        EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
    }

    fn patterned(len: usize, modulo: usize) -> Vec<u8> {
        (0..len).map(|index| (index % modulo) as u8).collect()
    }

    /// Build a multi-volume RAR4 (`unp_ver 29`) set with two stored members and
    /// return its volumes in order.
    fn build_set(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let base = dir.join("set.rar");
        {
            let mut writer = ArchiveWriter::create_with(
                &base,
                WriterOptions::new()
                    .compression(ArchiveVersion::V29)
                    .volume_size(64 * 1024),
            )
            .unwrap();
            writer
                .add_bytes("a.bin", &patterned(150_000, 251), stored())
                .unwrap();
            writer
                .add_bytes("b.bin", &patterned(150_000, 253), stored())
                .unwrap();
            writer.finish().unwrap();
        }
        let volumes = rar_rs::discover_volumes(&base);
        assert!(
            volumes.len() > 1,
            "precondition: multi-volume set: {volumes:?}"
        );
        volumes
    }

    /// RAR4 main-header flags of a non-SFX volume: signature at 0, main header
    /// block at 7, flags at bytes 10..12.
    fn main_flags(bytes: &[u8]) -> u16 {
        assert_eq!(
            &bytes[..7],
            b"Rar!\x1a\x07\x00",
            "expected a plain RAR4 volume"
        );
        u16::from_le_bytes([bytes[10], bytes[11]])
    }

    fn comment_of(path: &std::path::Path) -> Option<Vec<u8>> {
        let mut archive = ArchiveReader::open(path).unwrap();
        archive.comment().unwrap()
    }

    #[test]
    fn comment_set_from_a_later_part_lands_on_the_first_volume() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = build_set(dir.path());
        let first = volumes[0].clone();
        let later = volumes[1].clone();
        assert_eq!(comment_of(&first), None);
        assert_eq!(comment_of(&later), None);

        // Open from a later part and set the comment.
        let mut editor = ArchiveEditor::open(&later).unwrap();
        editor
            .apply(EditPlan::new().set_comment(b"first-volume note".to_vec()))
            .unwrap();
        drop(editor);

        // The CMT block landed on the first volume and reads back from any part.
        assert_eq!(comment_of(&first), Some(b"first-volume note".to_vec()));
        for volume in &volumes {
            assert_eq!(
                comment_of(volume),
                Some(b"first-volume note".to_vec()),
                "{} must see the comment",
                volume.display()
            );
        }

        // The members survive the per-volume rewrite.
        let mut reader = ArchiveReader::open(&first).unwrap();
        let a = reader.unique_entry("a.bin").unwrap();
        assert_eq!(reader.read_entry(a).unwrap(), patterned(150_000, 251));
        let b = reader.unique_entry("b.bin").unwrap();
        assert_eq!(reader.read_entry(b).unwrap(), patterned(150_000, 253));
    }

    #[test]
    fn lock_from_a_later_part_targets_the_first_volume() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = build_set(dir.path());
        let first = volumes[0].clone();
        let later = volumes[1].clone();

        let mut editor = ArchiveEditor::open(&later).unwrap();
        editor.lock().unwrap();
        drop(editor);

        let first_bytes = std::fs::read(&first).unwrap();
        assert_ne!(
            main_flags(&first_bytes) & 0x0004,
            0,
            "MHD_LOCK must land on the first volume"
        );
        let later_bytes = std::fs::read(&later).unwrap();
        assert_eq!(
            main_flags(&later_bytes) & 0x0004,
            0,
            "later volumes stay untouched"
        );

        // The locked set refuses further edits through any part: the lock lives
        // on the first volume, which is what every edit checks.
        for opened in [&first, &later] {
            let mut editor = ArchiveEditor::open(opened).unwrap();
            assert!(
                matches!(
                    editor.apply(EditPlan::new().set_comment(b"x".to_vec())),
                    Err(RarError::ArchiveLocked)
                ),
                "{} must observe the lock",
                opened.display()
            );
        }
    }
}

mod rar4_edit_streaming {
    //! RAR4 delete/rename on a moderately large archive: the rewrite streams
    //! the member copy path through bounded buffers and still round-trips.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::{
        ArchiveEditor, ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EditPlan,
        EntryWriteOptions, WriterOptions,
    };

    const MIB: usize = 1 << 20;

    fn stored() -> EntryWriteOptions {
        EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
    }

    fn pattern() -> Vec<u8> {
        (0..MIB).map(|index| (index % 251) as u8).collect()
    }

    #[test]
    fn large_rar4_member_delete_and_rename_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.rar");
        let member = dir.path().join("big.bin");
        // 128 MiB written through a small repeating pattern, so the fixture
        // itself never holds the whole member.
        {
            let mut file = std::fs::File::create(&member).unwrap();
            let pattern = pattern();
            for _ in 0..128 {
                std::io::Write::write_all(&mut file, &pattern).unwrap();
            }
        }
        {
            let mut writer = ArchiveWriter::create_with(
                &path,
                WriterOptions::new().compression(ArchiveVersion::V29),
            )
            .unwrap();
            writer.add_path(&member, stored()).unwrap();
            writer
                .add_bytes("small.txt", b"small member", stored())
                .unwrap();
            writer.finish().unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() >= 128 * MIB as u64);

        let mut editor = ArchiveEditor::open(&path).unwrap();
        let big = editor.unique_entry("big.bin").unwrap();
        let small = editor.unique_entry("small.txt").unwrap();
        let report = editor
            .apply(EditPlan::new().rename(big, "renamed.bin").delete(small))
            .unwrap();
        assert_eq!((report.deleted(), report.renamed()), (1, 1));
        drop(editor);

        let mut reader = ArchiveReader::open(&path).unwrap();
        let id = reader.unique_entry("renamed.bin").unwrap();
        let data = reader.read_entry(id).unwrap();
        assert_eq!(data.len(), 128 * MIB);
        let pattern = pattern();
        for chunk in data.as_chunks::<MIB>().0 {
            assert_eq!(chunk, pattern.as_slice());
        }
        assert!(reader.unique_entry("small.txt").is_err());
    }
}

mod rar4_erase_multivolume {
    //! Deleting every member of a multi-volume RAR4 set must erase the whole
    //! set — every `.rar`/`.rNN` data volume and the `.rev` recovery volumes
    //! for the same base — not just the volume that happened to be opened.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::{
        ArchiveEditor, ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions,
        WriterOptions,
    };

    fn stored() -> EntryWriteOptions {
        EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
    }

    fn patterned(len: usize, modulo: usize) -> Vec<u8> {
        (0..len).map(|index| (index % modulo) as u8).collect()
    }

    #[test]
    fn deleting_every_member_erases_the_whole_rar4_volume_set() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("set.rar");
        {
            let mut writer = ArchiveWriter::create_with(
                &base,
                WriterOptions::new()
                    .compression(ArchiveVersion::V29)
                    .volume_size(64 * 1024),
            )
            .unwrap();
            writer
                .add_bytes("a.bin", &patterned(150_000, 251), stored())
                .unwrap();
            writer
                .add_bytes("b.bin", &patterned(150_000, 253), stored())
                .unwrap();
            writer.finish().unwrap();
        }
        let volumes = rar_rs::discover_volumes(&base);
        assert!(
            volumes.len() > 1,
            "precondition: multi-volume set: {volumes:?}"
        );
        // `.rev` recovery volumes for the same base must be retired too.
        let revs = rar_rs::build_recovery_volumes_for_set(&volumes, 1).unwrap();
        assert!(!revs.is_empty(), "precondition: .rev files present");

        let mut editor = ArchiveEditor::open(&volumes[0]).unwrap();
        let ids: Vec<_> = ["a.bin", "b.bin"]
            .iter()
            .map(|name| editor.unique_entry(name).unwrap())
            .collect();
        assert_eq!(editor.delete_entries(&ids).unwrap(), 2);
        assert_eq!(editor.entries().count(), 0);

        for path in volumes.iter().chain(revs.iter()) {
            assert!(!path.exists(), "{} must be removed", path.display());
        }
        // The journaled commit leaves no staging, journal or backup siblings.
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| {
                name.contains("rar5tmp") || name.contains("rar5bak") || name.contains("rar5commit")
            })
            .collect();
        assert!(leftovers.is_empty(), "commit leftovers: {leftovers:?}");
    }
}

mod rar4_rejection {
    //! RAR4 containers are now accepted and decoded.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::ArchiveReader;

    #[test]
    fn synthetic_rar4_with_bogus_header_is_refused_with_clear_error() {
        let dir = make_temp_dir();
        let path = dir.path().join("rar4_synthetic.rar");
        // Valid signature + a marker-block header with head_size=4 (below
        // minimum 7), which must fail with a Format error.
        let mut data = b"Rar!\x1a\x07\x00".to_vec();
        data.extend_from_slice(&[0x72, 0x04, 0x00, 0x00, 0x00]);
        std::fs::write(&path, &data).unwrap();

        let err = match ArchiveReader::open(&path) {
            Ok(_) => panic!("expected synthetic RAR4 with broken header to fail"),
            Err(e) => e,
        };
        match err {
            rar_rs::RarError::Format(msg) => assert!(
                msg.contains("too small") || msg.contains("truncated"),
                "expected format-level error, got: {msg}"
            ),
            other => panic!("expected Format error for broken RAR4 header, got {other:?}"),
        }
    }
}

mod rar4_solid_store_fallback {
    //! Pre-RAR3 (v15/v20) solid chains must stay in lockstep when a member
    //! falls back to STORE. The reader derives these chains from the archive-level
    //! `MHD_SOLID` flag and member position: a STORE member never reaches the
    //! decoder, so the writer's persistent encoder must not advance for it
    //! either. Regression for `rar a -ma2 -s -m5` with an incompressible member
    //! at the start or in the middle of the run.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::{
        ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions,
        SolidMode, WriterOptions,
    };
    use std::path::{Path, PathBuf};

    fn ewo(level: u8) -> EntryWriteOptions {
        EntryWriteOptions::new().compression_level(CompressionLevel::try_from(level).unwrap())
    }

    /// Deterministic incompressible bytes (splitmix64 stream).
    fn incompressible(len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            out.extend_from_slice(&z.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// One member of a test case: stored name and payload.
    type CaseMember = (&'static str, Vec<u8>);
    /// One test case: name, compression level and the member list.
    type Case = (&'static str, u8, Vec<CaseMember>);

    /// `(case, level, members)`: an incompressible member at the start, in the
    /// middle, at the end and as a leading run, each followed or preceded by
    /// compressible members that make the chain state observable; `all-text-m5`
    /// is the control with every member compressed.
    fn cases() -> Vec<Case> {
        let text_a = || b"a solid chain shares its window and tables; ".repeat(700);
        let text_b = || b"the next member continues from the previous one. ".repeat(700);
        vec![
            (
                "all-text-m5",
                5,
                vec![
                    ("text-a.txt", text_a()),
                    ("text-b.txt", text_b()),
                    ("text-c.txt", text_a()),
                ],
            ),
            (
                "store-first-m5",
                5,
                vec![
                    ("random.bin", incompressible(32 * 1024)),
                    ("text-a.txt", text_a()),
                    ("text-b.txt", text_b()),
                ],
            ),
            (
                "store-middle-m5",
                5,
                vec![
                    ("text-a.txt", text_a()),
                    ("random.bin", incompressible(32 * 1024)),
                    ("text-b.txt", text_b()),
                ],
            ),
            (
                "store-last-m5",
                5,
                vec![
                    ("text-a.txt", text_a()),
                    ("random.bin", incompressible(32 * 1024)),
                ],
            ),
            (
                "store-run-m1",
                1,
                vec![
                    ("random-a.bin", incompressible(16 * 1024)),
                    ("random-b.bin", incompressible(16 * 1024)),
                    ("text-a.txt", text_a()),
                ],
            ),
        ]
    }

    fn build(
        version: ArchiveVersion,
        case: &str,
        level: u8,
        members: &[(&str, Vec<u8>)],
    ) -> (tempfile::TempDir, PathBuf) {
        let dir = make_temp_dir();
        let path = dir.path().join(format!("{version}-{case}.rar"));
        {
            let mut archive = ArchiveWriter::create_with(
                &path,
                WriterOptions::new()
                    .compression(version)
                    .solid_mode(SolidMode::Continuous),
            )
            .unwrap_or_else(|e| panic!("create {version} {case}: {e}"));
            for (name, data) in members {
                archive
                    .add_bytes(name, data, ewo(level))
                    .unwrap_or_else(|e| panic!("add {version} {case} {name}: {e}"));
            }
            archive.finish().unwrap();
        }
        (dir, path)
    }

    /// `SA_OFFICIAL_UNRAR`, else the project's WinRAR cache (7.23 prefers the
    /// last release that reads pre-RAR5; 6.23 as fallback). When no binary is
    /// found the test skips with a visible marker; `SA_REQUIRE_OFFICIAL=1` turns
    /// that skip into a hard failure, matching `official_interop.rs`.
    fn official_unrar() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("SA_OFFICIAL_UNRAR") {
            return Some(PathBuf::from(path));
        }
        let exe = if cfg!(windows) { "UnRAR.exe" } else { "unrar" };
        let cached = [
            "../../.cache/winrar/7-23",
            "../.cache/winrar/7-23",
            ".cache/winrar/7-23",
            "../../.cache/winrar/6-23",
            "../.cache/winrar/6-23",
            ".cache/winrar/6-23",
        ]
        .iter()
        .map(|dir| Path::new(env!("CARGO_MANIFEST_DIR")).join(dir).join(exe))
        .find(|bin| bin.exists());
        if cached.is_none() {
            assert!(
                std::env::var_os("SA_REQUIRE_OFFICIAL").is_none(),
                "official unrar is required (SA_REQUIRE_OFFICIAL is set): set SA_OFFICIAL_UNRAR"
            );
            eprintln!("SKIPPED (SA_OFFICIAL_UNRAR unset and no cached unrar found)");
        }
        cached
    }

    #[test]
    fn old_format_solid_store_fallback_roundtrip() {
        for version in [ArchiveVersion::V15, ArchiveVersion::V20] {
            for (case, level, members) in cases() {
                let (_dir, path) = build(version, case, level, &members);
                let mut reader = ArchiveReader::open(&path)
                    .unwrap_or_else(|e| panic!("open {version} {case}: {e}"));
                let entries: Vec<_> = reader.entries().collect();
                assert_eq!(entries.len(), members.len(), "{version} {case}");
                // Every case with an incompressible member must actually have
                // taken the STORE fallback; a compressed output would make the
                // case vacuous.
                if let Some(random_idx) = members
                    .iter()
                    .position(|(name, _)| name.starts_with("random"))
                {
                    let methods: Vec<u8> = entries.iter().map(|e| e.metadata().method()).collect();
                    assert_eq!(
                        methods[random_idx], 0,
                        "{version} {case}: incompressible member must STORE"
                    );
                }
                // RAR 2.x flags every compressed continuation with FHD_SOLID
                // (official UnRAR feeds the flag to `Unpack20` and resets its
                // tables without it); RAR 1.5 is position-derived and never
                // flags. STORE members never carry the flag.
                let methods: Vec<u8> = entries.iter().map(|e| e.metadata().method()).collect();
                let solids: Vec<bool> = entries.iter().map(|e| e.metadata().comp_solid()).collect();
                let mut seen_compressed = false;
                for (i, method) in methods.iter().enumerate() {
                    let compressed = *method != 0;
                    let expected = version == ArchiveVersion::V20 && compressed && seen_compressed;
                    assert_eq!(
                        solids[i], expected,
                        "{version} {case}: FHD_SOLID on member {i}"
                    );
                    seen_compressed |= compressed;
                }
                let ids: Vec<_> = entries.into_iter().map(|e| e.id()).collect();
                for (i, (name, expected)) in members.iter().enumerate() {
                    let got = reader
                        .read_entry(ids[i])
                        .unwrap_or_else(|e| panic!("read {version} {case} {name}: {e}"));
                    assert_eq!(&got, expected, "{version} {case} {name}");
                }
            }
        }
    }

    #[test]
    fn official_unrar_validates_old_format_solid_store_fallback() {
        let Some(unrar) = official_unrar() else {
            return; // skipped with a visible marker unless SA_REQUIRE_OFFICIAL is set
        };
        let mut failures: Vec<String> = Vec::new();
        for version in [ArchiveVersion::V15, ArchiveVersion::V20] {
            for (case, level, members) in cases() {
                let (_dir, path) = build(version, case, level, &members);
                let out = std::process::Command::new(&unrar)
                    .arg("t")
                    .arg("-idq")
                    .arg(&path)
                    .output()
                    .expect("spawn official unrar");
                if !out.status.success() {
                    failures.push(format!(
                        "{version} {case}:\nstdout: {}\nstderr: {}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr),
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "official unrar t rejected {} case(s):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}

mod rar4_solid_repack_password {
    //! Regression tests: RAR4 solid-repack edits must preserve `-p` member
    //! encryption (delete and the deferred solid-append repack).

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::{
        AppendOptions, ArchiveEditor, ArchiveReader, ArchiveVersion, ArchiveWriter,
        CompressionLevel, EntryWriteOptions, OpenOptions, RarError, SolidMode, WriterOptions,
    };

    fn text(len: usize, seed: usize) -> Vec<u8> {
        let line = b"the quick brown fox jumps over the lazy dog 0123456789\n";
        (0..len)
            .map(|index| line[(index + seed) % line.len()])
            .collect()
    }

    fn normal() -> EntryWriteOptions {
        EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL)
    }

    fn build_solid_encrypted(path: &std::path::Path, payloads: &[(&str, &[u8])]) {
        let mut writer = ArchiveWriter::create_with(
            path,
            WriterOptions::new()
                .compression(ArchiveVersion::V29)
                .solid_mode(SolidMode::Continuous)
                .password("secret"),
        )
        .unwrap();
        for (name, data) in payloads {
            writer.add_bytes(name, data, normal()).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn solid_delete_keeps_member_encryption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("solid-p.rar");
        let f1 = text(40_000, 0);
        let f2 = text(30_000, 7);
        let f3 = text(20_000, 13);
        build_solid_encrypted(&path, &[("f1.bin", &f1), ("f2.bin", &f2), ("f3.bin", &f3)]);

        let mut editor = ArchiveEditor::open_with_password(&path, "secret").unwrap();
        let f2_id = editor.unique_entry("f2.bin").unwrap();
        assert_eq!(editor.delete_entries(&[f2_id]).unwrap(), 1);
        drop(editor);

        // Listing works without a password (headers stay plaintext)...
        let listed = ArchiveReader::open(&path).unwrap();
        let names: Vec<_> = listed
            .entries()
            .map(|entry| entry.name().to_owned())
            .collect();
        assert_eq!(names, ["f1.bin", "f3.bin"]);
        drop(listed);

        // ...but reading still needs it: the repack must keep `-p`.
        let mut no_password = ArchiveReader::open(&path).unwrap();
        let f1_id = no_password.unique_entry("f1.bin").unwrap();
        assert!(
            no_password.read_entry(f1_id).is_err(),
            "repack stripped member encryption"
        );
        drop(no_password);

        let mut reader =
            ArchiveReader::open_with(&path, OpenOptions::new().password("secret")).unwrap();
        let f1_id = reader.unique_entry("f1.bin").unwrap();
        assert_eq!(reader.read_entry(f1_id).unwrap(), f1);
        let f3_id = reader.unique_entry("f3.bin").unwrap();
        assert_eq!(reader.read_entry(f3_id).unwrap(), f3);
        assert!(reader.verify().unwrap().is_ok());
    }

    #[test]
    fn solid_append_keeps_member_encryption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("solid-append-p.rar");
        let f1 = text(30_000, 0);
        let f2 = text(30_000, 3);
        build_solid_encrypted(&path, &[("f1.bin", &f1), ("f2.bin", &f2)]);

        // Appending to a solid archive defers to a whole-archive repack at close.
        let f3 = text(20_000, 11);
        {
            let mut writer =
                ArchiveWriter::append_with(&path, AppendOptions::new().password("secret")).unwrap();
            writer.add_bytes("f3.bin", &f3, normal()).unwrap();
            writer.finish().unwrap();
        }

        let mut no_password = ArchiveReader::open(&path).unwrap();
        assert_eq!(no_password.entries().count(), 3);
        let f1_id = no_password.unique_entry("f1.bin").unwrap();
        assert!(
            no_password.read_entry(f1_id).is_err(),
            "deferred solid append stripped member encryption"
        );
        drop(no_password);

        let mut reader =
            ArchiveReader::open_with(&path, OpenOptions::new().password("secret")).unwrap();
        for (name, expected) in [("f1.bin", &f1), ("f2.bin", &f2), ("f3.bin", &f3)] {
            let id = reader.unique_entry(name).unwrap();
            assert_eq!(&reader.read_entry(id).unwrap(), expected, "{name}");
        }
        assert!(reader.verify().unwrap().is_ok());
    }

    #[test]
    fn solid_delete_without_password_refuses_and_preserves_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("solid-refuse.rar");
        let f1 = text(30_000, 0);
        let f2 = text(30_000, 5);
        build_solid_encrypted(&path, &[("f1.bin", &f1), ("f2.bin", &f2)]);
        let before = std::fs::read(&path).unwrap();

        let mut editor = ArchiveEditor::open(&path).unwrap();
        let f2_id = editor.unique_entry("f2.bin").unwrap();
        let error = editor.delete_entries(&[f2_id]).unwrap_err();
        assert!(
            matches!(error, RarError::Encrypted(_)),
            "expected a clear password error, got {error:?}"
        );
        drop(editor);

        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a refused repack must leave the archive untouched"
        );
        let mut reader =
            ArchiveReader::open_with(&path, OpenOptions::new().password("secret")).unwrap();
        assert_eq!(
            reader
                .read_entry(reader.unique_entry("f1.bin").unwrap())
                .unwrap(),
            f1
        );
    }
}
