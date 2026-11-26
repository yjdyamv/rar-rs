//! Damage a *valid* archive where a real storage fault would and assert the
//! ordinary read/extract path stays safe.
//!
//! `robustness.rs` throws random bytes at the parser and
//! `structured_recovery_mutations.rs` mutates recovery records with their
//! checksums recomputed. This suite starts from an archive our own writer
//! just produced — RAR5 (quick-open service record included) and RAR4
//! (`unp_ver 29`), each with a STORE member (CRC-only integrity) and a
//! compressed member (decoder plus CRC) — and damages it the ways a disk
//! does: every single byte flipped, zeroed and overwritten, and every
//! possible truncation.
//!
//! Three invariants must hold for every mutated input:
//!
//! * the read/extract path never panics (a panic is a defect);
//! * a *successful* read or extraction never returns bytes other than the
//!   member's original content (silent corruption is a defect);
//! * a failed extraction never leaves a `.rar5tmp` staging sibling behind.
//!
//! A few targeted cases then pin the *classification* the sweep cannot: a
//! flipped STORE payload must surface as `RarError::Crc`, and a damaged
//! signature must be refused rather than read as an empty archive.
//!
//! On top of the read/extract sweeps, an **operation battery** runs the whole
//! surface over a sampled set of damaged archives — query (list/read/verify/
//! extract), edit (delete, rename, comment, lock, append) and repair (inline
//! record repair, no-record rebuild, legacy PROTECT_HEAD repair, `.rev`
//! volume rebuild). Two operation-specific invariants join the three above:
//! a *failed* edit leaves the file byte-for-byte untouched (the atomic-rewrite
//! promise), and any edit or repair that succeeds leaves an archive whose
//! surviving members still read correctly.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use rar_rs::{
    AppendOptions, ArchiveEditor, ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel,
    EntryId, EntryWriteOptions, ExtractOptions, RarError, WriterOptions,
};

const STORED_NAME: &str = "stored.bin";
const PACKED_NAME: &str = "packed.bin";

fn store() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn normal() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL)
}

/// Deterministic STORE payload (any byte flip is a CRC mismatch).
fn stored_payload() -> Vec<u8> {
    (0..512u32)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect()
}

/// Compressible text for the compressed member.
fn packed_payload() -> Vec<u8> {
    b"the quick brown fox jumps over the lazy dog\n".repeat(48)
}

/// The members every generated archive carries, with their original bytes.
fn members() -> Vec<(String, Vec<u8>)> {
    vec![
        (STORED_NAME.to_string(), stored_payload()),
        (PACKED_NAME.to_string(), packed_payload()),
    ]
}

/// Absolute offset of a member's data area, read from the (intact) archive
/// itself so the location is correct for every container generation.
fn data_offset(path: &Path, name: &str) -> usize {
    let reader = ArchiveReader::open(path).unwrap();
    let id = reader.unique_entry(name).unwrap();
    reader.entry(id).unwrap().data_offset() as usize
}

/// A single-volume RAR5 archive with a quick-open service record.
fn build_rar5_quick_open(dir: &Path) -> PathBuf {
    let path = dir.join("ok5qo.rar");
    let mut writer =
        ArchiveWriter::create_with(&path, WriterOptions::new().quick_open(true)).unwrap();
    writer
        .add_bytes(STORED_NAME, &stored_payload(), store())
        .unwrap();
    writer
        .add_bytes(PACKED_NAME, &packed_payload(), normal())
        .unwrap();
    writer.finish().unwrap();
    path
}

/// The same RAR5 archive behind a deterministic SFX-style stub, so the reader
/// and the rewrite paths locate the archive at a non-zero `sfx_offset`.
fn build_rar5_sfx(dir: &Path) -> PathBuf {
    let plain = build_rar5_quick_open(dir);
    let archive = std::fs::read(&plain).unwrap();
    let mut stub = vec![0u8; 4096];
    for (index, byte) in stub.iter_mut().enumerate() {
        *byte = (index.wrapping_mul(31).wrapping_add(7) & 0xFF) as u8;
    }
    stub.extend_from_slice(&archive);
    let path = dir.join("ok5sfx.exe");
    std::fs::write(&path, &stub).unwrap();
    path
}

/// A RAR5 archive with header encryption (`-hp`); editing it needs the
/// password, and the rewrite re-encrypts every rebuilt header.
fn build_rar5_hp(dir: &Path) -> PathBuf {
    let path = dir.join("ok5hp.rar");
    let mut writer = ArchiveWriter::create_with(
        &path,
        WriterOptions::new()
            .password("damage-pw")
            .encrypt_headers(true),
    )
    .unwrap();
    writer
        .add_bytes(STORED_NAME, &stored_payload(), store())
        .unwrap();
    writer
        .add_bytes(PACKED_NAME, &packed_payload(), normal())
        .unwrap();
    writer.finish().unwrap();
    path
}

