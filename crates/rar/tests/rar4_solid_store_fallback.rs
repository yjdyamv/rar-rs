//! Pre-RAR3 (v15/v20) solid chains must stay in lockstep when a member
//! falls back to STORE. The reader derives these chains from the archive-level
//! `MHD_SOLID` flag and member position: a STORE member never reaches the
//! decoder, so the writer's persistent encoder must not advance for it
//! either. Regression for `rar a -ma2 -s -m5` with an incompressible member
//! at the start or in the middle of the run.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar_rs::{
    ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions, SolidMode,
    WriterOptions,
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
/// last release that reads pre-RAR5; 6.23 as fallback). `None` skips the
/// official check.
fn official_unrar() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SA_OFFICIAL_UNRAR") {
        return Some(PathBuf::from(path));
    }
    let exe = if cfg!(windows) { "UnRAR.exe" } else { "unrar" };
    [
        "../../.cache/winrar/7-23",
        "../.cache/winrar/7-23",
        ".cache/winrar/7-23",
        "../../.cache/winrar/6-23",
        "../.cache/winrar/6-23",
        ".cache/winrar/6-23",
    ]
    .iter()
    .map(|dir| Path::new(env!("CARGO_MANIFEST_DIR")).join(dir).join(exe))
    .find(|bin| bin.exists())
}

#[test]
fn old_format_solid_store_fallback_roundtrip() {
    for version in [ArchiveVersion::V15, ArchiveVersion::V20] {
        for (case, level, members) in cases() {
            let (_dir, path) = build(version, case, level, &members);
            let mut reader =
                ArchiveReader::open(&path).unwrap_or_else(|e| panic!("open {version} {case}: {e}"));
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
        eprintln!("SKIP: official UnRAR not found (set SA_OFFICIAL_UNRAR)");
        return;
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
