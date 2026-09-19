//! Streaming / large-member edge cases: disjoint x86 filter records, solid
//! dictionary growth, v70 at small scale, encrypted multi-volume edits and a
//! WinRAR-produced recovery record.

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

mod streamed_x86_regions {
    //! Streaming (>= 64 MiB) auto-x86 filter records must be disjoint.
    //!
    //! Regression: the streaming writer extended *every* sample-detected x86
    //! region to the end of the member, so multi-region samples produced
    //! overlapping (and, per window, duplicated) filter records — a shape the
    //! buffered writer's `validate_filter_specs` rejects and official WinRAR
    //! never writes. The fix collapses the sample regions into one span from
    //! the first detection to EOF.
    //!
    //! The fixture alternates 8 KiB x86 clusters with 48 KiB zero gaps: the
    //! leading 64 KiB sample sees two clusters separated by more than the
    //! span gap, so the detector returns two regions and the old code emitted
    //! the overlapping shape.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use std::io::{Read, Seek, SeekFrom};

    use rar_rs::{
        ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions,
    };

    const SIZE: usize = 64 * 1024 * 1024;

    /// Append x86-like code (NOP runs punctuated by CALL rel32) up to `end`.
    fn x86_cluster(out: &mut Vec<u8>, end: usize) {
        let mut pos = 0u32;
        while out.len() < end {
            out.extend_from_slice(&[0x90; 64]); // NOP
            pos += 64;
            out.push(0xe8); // CALL rel32
            out.extend_from_slice(&(pos.wrapping_mul(7) & 0x00FF_FFFF).to_le_bytes());
            pos += 5;
            out.extend_from_slice(&[0x41; 16]); // INC ECX
            pos += 16;
        }
        out.truncate(end);
    }

    fn multi_region_fixture() -> Vec<u8> {
        let mut data = Vec::with_capacity(SIZE);
        while data.len() < SIZE {
            let end = data.len() + 8 * 1024;
            x86_cluster(&mut data, end);
            data.resize(data.len() + 48 * 1024, 0);
        }
        data.truncate(SIZE);
        data
    }

    #[test]
    fn streamed_x86_filter_records_are_disjoint() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("multi.bin");
        let data = multi_region_fixture();
        std::fs::write(&src, &data).unwrap();

        let arc = dir.path().join("multi.rar");
        {
            // Pin the window size (2 threads -> 24 MiB) so the member spans
            // several windows regardless of the host.
            let mut ar = ArchiveWriter::create_with(
                &arc,
                WriterOptions::default()
                    .thread_count(rar_rs::ThreadCount::try_from(2usize).unwrap()),
            )
            .unwrap();
            ar.add_path(
                &src,
                EntryWriteOptions::new()
                    .compression_level(CompressionLevel::try_from(1u8).unwrap()),
            )
            .unwrap();
            ar.finish().unwrap();
        }

        let regions = {
            let reader = ArchiveReader::open(&arc).unwrap();
            let entry = reader.entries().next().unwrap();
            assert!(!entry.comp_solid(), "a filtered member must be standalone");
            let (offset, packed) = (entry.data_offset(), entry.compressed_size());
            assert!(packed > 0);
            let mut f = std::fs::File::open(&arc).unwrap();
            f.seek(SeekFrom::Start(offset)).unwrap();
            let mut buf = vec![0u8; packed as usize];
            f.read_exact(&mut buf).unwrap();
            let analysis = rar_rs::codec::lzss_huff::analyze_stream(
                &buf,
                entry.size(),
                entry.comp_dict_size(),
                rar_rs::ArchiveVersion::from_v70(entry.comp_version() == 1),
            )
            .unwrap();
            analysis.filter_regions
        };

        assert!(
            regions.len() > 1,
            "expected the multi-region sample to produce several records, got {regions:?}"
        );
        let mut sorted = regions.clone();
        sorted.sort_unstable();
        for pair in sorted.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            assert!(
                a.0 + a.1 <= b.0,
                "overlapping filter records {a:?} and {b:?}"
            );
        }
        assert!(sorted[0].0 < 1024, "span starts near the first detection");
        let last = sorted.last().unwrap();
        assert_eq!(
            last.0 + last.1,
            SIZE as u64,
            "span must run through end-of-member"
        );

        // Our own decoder round-trips the disjoint records byte-identically.
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        {
            let mut rar = ArchiveReader::open(&arc).unwrap();
            rar.extract_all_with_options(&out, rar_rs::ExtractOptions::default())
                .unwrap();
        }
        assert_eq!(std::fs::read(out.join("multi.bin")).unwrap(), data);
    }
}

