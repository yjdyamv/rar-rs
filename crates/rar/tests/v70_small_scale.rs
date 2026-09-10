//! RAR7 (v70) at small scale via the `compression(V70)` writer seam.
//!
//! WinRAR only writes v70 members when the dictionary exceeds 4 GiB (the
//! `-md8g` tests need a > 4 GiB source and stay `#[ignore]`d), so the v70
//! header paths and the DCX distance table had no default-suite coverage.
//! `compression(V70)` writes legal v70 headers (`comp_version` 1) with any
//! supported dictionary — the format does not require > 4 GiB — letting
//! these tests run the v70 archive I/O at small scale. WinRAR
//! compatibility at this scale is not part of the validated surface; the
//! seam is for our own round trips.
//!
//! Note on sizes: the declared dictionary is capped at twice the member
//! size (WinRAR's selection rule), so members here are >= 4 MiB to keep
//! the requested 8 MiB dictionary intact. Non-power-of-two byte counts
//! through 4 GiB (`6m`) are v70-only — the 5-bit base plus 1/32 increment
//! header encodes them exactly — and are exercised here too.

use rar_rs::ArchiveReader;

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

/// Deterministic pseudo-random bytes (LCG) — incompressible, so a
/// level-3 member keeps its size and actually splits across volumes.
fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
    let mut state = seed
        .wrapping_mul(0x2545_F491_4F6C_DD1D)
        .wrapping_add(0x9E37_79B9);
    (0..n)
        .map(|_| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8
        })
        .collect()
}

/// `compressible`-style data with a distant copy (second half = first
/// half) so long matches exercise the DCX distance coding.
fn distant_copy(seed: u8, half: usize) -> Vec<u8> {
    let mut data = compressible(seed, half);
    data.reserve(half);
    let first = data.clone();
    data.extend_from_slice(&first);
    data
}

/// v70 members: `comp_version` 1, exact `dict_size_bytes` round trip,
/// and byte-identical reads. Without `compression(V70)` the same small
/// dictionary must stay a plain v50 member.
#[test]
fn v70_forced_headers_and_roundtrip() {
    for dict in [4u64 * 1024 * 1024, 8 * 1024 * 1024] {
        let dir = make_temp_dir();
        let arc = dir.path().join("v70.rar");
        let a = compressible(11, 4 * 1024 * 1024);
        let b = distant_copy(12, 2 * 1024 * 1024);
        {
            let mut rar = rar_rs::ArchiveWriter::create_with(
                &arc,
                rar_rs::WriterOptions::default()
                    .dictionary_size(rar_rs::DictionarySize::try_from(dict).unwrap())
                    .compression(rar_rs::version::ArchiveVersion::V70),
            )
            .unwrap();
            let opts = rar_rs::EntryWriteOptions::new()
                .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
            rar.add_bytes("a.bin", &a, opts).unwrap();
            rar.add_bytes("b.bin", &b, opts).unwrap();
            rar.finish().unwrap();
        }
        let mut rar = ArchiveReader::open(&arc).unwrap();
        for (name, expected) in [("a.bin", &a), ("b.bin", &b)] {
            let id = rar.unique_entry(name).unwrap();
            let entry = rar.entry(id).unwrap();
            assert_eq!(entry.comp_version(), 1, "v70 header for {name}");
            assert_eq!(
                entry.dict_size_bytes(),
                Some(dict),
                "declared dictionary round trip for {name}"
            );
            assert_eq!(&rar.read_entry(id).unwrap(), expected, "bytes for {name}");
        }
    }

    // Same small dictionary without the seam: plain v50, no dict bytes.
    let dir = make_temp_dir();
    let arc = dir.path().join("v50.rar");
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &arc,
            rar_rs::WriterOptions::default()
                .dictionary_size(rar_rs::DictionarySize::try_from(8 * 1024 * 1024).unwrap()),
        )
        .unwrap();
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
        rar.add_bytes("a.bin", b"plain v50", opts).unwrap();
        rar.finish().unwrap();
    }
    let mut rar = ArchiveReader::open(&arc).unwrap();
    let id = rar.unique_entry("a.bin").unwrap();
    let entry = rar.entry(id).unwrap();
    assert_eq!(entry.comp_version(), 0, "still v50 without the seam");
    assert_eq!(entry.dict_size_bytes(), None, "no v70 dict declared");
    assert_eq!(rar.read_entry(id).unwrap(), b"plain v50");
}

/// A non-power-of-two dictionary through 4 GiB (`6m`) is a v70-only byte
/// size: the header's 5-bit base plus 1/32 increment encodes it exactly,
/// and the member round trips byte-identically. The same 6 MiB request
/// without the `compression(V70)` seam cannot be declared by a plain v50
/// log, so the writer rounds the RAR5 log up (6 MiB -> 8 MiB) and emits a
/// plain v50 member.
#[test]
fn v70_forced_non_power_of_two_dictionary() {
    let dir = make_temp_dir();
    let arc = dir.path().join("v70_6m.rar");
    let dict = 6 * 1024 * 1024u64;
    // Member size must clear the 2x-file-size cap (>= 3 MiB) so the
    // requested 6 MiB dictionary is declared in full.
    let a = compressible(51, 6 * 1024 * 1024);
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &arc,
            rar_rs::WriterOptions::default()
                .dictionary_size(rar_rs::DictionarySize::try_from(dict).unwrap())
                .compression(rar_rs::version::ArchiveVersion::V70),
        )
        .unwrap();
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
        rar.add_bytes("a.bin", &a, opts).unwrap();
        rar.finish().unwrap();
    }
    let mut rar = ArchiveReader::open(&arc).unwrap();
    let id = rar.unique_entry("a.bin").unwrap();
    let entry = rar.entry(id).unwrap();
    assert_eq!(entry.comp_version(), 1, "v70 header for 6 MiB dict");
    assert_eq!(
        entry.dict_size_bytes(),
        Some(dict),
        "non-power-of-two dictionary round trips exactly"
    );
    assert_eq!(&rar.read_entry(id).unwrap(), &a, "bytes");

    // Same request, no seam: still a legal v50 member (log rounds up).
    let dir = make_temp_dir();
    let arc = dir.path().join("v50_6m.rar");
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &arc,
            rar_rs::WriterOptions::default()
                .dictionary_size(rar_rs::DictionarySize::try_from(dict).unwrap()),
        )
        .unwrap();
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
        rar.add_bytes("a.bin", &a, opts).unwrap();
        rar.finish().unwrap();
    }
    let mut rar = ArchiveReader::open(&arc).unwrap();
    let id = rar.unique_entry("a.bin").unwrap();
    let entry = rar.entry(id).unwrap();
    assert_eq!(entry.comp_version(), 0, "plain v50 without the seam");
    assert_eq!(entry.dict_size_bytes(), None, "no v70 dict declared");
    assert_eq!(&rar.read_entry(id).unwrap(), &a, "bytes");
}