/// The password the `-hp` fixture uses.
const HP_PASSWORD: &str = "damage-pw";

/// A single-volume RAR4 (`unp_ver 29`) archive.
fn build_rar4(dir: &Path) -> PathBuf {
    let path = dir.join("ok4.rar");
    let mut writer =
        ArchiveWriter::create_with(&path, WriterOptions::new().compression(ArchiveVersion::V29))
            .unwrap();
    writer
        .add_bytes(STORED_NAME, &stored_payload(), store())
        .unwrap();
    writer
        .add_bytes(PACKED_NAME, &packed_payload(), normal())
        .unwrap();
    writer.finish().unwrap();
    path
}

/// A solid RAR4 archive (a shared window makes data damage propagate across
/// members, so it exercises the chain reset).
fn build_rar4_solid(dir: &Path) -> PathBuf {
    let path = dir.join("ok4s.rar");
    let mut writer = ArchiveWriter::create_with(
        &path,
        WriterOptions::new()
            .compression(ArchiveVersion::V29)
            .solid_mode(rar_rs::SolidMode::Continuous),
    )
    .unwrap();
    writer
        .add_bytes(STORED_NAME, &stored_payload(), normal())
        .unwrap();
    writer
        .add_bytes(PACKED_NAME, &packed_payload(), normal())
        .unwrap();
    writer.finish().unwrap();
    path
}

/// Two-volume RAR5 set: the first member spans the split.
fn build_rar5_multivolume(dir: &Path) -> PathBuf {
    let path = dir.join("set.rar");
    let mut writer =
        ArchiveWriter::create_with(&path, WriterOptions::new().volume_size(1024)).unwrap();
    writer
        .add_bytes(STORED_NAME, &stored_payload(), store())
        .unwrap();
    writer
        .add_bytes(PACKED_NAME, &packed_payload(), store())
        .unwrap();
    writer.finish().unwrap();
    path
}

/// Two-volume RAR5 set carrying a `.rev` recovery volume.
fn build_rar5_set_with_rev(path: &Path) {
    let mut writer = ArchiveWriter::create_with(
        path,
        WriterOptions::new()
            .volume_size(1024)
            .recovery_volume_count(1),
    )
    .unwrap();
    writer
        .add_bytes(STORED_NAME, &stored_payload(), store())
        .unwrap();
    writer
        .add_bytes(PACKED_NAME, &packed_payload(), store())
        .unwrap();
    writer.finish().unwrap();
}

/// Every regular file in `dir` and its bytes, sorted by path.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

/// Whether `dir` still holds exactly the files in `snap`, byte-for-byte.
fn unchanged(dir: &Path, snap: &[(PathBuf, Vec<u8>)]) -> bool {
    snapshot(dir) == snap
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// Open, list, read and extract `path`, asserting the safety invariants
/// against the archive's original member set. Any violation panics with a
/// message the caller's `catch_unwind` turns into a labelled failure.
fn check_safe(path: &Path, out: &Path, original: &[(String, Vec<u8>)]) {
    let _ = std::fs::remove_dir_all(out);
    let opts = ExtractOptions {
        safe_paths: true,
        max_unpacked_bytes: Some(8 * 1024 * 1024),
        max_total_unpacked_bytes: Some(16 * 1024 * 1024),
        ..Default::default()
    };

    let Ok(mut reader) = ArchiveReader::open(path) else {
        return; // refusing a damaged archive outright is safe
    };

    // Listing must never invent a member the original archive did not have,
    // and a member that still reads must read *correctly*.
    let listed: Vec<(String, EntryId)> = reader
        .entries()
        .map(|entry| (entry.name().to_string(), entry.id()))
        .collect();
    for (name, id) in listed {
        let expected = original
            .iter()
            .find(|(member, _)| member == &name)
            .map(|(_, bytes)| bytes.as_slice());
        match (reader.read_entry(id), expected) {
            (Ok(got), Some(bytes)) => assert!(
                got == bytes,
                "a damaged archive returned wrong bytes for {name}"
            ),
            (Ok(_), None) => panic!("a damaged archive invented member {name}"),
            (Err(_), _) => {}
        }
    }

    let _ = reader.extract_all_with_options(out, opts);

    if out.is_dir() {
        for entry in std::fs::read_dir(out).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                !name.contains("rar5tmp"),
                "a failed extraction left staging sibling {name}"
            );
            let expected = original
                .iter()
                .find(|(member, _)| member == &name)
                .map(|(_, bytes)| bytes.as_slice());
            match expected {
                Some(bytes) => assert_eq!(
                    std::fs::read(out.join(&name)).unwrap(),
                    bytes,
                    "extraction wrote wrong bytes for {name}"
                ),
                None => panic!("extraction wrote unexpected member {name}"),
            }
        }
    }
}