mod solid_dict_growth {
    //! Regression tests for the solid-chain dictionary window.
    //!
    //! The writer clamps a later member whose own size selects a larger
    //! dictionary to the chain-start value, so self-produced archives always
    //! stay inside the shared window. The reader accepts continuation headers
    //! that declare a larger dictionary anyway (official archives do): it grows
    //! the shared window and carries the lookbehind tail forward instead of
    //! rejecting the member.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::{
        ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, SolidMode,
        WriterOptions, wire,
    };

    fn compressible(seed: u8, n: usize) -> Vec<u8> {
        let pat: Vec<u8> = (0..64u8)
            .map(|i| i.wrapping_mul(7).wrapping_add(seed))
            .collect();
        let mut out = Vec::with_capacity(n + pat.len());
        while out.len() < n {
            out.extend_from_slice(&pat);
        }
        out.truncate(n);
        out
    }

    fn write_solid_archive(path: &std::path::Path, small: &[u8], large: &[u8]) {
        let mut rar = ArchiveWriter::create_with(
            path,
            WriterOptions::default().solid_mode(SolidMode::Continuous),
        )
        .expect("create solid archive");
        let opts =
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap());
        rar.add_bytes("small.bin", small, opts)
            .expect("add small member");
        rar.add_bytes("large.bin", large, opts)
            .expect("add large member");
        rar.finish().expect("close solid archive");
    }

    /// The 64 KiB head selects a 128 KiB dictionary; the ~5 MiB second member
    /// would select 8 MiB and repeats the head's bytes 320 KiB back. The writer
    /// must clamp it to the chain window and the archive must round-trip.
    #[test]
    fn solid_chain_clamps_a_larger_member_dictionary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("solid-dict.rar");

        let small = compressible(7, 64 * 1024);
        let mut large = compressible(200, 256 * 1024);
        large.extend_from_slice(&small);
        large.extend_from_slice(&compressible(41, 5 * 1024 * 1024 - large.len()));

        write_solid_archive(&path, &small, &large);

        let mut reader = ArchiveReader::open(&path).expect("open archive");
        let small_id = reader.unique_entry("small.bin").expect("small entry");
        let large_id = reader.unique_entry("large.bin").expect("large entry");
        let small_entry = reader.entry(small_id).expect("small metadata");
        let large_entry = reader.entry(large_id).expect("large metadata");
        assert_eq!(
            small_entry.comp_dict_size(),
            0,
            "chain head declares the 128 KiB selection"
        );
        assert_eq!(
            large_entry.comp_dict_size(),
            0,
            "continuation member is clamped to the chain-start dictionary"
        );
        assert!(large_entry.comp_solid(), "large member continues the chain");

        assert_eq!(reader.read_entry(small_id).expect("read small"), small);
        assert_eq!(reader.read_entry(large_id).expect("read large"), large);
    }

    /// Offset of the comp-info vint inside a FILE_HEADER body (the same field
    /// walk `parse_stream_params` performs).
    fn comp_info_offset(body: &[u8]) -> usize {
        let mut off = 0usize;
        let next = |off: &mut usize| -> u64 {
            let (value, n) = wire::vint::decode_from_slice(body, *off).expect("vint");
            *off += n;
            value
        };
        let _ = next(&mut off); // block type
        let flags = next(&mut off);
        if flags & 0x01 != 0 {
            let _ = next(&mut off); // extra area size
        }
        if flags & 0x02 != 0 {
            let _ = next(&mut off); // data area size
        }
        let file_flags = next(&mut off);
        let _ = next(&mut off); // unpacked size
        let _ = next(&mut off); // attributes
        if file_flags & 0x0002 != 0 {
            off += 4; // FILE_FLAG_TIME_UNIX
        }
        if file_flags & 0x0004 != 0 {
            off += 4; // FILE_FLAG_CRC32
        }
        off
    }

    /// Set `name`'s dictionary log to `dict_log` **in place** in the stored
    /// header bytes and recompute the header CRC, mirroring how the review
    /// patched a WinRAR archive. In-place keeps the original serialization
    /// layout (official UnRAR still accepts the patched file), unlike a
    /// resynthesized header.
    fn patch_member_dict(archive: &[u8], name: &str, dict_log: u8) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(archive);
        cursor.set_position(8);
        loop {
            let meta = wire::read_block(&mut cursor, None)
                .expect("read block")
                .expect("block");
            if meta.block_type == 0x02 {
                let hdr = wire::FileHeader::from_raw(&meta.raw, meta.block_start)
                    .expect("parse member header");
                if hdr.name == name {
                    let body = &meta.raw.header_data;
                    let off = comp_info_offset(body);
                    let (comp_info, vint_len) =
                        wire::vint::decode_from_slice(body, off).expect("comp info vint");
                    let patched = (comp_info & !(0x0F << 10)) | (u64::from(dict_log) << 10);
                    let encoded = wire::vint::encode(patched);
                    assert_eq!(
                        encoded.len(),
                        vint_len,
                        "dictionary log must keep the vint width"
                    );
                    let body_start = meta.block_start as usize + 4 + meta.hsize_vint_len;
                    let content_start = meta.block_start as usize + 4;
                    let content_end = meta.data_offset as usize;
                    let mut out = archive.to_vec();
                    out[body_start + off..body_start + off + vint_len].copy_from_slice(&encoded);
                    let crc = crc32fast::hash(&out[content_start..content_end]);
                    out[meta.block_start as usize..content_start]
                        .copy_from_slice(&crc.to_le_bytes());
                    return out;
                }
            }
            cursor.set_position(meta.data_end);
            if meta.block_type == 0x05 {
                panic!("member {name} not found");
            }
        }
    }

    /// A solid-chain continuation header declaring a dictionary larger than the
    /// chain head's (official archives do this; a reviewer reproduced it with a
    /// WinRAR solid archive) must decode: the reader grows the shared window and
    /// carries the lookbehind tail forward instead of rejecting the archive.
    /// The patched header keeps the packed stream the writer produced against
    /// the chain window, so every distance resolves inside the preserved tail.
    #[test]
    fn read_decodes_a_solid_member_with_a_larger_dictionary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("solid-patched.rar");

        let small = compressible(7, 64 * 1024);
        let large = compressible(9, 512 * 1024);
        write_solid_archive(&path, &small, &large);

        let bytes = std::fs::read(&path).expect("read archive");
        let patched = patch_member_dict(&bytes, "large.bin", 6); // 8 MiB
        let patched_path = dir.path().join("solid-patched-header.rar");
        std::fs::write(&patched_path, &patched).expect("write patched archive");

        let mut reader = ArchiveReader::open(&patched_path).expect("open patched archive");
        let id = reader.unique_entry("large.bin").expect("large entry");
        assert_eq!(
            reader.entry(id).expect("large metadata").comp_dict_size(),
            6,
            "the patched continuation declaration must be visible"
        );
        assert_eq!(
            reader
                .read_entry(id)
                .expect("growing dictionary must decode"),
            large,
            "the continuation must decode against the grown window"
        );
        let small_id = reader.unique_entry("small.bin").expect("small entry");
        assert_eq!(
            reader.read_entry(small_id).expect("chain head must decode"),
            small,
            "the chain head must still decode after the window grew"
        );
    }

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

    #[test]
    fn per_extension_reset_group_starts_a_new_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("se.rar");
        let a = compressible(1, 64 * 1024);
        let a2 = compressible(2, 64 * 1024);
        // Reset group head: 256 KiB of pattern, a 256 KiB random block and its
        // exact copy (a 256 KiB-back match, beyond the first group's 128 KiB
        // window), then filler to select an 8 MiB dictionary.
        let mut b = compressible(3, 256 * 1024);
        let rnd = pseudo_random(256 * 1024, 99);
        b.extend_from_slice(&rnd);
        b.extend_from_slice(&rnd);
        b.extend_from_slice(&compressible(30, 4 * 1024 * 1024));
        let b2 = compressible(4, 2 * 1024 * 1024);
        {
            let mut rar = ArchiveWriter::create_with(
                &path,
                WriterOptions::default().solid_mode(SolidMode::PerExtension),
            )
            .unwrap();
            let opts = EntryWriteOptions::new()
                .compression_level(CompressionLevel::try_from(3u8).unwrap());
            rar.add_bytes("a.txt", &a, opts).unwrap();
            rar.add_bytes("a2.txt", &a2, opts).unwrap();
            rar.add_bytes("b.bin", &b, opts).unwrap();
            rar.add_bytes("b2.bin", &b2, opts).unwrap();
            rar.finish().unwrap();
        }
        let mut reader = ArchiveReader::open(&path).unwrap();
        for (name, expected) in [
            ("a.txt", &a),
            ("a2.txt", &a2),
            ("b.bin", &b),
            ("b2.bin", &b2),
        ] {
            let id = reader.unique_entry(name).unwrap();
            assert_eq!(&reader.read_entry(id).unwrap(), expected, "{name}");
        }
    }
}

