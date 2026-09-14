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

use std::io::{Read, Seek, SeekFrom};

use rar_rs::{ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

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
            WriterOptions::default().thread_count(rar_rs::ThreadCount::try_from(2usize).unwrap()),
        )
        .unwrap();
        ar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(1u8).unwrap()),
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