/// Baseline: the generated archives really read and extract, so a sweep
/// failure is attributable to the mutation and not to the fixture.
#[test]
fn valid_archives_read_and_extract() {
    let dir = tempfile::tempdir().unwrap();
    let original = members();
    for (label, path) in [
        ("rar5-qo", build_rar5_quick_open(dir.path())),
        ("rar4", build_rar4(dir.path())),
        ("rar4-solid", build_rar4_solid(dir.path())),
    ] {
        let mut reader = ArchiveReader::open(&path).unwrap_or_else(|e| panic!("{label}: {e}"));
        for (name, bytes) in &original {
            let id = reader
                .unique_entry(name)
                .unwrap_or_else(|e| panic!("{label}: {name}: {e}"));
            assert_eq!(&reader.read_entry(id).unwrap(), bytes, "{label}: {name}");
        }
        let out = dir.path().join(format!("baseline-{label}"));
        reader
            .extract_all_with_options(&out, ExtractOptions::default())
            .unwrap_or_else(|e| panic!("{label} extract: {e}"));
        for (name, bytes) in &original {
            assert_eq!(&std::fs::read(out.join(name)).unwrap(), bytes, "{label}");
        }
    }
}

/// Every single-byte mutation of a valid archive must stay safe. The flip
/// pass is exhaustive; the zero/overwrite passes stride so the sweep stays
/// within a modest wall clock while still covering every region.
#[test]
fn single_byte_corruption_is_safe() {
    let dir = tempfile::tempdir().unwrap();
    let original = members();
    for (label, path) in [
        ("rar5-qo", build_rar5_quick_open(dir.path())),
        ("rar4", build_rar4(dir.path())),
        ("rar4-solid", build_rar4_solid(dir.path())),
    ] {
        let good = std::fs::read(&path).unwrap();
        let work = dir.path().join(format!("mut-{label}.rar"));
        let out = dir.path().join(format!("out-{label}"));
        let mut failures: Vec<String> = Vec::new();
        let mut checked = 0usize;

        let mut probe = |offset: usize, replacement: u8, tag: &str| {
            let mut bytes = good.clone();
            bytes[offset] = replacement;
            std::fs::write(&work, &bytes).unwrap();
            checked += 1;
            if let Err(payload) =
                catch_unwind(AssertUnwindSafe(|| check_safe(&work, &out, &original)))
            {
                failures.push(format!(
                    "{label} offset {offset} {tag}: {}",
                    panic_message(&*payload)
                ));
            }
        };

        for (offset, &byte) in good.iter().enumerate() {
            probe(offset, byte ^ 0x01, "flip");
        }
        for offset in (0..good.len()).step_by(7) {
            probe(offset, 0x00, "zero");
            probe(offset, 0xFF, "ones");
        }

        assert!(checked > good.len(), "{label}: sweep did not run");
        assert!(
            failures.is_empty(),
            "{label}: {} unsafe mutations: {failures:?}",
            failures.len()
        );
    }
}

/// Every truncation of a valid archive must stay safe: dropping any suffix,
/// from the signature to the last byte, may only ever produce a classified
/// error or a correct partial read.
#[test]
fn truncation_at_every_length_is_safe() {
    let dir = tempfile::tempdir().unwrap();
    let original = members();
    for (label, path) in [
        ("rar5-qo", build_rar5_quick_open(dir.path())),
        ("rar4", build_rar4(dir.path())),
    ] {
        let good = std::fs::read(&path).unwrap();
        let work = dir.path().join(format!("trunc-{label}.rar"));
        let out = dir.path().join(format!("out-trunc-{label}"));
        let mut failures: Vec<String> = Vec::new();
        for len in 0..good.len() {
            std::fs::write(&work, &good[..len]).unwrap();
            if let Err(payload) =
                catch_unwind(AssertUnwindSafe(|| check_safe(&work, &out, &original)))
            {
                failures.push(format!(
                    "{label} truncated to {len}: {}",
                    panic_message(&*payload)
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{label}: {} unsafe truncations: {failures:?}",
            failures.len()
        );
    }
}

/// A flipped byte in a STORE member's payload is exactly a CRC mismatch,
/// reported on read, on extraction and by `verify` — and the failed
/// extraction leaves no output behind.
#[test]
fn stored_member_payload_flip_is_a_crc_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = build_rar5_quick_open(dir.path());
    let mut bytes = std::fs::read(&path).unwrap();
    let at = data_offset(&path, STORED_NAME) + 7;
    bytes[at] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let mut reader = ArchiveReader::open(&path).unwrap();
    let id = reader.unique_entry(STORED_NAME).unwrap();
    match reader.read_entry(id) {
        Err(RarError::Crc { .. }) => {}
        other => panic!("expected a CRC mismatch from read, got {other:?}"),
    }

    let out = dir.path().join("out");
    match reader.extract_entry(id, &out) {
        Err(RarError::Crc { .. }) => {}
        other => panic!("expected a CRC mismatch from extract, got {other:?}"),
    }
    assert!(
        !out.join(STORED_NAME).exists(),
        "a failed extraction must not leave the member behind"
    );
    drop(reader);

    let mut archive = ArchiveReader::open(&path).unwrap();
    let report = archive.verify().unwrap();
    assert_eq!(
        (report.passed() + report.failed(), report.failed()),
        (2, 1),
        "verify must report exactly the damaged member"
    );

    // The untouched compressed member still round-trips.
    let mut reader = ArchiveReader::open(&path).unwrap();
    let id = reader.unique_entry(PACKED_NAME).unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), packed_payload());
}

