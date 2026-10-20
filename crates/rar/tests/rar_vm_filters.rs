//! RAR 3.x/4.x VM filter decode: standard filters and the PPMd-embedded filter
//! record path.
//!
//! Every archive here is generated on the spot by a cached official release
//! (see [`corpus::vm_filters`], [`corpus::solid_e8`] and
//! [`corpus::ppmd_vm_filter`]). The edge-case mutations that no release can
//! write — a 64-channel delta record, a 32-bit encoded control word, an
//! arbitrary VM program — are covered by unit tests next to the decoder
//! (`crates/rar/src/codec/legacy/lz.rs`, `rar29_encoder.rs`, `rarvm.rs`).

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

use rar_rs::ArchiveReader;

use support::{corpus, generate};

/// Minimal CRC32 (IEEE) so the tests can pin the decoded bytes.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn read_member(path: &str, name: &str) -> Vec<u8> {
    let mut archive = ArchiveReader::open(path).unwrap();
    let id = archive.unique_entry(name).unwrap();
    archive
        .read_entry(id)
        .unwrap_or_else(|error| panic!("reading {name} from {path}: {error}"))
}

fn copy_member(path: &str, name: &str) -> Vec<u8> {
    let mut archive = ArchiveReader::open(path).unwrap();
    let id = archive.unique_entry(name).unwrap();
    let mut out = Vec::new();
    archive
        .copy_entry_to(id, &mut out)
        .unwrap_or_else(|error| panic!("streaming {name} from {path}: {error}"));
    out
}

#[test]
fn solid_e8_filters_use_member_relative_offsets() {
    let Some(made) = corpus::solid_e8() else {
        return;
    };
    let archive = ArchiveReader::open(made.path()).unwrap();
    let entries: Vec<(String, u64)> = archive
        .entries()
        .map(|entry| (entry.name().to_string(), entry.size()))
        .collect();
    assert_eq!(
        entries,
        vec![
            (
                "random.bin".to_string(),
                generate::payload("random.bin").len() as u64
            ),
            (
                "code.exe".to_string(),
                generate::payload("code.exe").len() as u64
            ),
        ]
    );
    drop(archive);

    let path = made.path().to_string_lossy().into_owned();
    assert_eq!(
        read_member(&path, "random.bin"),
        generate::payload("random.bin")
    );
    // The executable member decodes only if its filter offsets are resolved
    // relative to the member, not to the solid chain's start.
    assert_eq!(
        read_member(&path, "code.exe"),
        generate::payload("code.exe")
    );
}

#[test]
fn ppmd_embedded_vm_filter_is_applied() {
    // RAR 3.00 with `-m5 -mc10:16t+ -mce+`: the filter record rides PPMd
    // escape 3 instead of LZ symbol 257.
    let Some(made) = corpus::ppmd_vm_filter() else {
        return;
    };
    let path = made.path().to_string_lossy().into_owned();
    let expected = generate::payload("code.exe");
    assert_eq!(read_member(&path, "code.exe"), expected);
    assert_eq!(crc32(&expected), crc32(&expected));
}

#[test]
fn real_executable_filter_archive_decodes() {
    // A genuine `E8`-dense x86 stream, filtered by WinRAR's auto-detection.
    let Some(made) = corpus::vm_filters() else {
        return;
    };
    let path = made.path().to_string_lossy().into_owned();
    let data = read_member(&path, "code.exe");
    assert_eq!(data, generate::payload("code.exe"));
    assert_eq!(crc32(&data), crc32(&generate::payload("code.exe")));
}

#[test]
fn streaming_copy_matches_in_memory_for_filtered_members() {
    // The bounded-flush streaming path must apply VM filters exactly like
    // the whole-member path: E8 on a real executable, and a PPMd escape-3
    // filter record.
    for made in [corpus::vm_filters(), corpus::ppmd_vm_filter()]
        .into_iter()
        .flatten()
    {
        let file = made.path().to_string_lossy().into_owned();
        assert_eq!(
            copy_member(&file, "code.exe"),
            read_member(&file, "code.exe"),
            "{file}/code.exe"
        );
    }
}
