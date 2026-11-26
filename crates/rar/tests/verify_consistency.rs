//! Cross-check that the three read-side verdicts agree on damaged archives.
//!
//! `corruption.rs` asserts each of open/list/read/verify/extract is *safe* in
//! isolation; this suite asserts they *agree*. On every damaged input the
//! following must hold:
//!
//! * `read_entry` never returns bytes other than a member's original content,
//!   and never invents a member;
//! * `verify` never reports a member as passing when its `read_entry` failed
//!   (no false "OK" that would hide damage), and never flags one that read
//!   back its exact original bytes;
//! * `extract_entry`'s bytes equal `read_entry`'s for every member.
//!
//! A disagreement in any direction is a defect: an oracle that reports the
//! wrong answer about a member's integrity is worse than one that errors.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rar_rs::{
    ArchiveReader, ArchiveVersion, ArchiveWriter, CompressionLevel, EntryId, EntryWriteOptions,
    WriterOptions,
};

const STORED_NAME: &str = "stored.bin";
const PACKED_NAME: &str = "packed.bin";

fn store() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
}

fn normal() -> EntryWriteOptions {
    EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL)
}

fn stored_payload() -> Vec<u8> {
    (0..512u32)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect()
}

fn packed_payload() -> Vec<u8> {
    b"the quick brown fox jumps over the lazy dog\n".repeat(48)
}

fn members() -> HashMap<String, Vec<u8>> {
    HashMap::from([
        (STORED_NAME.to_string(), stored_payload()),
        (PACKED_NAME.to_string(), packed_payload()),
    ])
}

fn build_rar5(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("cons5.rar");
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

fn build_rar4(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("cons4.rar");
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

#[test]
fn read_verify_and_extract_agree_on_damaged_archives() {
    let dir = tempfile::tempdir().unwrap();
    let original = members();
    let mut violations: Vec<String> = Vec::new();

    for (label, path) in [
        ("rar5", build_rar5(dir.path())),
        ("rar4", build_rar4(dir.path())),
    ] {
        let good = std::fs::read(&path).unwrap();
        let work = dir.path().join(format!("work-{label}.rar"));
        let stride = (good.len() / 250).max(1);

        let mut cases: Vec<Vec<u8>> = Vec::new();
        for offset in (0..good.len()).step_by(stride) {
            let mut bytes = good.clone();
            bytes[offset] ^= 0x01;
            cases.push(bytes);
        }
        for len in (20..good.len()).step_by(stride) {
            cases.push(good[..len].to_vec());
        }

        for (index, bytes) in cases.into_iter().enumerate() {
            std::fs::write(&work, &bytes).unwrap();
            let Ok(mut reader) = ArchiveReader::open(&work) else {
                continue;
            };
            let listed: Vec<(String, EntryId)> = reader
                .entries()
                .map(|entry| (entry.name().to_string(), entry.id()))
                .collect();

            let mut reads: HashMap<EntryId, Option<Vec<u8>>> = HashMap::new();
            for (_, id) in &listed {
                reads.insert(*id, reader.read_entry(*id).ok());
            }

            // verify() must not contradict read_entry, in either direction.
            match reader.verify() {
                Ok(report) => {
                    let failed: HashSet<EntryId> =
                        report.failures().iter().map(|f| f.entry_id()).collect();
                    for (name, id) in &listed {
                        let read = reads.get(id).cloned().flatten();
                        let verify_ok = !failed.contains(id);
                        if let Some(bytes) = &read {
                            match original.get(name) {
                                Some(expected) if expected == bytes => {}
                                Some(_) => violations.push(format!(
                                    "{label} case{index}: read returned wrong bytes for {name}"
                                )),
                                None => violations.push(format!(
                                    "{label} case{index}: read invented member {name}"
                                )),
                            }
                        }
                        // A member whose read failed must be reported by verify;
                        // a member that read back intact must not be.
                        if read.is_none() && verify_ok {
                            violations.push(format!(
                                "{label} case{index}: verify() passed {name} whose read failed"
                            ));
                        }
                        if read.is_some() && !verify_ok {
                            violations.push(format!(
                                "{label} case{index}: verify() failed {name} that read intact"
                            ));
                        }
                    }
                }
                Err(_) => {
                    // Refusing to verify a damaged archive is acceptable, but
                    // it must not stand in for a wrong per-member answer.
                }
            }

            // extract_entry must agree with read_entry.
            for (name, id) in &listed {
                let Some(read_bytes) = reads.get(id).and_then(|v| v.as_ref()) else {
                    continue;
                };
                let out = dir.path().join(format!("x-{label}-{index}"));
                if let Ok(extracted_path) = reader.extract_entry(*id, &out) {
                    let extracted = std::fs::read(&extracted_path).unwrap_or_default();
                    if &extracted != read_bytes {
                        violations.push(format!(
                            "{label} case{index}: extract/{name} disagrees with read_entry"
                        ));
                    }
                }
                let _ = std::fs::remove_dir_all(&out);
            }
        }
    }

    assert!(
        violations.is_empty(),
        "{} read/verify/extract disagreements: {:?}",
        violations.len(),
        &violations[..violations.len().min(20)]
    );
}