/// A flipped byte inside a STORE member's data is caught by the CRC even
/// when the member is later in a solid chain (whose window was seeded by the
/// intact first member).
#[test]
fn solid_chain_payload_flip_is_classified() {
    let dir = tempfile::tempdir().unwrap();
    let path = build_rar4_solid(dir.path());
    let mut bytes = std::fs::read(&path).unwrap();
    let at = data_offset(&path, PACKED_NAME) + 5;
    bytes[at] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let mut reader = ArchiveReader::open(&path).unwrap();
    let id = reader.unique_entry(PACKED_NAME).unwrap();
    match reader.read_entry(id) {
        Ok(got) => assert_ne!(got, packed_payload(), "a damaged member read back intact"),
        Err(error) => assert!(
            matches!(
                error.code(),
                rar_rs::ErrorCode::CrcMismatch | rar_rs::ErrorCode::Format
            ),
            "unexpected classification: {error}"
        ),
    }
    // The intact first member is unaffected.
    let id = reader.unique_entry(STORED_NAME).unwrap();
    assert_eq!(reader.read_entry(id).unwrap(), stored_payload());
}

/// A damaged signature must be refused, not read as an empty archive.
#[test]
fn damaged_signature_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    for (label, path) in [
        ("rar5", build_rar5_quick_open(dir.path())),
        ("rar4", build_rar4(dir.path())),
    ] {
        let good = std::fs::read(&path).unwrap();
        let work = dir.path().join(format!("sig-{label}.rar"));
        let mut bytes = good.clone();
        bytes[3] ^= 0xFF; // corrupt "Rar!" -> the detector must not accept it
        std::fs::write(&work, &bytes).unwrap();

        match ArchiveReader::open(&work) {
            Err(error) => assert!(
                matches!(
                    error.code(),
                    rar_rs::ErrorCode::Format | rar_rs::ErrorCode::Io
                ),
                "{label}: unexpected classification {error}"
            ),
            Ok(_) => panic!("{label}: a corrupted signature was accepted"),
        }
    }
}

/// A missing middle/last volume of a set must stay safe and classified: the
/// open may fail, or list a prefix whose members read correctly, but it must
/// never invent data.
#[test]
fn a_missing_volume_is_safe() {
    let dir = tempfile::tempdir().unwrap();
    let base = build_rar5_multivolume(dir.path());
    let original = members();
    let volumes = rar_rs::discover_volumes(&base);
    assert!(volumes.len() >= 2, "precondition: multi-volume set");

    let out = dir.path().join("out-missing");
    for victim in &volumes[1..] {
        let saved = std::fs::read(victim).unwrap();
        std::fs::remove_file(victim).unwrap();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            check_safe(&volumes[0], &out, &original)
        }));
        if let Err(payload) = outcome {
            std::fs::write(victim, &saved).unwrap();
            panic!(
                "missing {} panicked: {}",
                victim.display(),
                panic_message(&*payload)
            );
        }
        std::fs::write(victim, &saved).unwrap();
    }
}

// ── The operation battery ──────────────────────────────────────────────────
//
// The sweeps above stop at the read/extract surface because they run on
// every byte. The battery below runs the *whole* operation set — query,
// edit (增删改) and repair (修复) — over a *sampled* set of damaged inputs,
// asserting the same safety contract plus two operation-specific ones:
//
// * a mutating edit that fails leaves the archive byte-for-byte untouched
//   (the atomic-rewrite promise), and
// * any edit or repair that succeeds leaves an archive whose surviving
//   members still read *correctly*.