/// v70 + solid: the shared LZ window carries the DCX member state across
/// members; all members stay byte-identical. Every member is >= 4 MiB so
/// each declares the full 8 MiB dictionary (per-member 2x-file cap).
#[test]
fn v70_forced_solid_roundtrip() {
    let dir = make_temp_dir();
    let arc = dir.path().join("v70s.rar");
    let a = compressible(21, 4 * 1024 * 1024);
    let b = distant_copy(22, 2 * 1024 * 1024);
    let c = compressible(23, 4 * 1024 * 1024);
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &arc,
            rar_rs::WriterOptions::default()
                .solid_mode(rar_rs::SolidMode::Continuous)
                .dictionary_size(rar_rs::DictionarySize::try_from(8 * 1024 * 1024).unwrap())
                .compression(rar_rs::version::ArchiveVersion::V70),
        )
        .unwrap();
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
        rar.add_bytes("a.bin", &a, opts).unwrap();
        rar.add_bytes("b.bin", &b, opts).unwrap();
        rar.add_bytes("c.bin", &c, opts).unwrap();
        rar.finish().unwrap();
    }
    let mut rar = ArchiveReader::open(&arc).unwrap();
    assert_eq!(
        rar.entries()
            .map(|e| e.name().to_string())
            .collect::<Vec<String>>(),
        ["a.bin", "b.bin", "c.bin"]
    );
    for (name, expected) in [("a.bin", &a), ("b.bin", &b), ("c.bin", &c)] {
        let id = rar.unique_entry(name).unwrap();
        let entry = rar.entry(id).unwrap();
        assert_eq!(entry.comp_version(), 1, "v70 solid member {name}");
        assert_eq!(
            entry.dict_size_bytes(),
            Some(8 * 1024 * 1024),
            "solid member {name} dictionary"
        );
        assert_eq!(
            &rar.read_entry(id).unwrap(),
            expected,
            "solid bytes for {name}"
        );
    }
}

/// v70 + multi-volume: DCX members split across volume boundaries and
/// reassemble byte-identically from the first volume. The member mixes
/// compressible and incompressible halves: it passes the
/// incompressibility probe (so the v70 compressed path runs) while its
/// packed size still exceeds one 2 MiB volume.
#[test]
fn v70_forced_multivolume_roundtrip() {
    let dir = make_temp_dir();
    let arc = dir.path().join("v70m.rar");
    let mut a = compressible(31, 8 * 1024 * 1024);
    a.extend_from_slice(&pseudo_random(8 * 1024 * 1024, 32));
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &arc,
            rar_rs::WriterOptions::default()
                .volume_size(2 * 1024 * 1024)
                .dictionary_size(rar_rs::DictionarySize::try_from(8 * 1024 * 1024).unwrap())
                .compression(rar_rs::version::ArchiveVersion::V70),
        )
        .unwrap();
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
        rar.add_bytes("a.bin", &a, opts).unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&arc);
    assert!(volumes.len() >= 2, "precondition: multi-volume set");
    let mut rar = ArchiveReader::open(&volumes[0]).unwrap();
    let id = rar.unique_entry("a.bin").unwrap();
    let entry = rar.entry(id).unwrap();
    assert_eq!(entry.comp_version(), 1, "v70 multi-volume member");
    assert_eq!(&rar.read_entry(id).unwrap(), &a);
}

/// v70 + file-level encryption: the payload encryption path is
/// independent of the dictionary, but the combination must still round
/// trip (chained CBC over DCX blocks).
#[test]
fn v70_forced_encrypted_roundtrip() {
    let dir = make_temp_dir();
    let arc = dir.path().join("v70e.rar");
    let a = compressible(41, 4 * 1024 * 1024);
    {
        let mut rar = rar_rs::ArchiveWriter::create_with(
            &arc,
            rar_rs::WriterOptions::default()
                .password("s3cret")
                .dictionary_size(rar_rs::DictionarySize::try_from(8 * 1024 * 1024).unwrap())
                .compression(rar_rs::version::ArchiveVersion::V70),
        )
        .unwrap();
        let opts = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(3).unwrap());
        rar.add_bytes("a.bin", &a, opts).unwrap();
        rar.finish().unwrap();
    }
    let mut rar =
        ArchiveReader::open_with(&arc, rar_rs::OpenOptions::new().password("s3cret")).unwrap();
    let id = rar.unique_entry("a.bin").unwrap();
    let entry = rar.entry(id).unwrap();
    assert_eq!(entry.comp_version(), 1, "v70 encrypted member");
    assert_eq!(&rar.read_entry(id).unwrap(), &a);
}