mod v70_small_scale {
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

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::ArchiveReader;

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
}

mod multivolume_edit_encrypted {
    //! Multi-volume RAR5 edit regressions: encrypted payloads must stay
    //! encrypted through a delete, a rewrite that grows the staged set must
    //! install every volume, and `.rev` recovery volumes must be regenerated
    //! for zero-padded sets.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::{
        ArchiveEditor, ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions,
        OpenOptions, WriterOptions,
    };

    fn stored() -> EntryWriteOptions {
        EntryWriteOptions::new().compression_level(CompressionLevel::STORE)
    }

    fn patterned(len: usize, modulo: usize) -> Vec<u8> {
        (0..len).map(|index| (index % modulo) as u8).collect()
    }

    fn staging_leftovers(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| {
                name.contains("rar5tmp") || name.contains("rar5bak") || name.contains("rar5commit")
            })
            .collect()
    }

    fn rev_names(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".rev"))
            .collect()
    }

    #[test]
    fn delete_from_encrypted_multivolume_set_keeps_survivor_decryptable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enc-set.rar");
        let a = patterned(70_000, 251);
        let b = patterned(70_000, 253);
        {
            let mut writer = ArchiveWriter::create_with(
                &path,
                WriterOptions::new().volume_size(60_000).password("secret"),
            )
            .unwrap();
            writer.add_bytes("a.bin", &a, stored()).unwrap();
            writer.add_bytes("b.bin", &b, stored()).unwrap();
            writer.finish().unwrap();
        }
        let before = rar_rs::discover_volumes(&path);
        assert!(before.len() >= 2, "precondition: multi-volume set");

        // Delete from a later volume, like the reported CLI repro.
        let mut editor =
            ArchiveEditor::open_with_password(before.last().unwrap().as_path(), "secret").unwrap();
        let b_id = editor.unique_entry("b.bin").unwrap();
        editor.delete_entries(&[b_id]).unwrap();
        drop(editor);

        // The surviving member must still verify with the password: it used to
        // be written as plaintext under its ENCR header, failing CRC.
        let volumes = rar_rs::discover_volumes(&path);
        let mut reader =
            ArchiveReader::open_with(&volumes[0], OpenOptions::new().password("secret")).unwrap();
        let a_id = reader.unique_entry("a.bin").unwrap();
        assert_eq!(reader.read_entry(a_id).unwrap(), a);
        assert!(reader.verify().unwrap().is_ok(), "survivor must verify");
        assert!(staging_leftovers(dir.path()).is_empty());
    }

    #[test]
    fn rename_growing_the_staged_set_installs_every_volume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grow.rar");
        let payload = patterned(102_100, 257);
        {
            let mut writer =
                ArchiveWriter::create_with(&path, WriterOptions::new().volume_size(50 * 1024))
                    .unwrap();
            writer.add_bytes("a.bin", &payload, stored()).unwrap();
            writer.finish().unwrap();
        }
        let before = rar_rs::discover_volumes(&path);
        assert_eq!(before.len(), 2, "precondition: two-volume set");

        // A 254-character name pushes the split past the old volume count; the
        // rewrite used to install only the old number of parts and leak the rest.
        let long = "x".repeat(254);
        let mut editor = ArchiveEditor::open(&before[0]).unwrap();
        let id = editor.unique_entry("a.bin").unwrap();
        assert_eq!(editor.rename_entries(&[(id, long.clone())]).unwrap(), 1);
        drop(editor);

        let after = rar_rs::discover_volumes(&path);
        assert_eq!(
            after.len(),
            3,
            "the grown staged set must be fully installed: {after:?}"
        );
        assert!(
            staging_leftovers(dir.path()).is_empty(),
            "staging leaked: {:?}",
            staging_leftovers(dir.path())
        );
        let mut reader = ArchiveReader::open(&after[0]).unwrap();
        let id = reader.unique_entry(&long).unwrap();
        assert_eq!(reader.read_entry(id).unwrap(), payload);
        assert!(reader.verify().unwrap().is_ok());
    }

    #[test]
    fn delete_on_padded_set_regenerates_padded_recovery_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("padded.rar");
        {
            let mut writer = ArchiveWriter::create_with(
                &path,
                WriterOptions::new()
                    .volume_size(30_000)
                    .recovery_volumes_percent(50),
            )
            .unwrap();
            for index in 0..12 {
                let member = patterned(28_000, 251 + index);
                writer
                    .add_bytes(&format!("m{index:02}.bin"), &member, stored())
                    .unwrap();
            }
            writer.finish().unwrap();
        }
        let before = rar_rs::discover_volumes(&path);
        assert!(before.len() >= 10, "precondition: padded volume set");
        assert!(
            !rev_names(dir.path()).is_empty(),
            "precondition: .rev files present"
        );

        let mut editor = ArchiveEditor::open(&before[0]).unwrap();
        let victim = editor.unique_entry("m05.bin").unwrap();
        editor.delete_entries(&[victim]).unwrap();
        drop(editor);

        let after = rar_rs::discover_volumes(&path);
        assert!(after.len() >= 10, "set should stay padded: {after:?}");
        let regenerated = rev_names(dir.path());
        assert!(
            !regenerated.is_empty(),
            "recovery volumes must be regenerated: {regenerated:?}"
        );
        assert!(
            regenerated.iter().any(|name| name == "padded.part01.rev"),
            "expected padded recovery names: {regenerated:?}"
        );
        assert!(staging_leftovers(dir.path()).is_empty());

        let mut reader = ArchiveReader::open(&after[0]).unwrap();
        assert!(reader.verify().unwrap().is_ok());
        assert!(reader.unique_entry("m05.bin").is_err());
        let m06 = reader.unique_entry("m06.bin").unwrap();
        assert_eq!(reader.read_entry(m06).unwrap(), patterned(28_000, 251 + 6));
    }
}