/// Every member of `path` that still lists under an original name must read
/// back its original bytes (a member under a new name — a rename or append
/// result — is not compared).
fn verify_known_members(path: &Path, original: &[(String, Vec<u8>)]) {
    let Ok(mut reader) = ArchiveReader::open(path) else {
        return;
    };
    let listed: Vec<(String, EntryId)> = reader
        .entries()
        .map(|entry| (entry.name().to_string(), entry.id()))
        .collect();
    for (name, id) in listed {
        if let Some((_, bytes)) = original.iter().find(|(member, _)| member == &name)
            && let Ok(got) = reader.read_entry(id)
        {
            assert!(got == *bytes, "member {name} came back corrupted");
        }
    }
}

/// Run one mutating edit on a throwaway copy of `base` and assert the edit is
/// either a classified failure that left the file untouched, or a success
/// whose survivors still read correctly.
fn check_edit(
    dir: &Path,
    name: &str,
    base: &[u8],
    original: &[(String, Vec<u8>)],
    edit: impl FnOnce(&mut ArchiveEditor) -> rar_rs::RarResult<()>,
) {
    let copy = dir.join(format!("{name}.rar"));
    std::fs::write(&copy, base).unwrap();
    let result = ArchiveEditor::open(&copy).and_then(|mut editor| edit(&mut editor));
    match result {
        Ok(()) => verify_known_members(&copy, original),
        Err(error) => {
            let after = std::fs::read(&copy).unwrap();
            assert!(
                after == base,
                "{name}: a failed edit ({error}) modified the archive ({} -> {} bytes)",
                base.len(),
                after.len()
            );
        }
    }
}

/// Run an append on a throwaway copy (增) under the same contract as
/// [`check_edit`].
fn check_append(dir: &Path, name: &str, base: &[u8], original: &[(String, Vec<u8>)]) {
    let copy = dir.join(format!("{name}.rar"));
    std::fs::write(&copy, base).unwrap();
    let result = (|| -> rar_rs::RarResult<()> {
        let mut writer = ArchiveWriter::append_with(&copy, AppendOptions::new())?;
        writer.add_bytes("appended.bin", b"appended member payload", store())?;
        writer.finish()?;
        Ok(())
    })();
    match result {
        Ok(()) => verify_known_members(&copy, original),
        Err(error) => {
            let after = std::fs::read(&copy).unwrap();
            assert!(
                after == base,
                "{name}: a failed append ({error}) modified the archive ({} -> {} bytes)",
                base.len(),
                after.len()
            );
        }
    }
}

/// Run the full operation set over one damaged archive: query, then each edit
/// on its own copy, then every repair/reconstruct entry point. Any panic is a
/// defect; a successful result must read correctly; a failed edit must not
/// have touched the file.
fn check_operations(
    path: &Path,
    dir: &Path,
    name: &str,
    original: &[(String, Vec<u8>)],
    legacy: bool,
) {
    // 查: list, read, verify, extract.
    check_safe(path, &dir.join(format!("{name}-out")), original);
    let base = std::fs::read(path).unwrap();

    // 删: delete the first surviving member.
    check_edit(dir, &format!("{name}-del"), &base, original, |editor| {
        let ids: Vec<EntryId> = editor.entries().map(|entry| entry.id()).collect();
        if ids.is_empty() {
            return Ok(());
        }
        editor.delete_entries(&ids[..1]).map(|_| ())
    });
    // 改(名): rename the first surviving member.
    check_edit(dir, &format!("{name}-ren"), &base, original, |editor| {
        let ids: Vec<EntryId> = editor.entries().map(|entry| entry.id()).collect();
        if ids.is_empty() {
            return Ok(());
        }
        editor
            .rename_entries(&[(ids[0], "renamed.bin".to_string())])
            .map(|_| ())
    });
    // 改(注释): replace the archive comment.
    check_edit(dir, &format!("{name}-cmt"), &base, original, |editor| {
        editor
            .apply(rar_rs::EditPlan::new().set_comment(b"damage-probe".to_vec()))
            .map(|_| ())
    });
    // 改(锁定): lock (irreversible, so last among edits and always on a copy).
    check_edit(dir, &format!("{name}-lock"), &base, original, |editor| {
        editor.lock()
    });
    // 增: append a member.
    check_append(dir, &format!("{name}-app"), &base, original);

    // 修复: the in-memory repair, the streaming repair, the no-record
    // rebuild, and (legacy only) the PROTECT_HEAD repair.
    if let Ok(repaired) = rar_rs::repair_archive(&base) {
        let out = dir.join(format!("{name}-rr.rar"));
        std::fs::write(&out, &repaired).unwrap();
        verify_known_members(&out, original);
    }
    let repaired = dir.join(format!("{name}-repaired.rar"));
    if rar_rs::repair_archive_path(path, &repaired).is_ok() {
        verify_known_members(&repaired, original);
    }
    let rebuilt = dir.join(format!("{name}-rebuilt.rar"));
    if rar_rs::reconstruct_archive_path(path, &rebuilt, None).is_ok() {
        verify_known_members(&rebuilt, original);
    }
    if legacy {
        let fixed = dir.join(format!("{name}-fixed.rar"));
        if rar_rs::repair_legacy_archive_path(path, &fixed).is_ok() {
            verify_known_members(&fixed, original);
        }
    }
}

