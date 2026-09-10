//! Large-path regression tests at a scale that runs in the default suite.
//!
//! The extreme-scale tiers (> 4 GiB files, RAR7 v70 > 4 GiB dictionaries,
//! 128 MiB long-range ratio vs WinRAR) are irreducible — they exist only
//! to hit boundaries that smaller data cannot — and stay `#[ignore]`d in
//! `crates/rar-cli/tests/winrar_interop.rs`. These tests exercise the
//! same code paths (spill pipeline, chunked multi-volume writes, chained
//! CBC, long-range matching) at sizes that complete in seconds, so the
//! default suite still catches regressions in them.

use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, DictionarySize, EntryWriteOptions, OpenOptions,
    WriterOptions,
};
use std::path::Path;

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

fn sha256(path: &Path) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let mut f = std::fs::File::open(path).expect("open");
    let mut buf = vec![0u8; 1 << 20];
    loop {
        use std::io::Read;
        let n = f.read(&mut buf).expect("read");
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Write `len` bytes of a 2-channel 16-bit lane walk: each lane nudges by a
/// small random step per frame, so the inter-lane deltas stay tiny — exactly
/// the data profile the automatic delta filter targets.
fn write_lane_walk(path: &Path, len: usize) {
    let mut f = std::fs::File::create(path).expect("create lane-walk file");
    let mut rng = 42u64;
    let mut acc = [0i64; 2];
    let cap = 1 << 19;
    let mut frames = Vec::with_capacity(cap);
    let mut total = 0usize;
    while total < len {
        frames.clear();
        for _ in 0..cap {
            let mut pair = [0u8; 4];
            for ch in 0..2 {
                rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                acc[ch] += (rng >> 33) as i64 % 13 - 6;
                acc[ch] &= 0xFFFF;
                pair[ch * 2] = (acc[ch] & 0xFF) as u8;
                pair[ch * 2 + 1] = ((acc[ch] >> 8) & 0xFF) as u8;
            }
            frames.extend_from_slice(&pair);
        }
        if total + frames.len() > len {
            frames.truncate(len - total);
        }
        use std::io::Write;
        f.write_all(&frames).expect("write lane-walk file");
        total += frames.len();
    }
}

/// Create a sparse file of `size` bytes (reads as zeros, allocates almost
/// nothing on disk).
fn create_sparse(path: &Path, size: u64) {
    let f = std::fs::File::create(path).expect("create sparse file");
    f.set_len(size).expect("extend sparse file");
}

/// 256 MiB sparse single-file streaming compression round trip: the
/// all-zero input must stream through the spill pipeline (memory stays
/// bounded) and compress hard, and streamed extraction must reproduce it
/// byte-for-byte. The reduced-scale sibling of the ignored > 4 GiB test.
#[test]
fn large_sparse_streamed_compression_roundtrips() {
    let dir = make_temp_dir();
    let size = 256 * 1024 * 1024u64; // 256 MiB
    let src = dir.path().join("big.bin");
    create_sparse(&src, size);

    let arc = dir.path().join("big.rar");
    {
        let mut rar = ArchiveWriter::create(&arc).unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    assert!(
        std::fs::metadata(&arc).unwrap().len() < 64 * 1024 * 1024,
        "all-zero input must compress well"
    );

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    {
        let mut rar = ArchiveReader::open(&arc).unwrap();
        let id = rar.unique_entry("big.bin").unwrap();
        rar.extract_entry_with_options(
            id,
            &out,
            rar_rs::ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(std::fs::metadata(out.join("big.bin")).unwrap().len(), size);
    assert_eq!(sha256(&out.join("big.bin")), sha256(&src));
}

/// 192 MiB stored + encrypted multi-volume round trip: chained CBC across
/// exact-sized volumes (per-chunk ciphertext CRCs, per-chunk encryption
/// records), byte-exact volume sizes, and a streamed extraction round
/// trip. The reduced-scale sibling of the ignored > 4 GiB encrypted
/// multi-volume test.
#[test]
fn large_streamed_encrypted_multivolume_roundtrips() {
    let dir = make_temp_dir();
    let src = dir.path().join("big.bin");
    write_repeated(&src, 0x5A, 192 * 1024 * 1024); // stored: fills volumes

    let vol_size = 32 * 1024 * 1024u64;
    let arc = dir.path().join("big.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &arc,
            WriterOptions::default()
                .password("s3cret")
                .volume_size(vol_size),
        )
        .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(0u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&arc);
    assert!(
        volumes.len() >= 6,
        "expected several exact volumes, got {}",
        volumes.len()
    );
    for vol in &volumes[..volumes.len() - 1] {
        assert_eq!(
            std::fs::metadata(vol).unwrap().len(),
            vol_size,
            "non-final volume must be byte-exact"
        );
    }

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    {
        let mut rar =
            ArchiveReader::open_with(&volumes[0], OpenOptions::new().password("s3cret")).unwrap();
        let id = rar.unique_entry("big.bin").unwrap();
        rar.extract_entry_with_options(
            id,
            &out,
            rar_rs::ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(
        std::fs::metadata(out.join("big.bin")).unwrap().len(),
        192 * 1024 * 1024u64
    );
    assert_eq!(sha256(&out.join("big.bin")), sha256(&src));
}

/// Streaming (>= 64 MiB) delta filter in a solid archive: the 64 MiB
/// lane-walk member must cross the spill threshold, win the automatic delta
/// filter (decided on the leading 64 KiB sample), be forward-transformed per
/// window in independent regions (each capped at `MAX_FILTER_BLOCK_LENGTH`,
/// aligned on absolute member coordinates, serialized relative to each
/// window's start), and break the solid chain around itself (its window
/// holds transformed bytes, which must never seed `b.txt`). The small
/// members before and after pin that the chain reseeded cleanly; streamed
/// extraction must reproduce every member byte-for-byte. `threads: 2` pins
/// the window size (24 MiB) deterministically so the file spans several
/// windows regardless of the host. `-m1` keeps the heavy case fast — the
/// filter path is level-independent.
#[test]
fn large_streamed_delta_filter_roundtrips() {
    let dir = make_temp_dir();
    let size = 64 * 1024 * 1024usize; // the streaming threshold
    let a = dir.path().join("a.txt");
    std::fs::write(&a, b"solid member before the filtered one\nabcdef\n").unwrap();
    let src = dir.path().join("pcm.bin");
    write_lane_walk(&src, size);
    let b = dir.path().join("b.txt");
    std::fs::write(&b, b"solid member after the filtered one\n0123456789\n").unwrap();

    let arc = dir.path().join("solid.rar");
    {
        let opts =
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(1u8).unwrap());
        let mut rar = ArchiveWriter::create_with(
            &arc,
            WriterOptions::default()
                .solid_mode(rar_rs::SolidMode::Continuous)
                .thread_count(rar_rs::ThreadCount::try_from(2usize).unwrap()),
        )
        .unwrap();
        rar.add_path(&a, opts).unwrap();
        rar.add_path(&src, opts).unwrap();
        rar.add_path(&b, opts).unwrap();
        rar.finish().unwrap();
    }
    // The lane-walk member must compress hard (delta makes the lanes
    // near-zero slopes), proving the filtered streaming path was taken.
    let total = rar_rs::discover_volumes(&arc)
        .iter()
        .map(|v| std::fs::metadata(v).unwrap().len())
        .sum::<u64>();
    assert!(
        total < size as u64 / 2,
        "lane-walk delta data must compress hard, archive {total}"
    );

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    {
        let mut rar = ArchiveReader::open(&arc).unwrap();
        rar.extract_all_with_options(&out, rar_rs::ExtractOptions::default())
            .unwrap();
    }
    assert_eq!(
        std::fs::read(out.join("a.txt")).unwrap(),
        std::fs::read(&a).unwrap()
    );
    assert_eq!(sha256(&out.join("pcm.bin")), sha256(&src));
    assert_eq!(
        std::fs::read(out.join("b.txt")).unwrap(),
        std::fs::read(&b).unwrap()
    );
}

/// Streaming (>= 64 MiB) x86 (E8/E8E9) filter in a solid archive: the 64
/// MiB x86-like member must cross the spill threshold, win the automatic
/// x86 filter (detected on the leading 64 KiB sample, regions clipped per
/// window on absolute member coordinates, serialized relative to each
/// window's start, E8/E8E9 variant picked by sample packed size), and break
/// the solid chain around itself. The small members before and after pin
/// that the chain reseeded cleanly; streamed extraction must reproduce
/// every member byte-for-byte. `threads: 2` pins the window size (24 MiB)
/// deterministically so the file spans several windows. `-m1` keeps the
/// heavy case fast — the filter path is level-independent.
#[test]
fn large_streamed_x86_filter_roundtrips() {
    let dir = make_temp_dir();
    let size = 64 * 1024 * 1024usize; // the streaming threshold
    let a = dir.path().join("a.txt");
    std::fs::write(&a, b"solid member before the x86-filtered one\nabcdef\n").unwrap();
    let src = dir.path().join("x86.bin");
    let data = x86_like(size);
    std::fs::write(&src, &data).unwrap();
    let b = dir.path().join("b.txt");
    std::fs::write(&b, b"solid member after the x86-filtered one\n0123456789\n").unwrap();

    let arc = dir.path().join("solid_x86.rar");
    {
        let opts =
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(1u8).unwrap());
        let mut rar = ArchiveWriter::create_with(
            &arc,
            WriterOptions::default()
                .solid_mode(rar_rs::SolidMode::Continuous)
                .thread_count(rar_rs::ThreadCount::try_from(2usize).unwrap()),
        )
        .unwrap();
        rar.add_path(&a, opts).unwrap();
        rar.add_path(&src, opts).unwrap();
        rar.add_path(&b, opts).unwrap();
        rar.finish().unwrap();
    }
    // The x86 member must compress hard (the E8 rel32 targets are
    // 16 MB-normalised by the filter, making the high bytes near-zero),
    // proving the filtered streaming path was taken.
    let total = rar_rs::discover_volumes(&arc)
        .iter()
        .map(|v| std::fs::metadata(v).unwrap().len())
        .sum::<u64>();
    assert!(
        total < size as u64 / 2,
        "x86 data must compress hard, archive {total}"
    );

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    {
        let mut rar = ArchiveReader::open(&arc).unwrap();
        rar.extract_all_with_options(&out, rar_rs::ExtractOptions::default())
            .unwrap();
    }
    assert_eq!(
        std::fs::read(out.join("a.txt")).unwrap(),
        std::fs::read(&a).unwrap()
    );
    assert_eq!(sha256(&out.join("x86.bin")), sha256(&src));
    assert_eq!(
        std::fs::read(out.join("b.txt")).unwrap(),
        std::fs::read(&b).unwrap()
    );
}

/// Streaming (>= 64 MiB) combined delta + x86 filters: a member that is
/// both multi-channel delta-correlated and has a dense x86 opcode region.
/// Both filters must compose (delta first, then x86, per window on member
/// coordinates) and the roundtrip must be byte-identical.
#[test]
fn large_streamed_delta_x86_combined_roundtrips() {
    let dir = make_temp_dir();
    let size = 64 * 1024 * 1024usize;
    let a = dir.path().join("a.txt");
    std::fs::write(&a, b"before\n").unwrap();
    let src = dir.path().join("mix.bin");
    // Delta lanes for the first third, x86-like code for the rest.
    let mut data = vec![0u8; size];
    {
        let mut rng = 7u64;
        let mut acc = [0i64; 2];
        let third = size / 3;
        for chunk in data[..third].as_chunks_mut::<4>().0 {
            for ch in 0..2 {
                rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                acc[ch] += (rng >> 33) as i64 % 11 - 5;
                acc[ch] &= 0xFFFF;
                chunk[ch * 2] = (acc[ch] & 0xFF) as u8;
                chunk[ch * 2 + 1] = ((acc[ch] >> 8) & 0xFF) as u8;
            }
        }
        let x86_data = x86_like(size - third);
        data[third..].copy_from_slice(&x86_data[..]);
    }
    std::fs::write(&src, &data).unwrap();
    let b = dir.path().join("b.txt");
    std::fs::write(&b, b"after\n").unwrap();

    let arc = dir.path().join("mix.rar");
    {
        let opts =
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(1u8).unwrap());
        let mut rar = ArchiveWriter::create_with(
            &arc,
            WriterOptions::default()
                .solid_mode(rar_rs::SolidMode::Continuous)
                .thread_count(rar_rs::ThreadCount::try_from(2usize).unwrap()),
        )
        .unwrap();
        rar.add_path(&a, opts).unwrap();
        rar.add_path(&src, opts).unwrap();
        rar.add_path(&b, opts).unwrap();
        rar.finish().unwrap();
    }
    let total = rar_rs::discover_volumes(&arc)
        .iter()
        .map(|v| std::fs::metadata(v).unwrap().len())
        .sum::<u64>();
    assert!(
        total < size as u64 * 3 / 4,
        "combined delta+x86 data must compress, archive {total}"
    );

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    {
        let mut rar = ArchiveReader::open(&arc).unwrap();
        rar.extract_all_with_options(&out, rar_rs::ExtractOptions::default())
            .unwrap();
    }
    assert_eq!(sha256(&out.join("mix.bin")), sha256(&src));
    assert_eq!(
        std::fs::read(out.join("a.txt")).unwrap(),
        std::fs::read(&a).unwrap()
    );
    assert_eq!(
        std::fs::read(out.join("b.txt")).unwrap(),
        std::fs::read(&b).unwrap()
    );
}

/// x86-like code: NOP runs (0x90) punctuated by CALL rel32 (0xE8) with
/// small near-zero offsets — the profile the E8/E8E9 filter targets.
fn x86_like(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    let mut pos = 0u32;
    while out.len() < size {
        out.extend_from_slice(&[0x90; 64]); // NOP
        pos += 64;
        out.push(0xe8); // CALL rel32
        out.extend_from_slice(&(pos.wrapping_mul(7) & 0x00FF_FFFF).to_le_bytes());
        pos += 5;
        out.extend_from_slice(&[0x41; 16]); // INC ECX
        pos += 16;
    }
    out.truncate(size);
    out
}

/// Long-range matching (`-mcl` semantics) at reduced scale: a 32 MiB file whose second half copies its random first half at exactly 16 MiB
/// distance. The near finder only sees ~12 MiB of context (8 MiB tail +
/// 4 MiB chunk) and the 32 MiB dictionary window bounds representable
/// distances, so the sampled long-range history must supply the match:
/// the archive must compress the copy away and decode byte-identically.
/// The ratio-vs-WinRAR gate stays in the ignored 128 MiB test; this
/// locks correctness into the default suite.
#[test]
fn long_range_matches_roundtrip_at_scale() {
    let dir = make_temp_dir();
    let src = dir.path().join("pair.bin");
    let half = 16 * 1024 * 1024usize;
    let mut data = vec![0u8; half * 2];
    {
        let mut state = 42u64;
        for b in data[..half].iter_mut() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (state >> 33) as u8;
        }
        let first = data[..half].to_vec();
        data[half..].copy_from_slice(&first);
    }
    std::fs::write(&src, &data).unwrap();

    let arc = dir.path().join("pair.rar");
    {
        let mut rar = ArchiveWriter::create_with(
            &arc,
            WriterOptions::default().dictionary_size(DictionarySize::from_rar5_log(8).unwrap()),
        )
        .unwrap();
        rar.add_path(
            &src,
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap()),
        )
        .unwrap();
        rar.finish().unwrap();
    }
    // The distant copy must compress away: the random first half alone
    // already costs ~16 MiB, so a packed size barely above the first
    // half's cost proves the copy half collapsed (raw it would be ~32
    // MiB).
    let packed = std::fs::metadata(&arc).unwrap().len();
    assert!(
        packed < half as u64 * 11 / 10,
        "long-range match did not fire: packed {packed} >= 1.1x half {half}"
    );

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    {
        let mut rar = ArchiveReader::open(&arc).unwrap();
        let id = rar.unique_entry("pair.bin").unwrap();
        rar.extract_entry_with_options(
            id,
            &out,
            rar_rs::ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(
        std::fs::metadata(out.join("pair.bin")).unwrap().len(),
        half as u64 * 2
    );
    assert_eq!(sha256(&out.join("pair.bin")), sha256(&src));
}