mod rar50_recovery_external {
    //! External cross-validation against a real WinRAR-produced RAR5 archive that
    //! carries a genuine data-recovery record.
    //!
    //! The fixture `fixtures/rar50/winrar5_with_recovery_rr5.rar` was created with
    //! `WinRAR RAR 7.23` (`Rar.exe a -rr5% -ma5 -s- -m5 -ep doc.txt data.bin`), so
    //! it exercises the *real* RAR5 inline-recovery-record format rather than our
    //! own writer. Two things must hold for the cross-validation to pass:
    //!
    //! 1. Our reader decodes WinRAR's members (proving our RAR5 parser handles the
    //!    vendor's output, including locating the `RR` service header).
    //! 2. Our recovery/repair engine reconstructs the WinRAR-produced parity when
    //!    the archive is damaged, returning byte-identical original bytes.

    #[allow(unused_imports)] // the merged modules share the parent's support import
    use super::*;

    use rar_rs::{ArchiveReader, repair_archive};
    use std::path::PathBuf;

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rar50/winrar5_with_recovery_rr5.rar")
    }

    #[test]
    fn winrar_rar5_recovery_record_parses_and_decodes() {
        // Guard: the fixture really carries an inline recovery record.
        let raw = include_bytes!("fixtures/rar50/winrar5_with_recovery_rr5.rar");
        let text = String::from_utf8_lossy(raw);
        assert!(
            text.contains("RR"),
            "fixture must contain an RR service header"
        );
        assert!(
            text.contains("{RB}"),
            "fixture must contain an {{RB}} recovery chunk"
        );

        // Our reader must decode WinRAR's members correctly.
        let mut reader = ArchiveReader::open(fixture_path()).expect("open WinRAR RAR5 archive");
        assert_eq!(reader.entries().len(), 2, "expected doc.txt + data.bin");

        let doc_id = reader.unique_entry("doc.txt").expect("doc.txt entry");
        let doc = reader.read_entry(doc_id).expect("read doc.txt");
        assert_eq!(doc.len(), 304000, "doc.txt size must match source");
        let prefix =
            b"RAR5 recovery record cross-validation fixture generated by WinRAR RAR 7.23. ";
        assert_eq!(
            &doc[..prefix.len()],
            prefix,
            "doc.txt content must match WinRAR source"
        );

        let data_id = reader.unique_entry("data.bin").expect("data.bin entry");
        let data = reader.read_entry(data_id).expect("read data.bin");
        assert_eq!(data.len(), 65536, "data.bin size must match source");
    }

    #[test]
    fn winrar_rar5_recovery_record_repairs_damaged_archive() {
        let raw = include_bytes!("fixtures/rar50/winrar5_with_recovery_rr5.rar");
        // Guard: the fixture really carries an inline recovery record.
        let text = String::from_utf8_lossy(raw);
        assert!(
            text.contains("RR") && text.contains("{RB}"),
            "fixture must carry an RR service header and {{RB}} recovery chunk"
        );

        // Damage a single byte well inside the protected prefix (never inside the
        // recovery record itself, which lives at the very end of the archive).
        let mut damaged = raw.to_vec();
        let pos = raw.len() / 4;
        damaged[pos] ^= 0xFF;

        // Our repair engine must reconstruct the WinRAR-produced parity and return
        // the original bytes unchanged.
        let repaired = repair_archive(&damaged).expect("repair WinRAR recovery record");
        assert_eq!(
            repaired, raw,
            "repair must restore the original WinRAR-produced bytes"
        );
    }
}