/// A sampled set of damaged archives must survive the whole operation
/// battery. `flip`/`zero`/`ones` are applied at strided offsets and the
/// archive is truncated at strided lengths, so every structural region
/// (signature, headers, service records, member data, end marker) is hit on
/// some case without running the heavy battery at every byte.
#[test]
fn damaged_archives_survive_the_operation_battery() {
    let dir = tempfile::tempdir().unwrap();
    let original = members();
    let shapes: Vec<(&str, PathBuf, bool)> = vec![
        ("rar5-qo", build_rar5_quick_open(dir.path()), false),
        ("rar5-sfx", build_rar5_sfx(dir.path()), false),
        ("rar4", build_rar4(dir.path()), true),
        ("rar4-solid", build_rar4_solid(dir.path()), true),
    ];

    for (label, path, legacy) in shapes {
        let good = std::fs::read(&path).unwrap();
        let work = dir.path().join(format!("work-{label}.rar"));
        let stride = (good.len() / 10).max(1);

        let mut cases: Vec<(String, Vec<u8>)> = Vec::new();
        for offset in (0..good.len()).step_by(stride) {
            for (tag, replacement) in [
                ("flip", good[offset] ^ 0x01),
                ("zero", 0x00),
                ("ones", 0xFF),
            ] {
                let mut bytes = good.clone();
                bytes[offset] = replacement;
                cases.push((format!("off{offset}-{tag}"), bytes));
            }
        }
        for len in (0..good.len()).step_by(stride) {
            cases.push((format!("trunc{len}"), good[..len].to_vec()));
        }

        let mut failures: Vec<String> = Vec::new();
        for (case_name, bytes) in cases {
            std::fs::write(&work, &bytes).unwrap();
            let tag = format!("{label}-{case_name}");
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                check_operations(&work, dir.path(), &tag, &original, legacy)
            }));
            if let Err(payload) = outcome {
                failures.push(format!("{tag}: {}", panic_message(&*payload)));
            }
        }
        assert!(
            failures.is_empty(),
            "{label}: {} unsafe operation cases: {failures:?}",
            failures.len()
        );
    }
}

/// A multi-volume set that loses a volume is repaired (修复) through its
/// `.rev` parity, and a byte damaged in any single volume stays safe to read
/// (查) — the whole set reads back correctly once the volume is rebuilt.
#[test]
fn multivolume_set_damage_and_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("set.rar");
    build_rar5_set_with_rev(&base);
    let volumes = rar_rs::discover_volumes(&base);
    assert!(volumes.len() >= 2, "precondition: multi-volume set");
    let original = members();
    let out = dir.path().join("out-mv");

    // 查: a flipped byte in any one volume stays safe to read.
    for index in 0..volumes.len() {
        let saved = std::fs::read(&volumes[index]).unwrap();
        let mut bytes = saved.clone();
        bytes[saved.len() / 2] ^= 0xFF;
        std::fs::write(&volumes[index], &bytes).unwrap();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            check_safe(&volumes[0], &out, &original)
        }));
        std::fs::write(&volumes[index], &saved).unwrap();
        if let Err(payload) = outcome {
            panic!(
                "a damaged volume {index} panicked: {}",
                panic_message(&*payload)
            );
        }
    }

    // 修复: removing a middle volume and rebuilding it from `.rev` restores
    // the exact original bytes.
    let victim = volumes[1].clone();
    let saved = std::fs::read(&victim).unwrap();
    std::fs::remove_file(&victim).unwrap();
    match rar_rs::rebuild_missing_volumes(&volumes[0]) {
        Ok(rebuilt) => {
            assert!(
                rebuilt.contains(&victim),
                "the missing volume must be rebuilt: {rebuilt:?}"
            );
            assert_eq!(
                std::fs::read(&victim).unwrap(),
                saved,
                "the rebuilt volume must match the original bytes"
            );
        }
        Err(error) => {
            // A refusal is acceptable, but then the volume must still be
            // restored so the set stays readable and the test deterministic.
            std::fs::write(&victim, &saved).unwrap();
            panic!("a `.rev`-protected set refused to rebuild: {error}");
        }
    }

    // The re-completed set reads every member back.
    let mut reader = ArchiveReader::open(&volumes[0]).unwrap();
    for (name, bytes) in &original {
        let id = reader.unique_entry(name).unwrap();
        assert_eq!(&reader.read_entry(id).unwrap(), bytes, "{name}");
    }
}

/// Editing (增删改) a damaged multi-volume set must stay atomic: a byte is
/// flipped in one volume, then a rename and a delete run against the set. An
/// error must leave every volume and `.rev` file byte-for-byte untouched; a
/// success must leave the survivors readable.
#[test]
fn multivolume_damage_survives_edits() {
    let root = tempfile::tempdir().unwrap();
    let original = members();

    // Learn the set's shape from one reference copy.
    let reference = root.path().join("reference");
    std::fs::create_dir(&reference).unwrap();
    build_rar5_set_with_rev(&reference.join("set.rar"));
    let reference_volumes = rar_rs::discover_volumes(&reference.join("set.rar"));
    assert!(
        reference_volumes.len() >= 2,
        "precondition: multi-volume set"
    );

    let mut failures: Vec<String> = Vec::new();
    let mut case = 0usize;
    for index in 0..reference_volumes.len() {
        let pristine = std::fs::read(&reference_volumes[index]).unwrap();
        let damages: Vec<(&str, usize, bool)> = vec![
            ("flip-a", pristine.len() / 3, false),
            ("flip-b", 2 * pristine.len() / 3, false),
            ("trunc-half", pristine.len() / 2, true),
            ("trunc-quarter", pristine.len() / 4, true),
        ];
        for (dkind, value, truncate) in damages {
            for op in ["rename", "delete"] {
                case += 1;
                // One isolated directory per case keeps every op independent.
                let dir = root.path().join(format!("case{case}"));
                std::fs::create_dir(&dir).unwrap();
                let base = dir.join("set.rar");
                build_rar5_set_with_rev(&base);
                let volumes = rar_rs::discover_volumes(&base);
                let mut damaged = std::fs::read(&volumes[index]).unwrap();
                if truncate {
                    damaged.truncate(value.max(1));
                } else {
                    let at = value.min(damaged.len() - 1);
                    damaged[at] ^= 0x01;
                }
                std::fs::write(&volumes[index], &damaged).unwrap();
                let before = snapshot(&dir);
                let label = format!("case{case} (vol {index} {dkind} {op})");

                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    ArchiveEditor::open(&volumes[0]).and_then(|mut editor| {
                        let ids: Vec<EntryId> = editor.entries().map(|e| e.id()).collect();
                        if ids.is_empty() {
                            return Ok(());
                        }
                        if op == "rename" {
                            editor
                                .rename_entries(&[(ids[0], "renamed.bin".to_string())])
                                .map(|_| ())
                        } else {
                            editor.delete_entries(&ids[..1]).map(|_| ())
                        }
                    })
                }));
                match outcome {
                    Err(payload) => {
                        failures.push(format!("{label} panicked: {}", panic_message(&*payload)))
                    }
                    Ok(Err(_)) => {
                        if !unchanged(&dir, &before) {
                            failures.push(format!("{label}: a failed edit modified the set"));
                        }
                    }
                    Ok(Ok(())) => verify_known_members(&volumes[0], &original),
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} unsafe multi-volume edits: {failures:?}",
        failures.len()
    );
}

/// A damaged STORE member extracted with `keep_broken` must write the *partial*
/// (here: damaged) bytes, never the intact original, and must leave no staging
/// sibling behind. Extends the safety contract to WinRAR's keep-broken mode.
#[test]
fn keep_broken_extraction_writes_partial_but_no_staging() {
    let dir = tempfile::tempdir().unwrap();
    let path = build_rar5_quick_open(dir.path());
    let mut bytes = std::fs::read(&path).unwrap();
    let at = data_offset(&path, STORED_NAME) + 7;
    bytes[at] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let out = dir.path().join("out-keep");
    let opts = ExtractOptions {
        keep_broken: true,
        safe_paths: true,
        max_unpacked_bytes: Some(8 * 1024 * 1024),
        ..Default::default()
    };
    let mut reader = ArchiveReader::open(&path).unwrap();
    let _ = reader.extract_all_with_options(&out, opts);

    if !out.is_dir() {
        return;
    }
    for entry in std::fs::read_dir(&out).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.contains("rar5tmp"), "keep_broken left staging {name}");
    }
    let broken = out.join(STORED_NAME);
    if broken.exists() {
        assert_ne!(
            std::fs::read(&broken).unwrap(),
            stored_payload(),
            "keep_broken wrote the intact bytes for a damaged member"
        );
    }
}

/// A truncated archive must never be *edited* into a half-written file: at a
/// strided set of truncation lengths (both container families), a rename and a
/// delete either fail and leave the file byte-for-byte untouched, or succeed
/// and leave the survivors readable. This is the operation-level twin of
/// `truncation_at_every_length_is_safe`, and it is what found the RAR5
/// commit-then-replace defect the read sweep could not see.
#[test]
fn truncated_edits_are_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let original = members();
    for (label, path) in [
        ("rar5", build_rar5_quick_open(dir.path())),
        ("rar4", build_rar4(dir.path())),
    ] {
        let good = std::fs::read(&path).unwrap();
        let work = dir.path().join(format!("trunc-edit-{label}.rar"));
        let stride = (good.len() / 400).max(1);
        let mut failures: Vec<String> = Vec::new();
        for len in (0..good.len()).step_by(stride) {
            for op in ["rename", "delete"] {
                std::fs::write(&work, &good[..len]).unwrap();
                let before = std::fs::read(&work).unwrap();
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    ArchiveEditor::open(&work).and_then(|mut editor| {
                        let ids: Vec<EntryId> = editor.entries().map(|e| e.id()).collect();
                        if ids.is_empty() {
                            return Ok(());
                        }
                        if op == "rename" {
                            editor
                                .rename_entries(&[(ids[0], "renamed.bin".to_string())])
                                .map(|_| ())
                        } else {
                            editor.delete_entries(&ids[..1]).map(|_| ())
                        }
                    })
                }));
                match outcome {
                    Err(payload) => failures.push(format!(
                        "{label} trunc{len} {op}: panicked: {}",
                        panic_message(&*payload)
                    )),
                    Ok(Ok(())) => verify_known_members(&work, &original),
                    Ok(Err(error)) => {
                        let after = std::fs::read(&work).unwrap_or_default();
                        if after != before {
                            failures.push(format!(
                                "{label} trunc{len} {op}: a failed edit ({error}) modified the \
                                 archive ({} -> {} bytes)",
                                before.len(),
                                after.len()
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{label}: {} unsafe truncated edits: {failures:?}",
            failures.len()
        );
    }
}

/// Like [`verify_known_members`] but for a header-encrypted archive.
fn verify_known_members_hp(path: &Path, original: &[(String, Vec<u8>)]) {
    let Ok(mut reader) =
        ArchiveReader::open_with(path, rar_rs::OpenOptions::new().password(HP_PASSWORD))
    else {
        return;
    };
    let listed: Vec<(String, EntryId)> = reader
        .entries()
        .map(|entry| (entry.name().to_string(), entry.id()))
        .collect();
    for (name, id) in listed {
        if let Some((_, bytes)) = original.iter().find(|(member, _)| member == &name)
            && let Ok(got) = reader.read_entry(id)
        {
            assert!(got == *bytes, "member {name} came back corrupted");
        }
    }
}

/// The `-hp` (header-encrypted) edit path shares the rewrite engine but reads
/// encrypted headers, so it is exercised separately: a truncated encrypted
/// archive must obey the same atomicity contract under a password-carrying
/// edit.
#[test]
fn truncated_hp_edits_are_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let original = members();
    let path = build_rar5_hp(dir.path());
    let good = std::fs::read(&path).unwrap();
    let work = dir.path().join("trunc-hp.rar");
    let stride = (good.len() / 300).max(1);
    let mut failures: Vec<String> = Vec::new();
    for len in (0..good.len()).step_by(stride) {
        for op in ["rename", "delete"] {
            std::fs::write(&work, &good[..len]).unwrap();
            let before = std::fs::read(&work).unwrap();
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                ArchiveEditor::open_with_password(&work, HP_PASSWORD).and_then(|mut editor| {
                    let ids: Vec<EntryId> = editor.entries().map(|e| e.id()).collect();
                    if ids.is_empty() {
                        return Ok(());
                    }
                    if op == "rename" {
                        editor
                            .rename_entries(&[(ids[0], "renamed.bin".to_string())])
                            .map(|_| ())
                    } else {
                        editor.delete_entries(&ids[..1]).map(|_| ())
                    }
                })
            }));
            match outcome {
                Err(payload) => failures.push(format!(
                    "hp trunc{len} {op}: panicked: {}",
                    panic_message(&*payload)
                )),
                Ok(Ok(())) => verify_known_members_hp(&work, &original),
                Ok(Err(error)) => {
                    let after = std::fs::read(&work).unwrap_or_default();
                    if after != before {
                        failures.push(format!(
                            "hp trunc{len} {op}: a failed edit ({error}) modified the archive \
                             ({} -> {} bytes)",
                            before.len(),
                            after.len()
                        ));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} unsafe hp truncated edits: {failures:?}",
        failures.len()
    );
}
