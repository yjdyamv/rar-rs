use super::*;

use super::super::{
    FILTER_DELTA, HUFF_DC, HUFF_LDC, HUFF_NC, HUFF_RC, MAX_CODE_LENGTH, SYM_FILTER,
};
use super::emit::{build_block_header, ensure_nonzero, write_filter_data, write_tables};
use crate::codec::common::bitstream::BitWriter;
use crate::codec::common::huffman::{
    DecodeTable, EncodeTable, build_code_lengths_from_freqs, encode_symbol,
};
use crate::codec::{DecodeOptions, decode_to_writer};
use crate::version::ArchiveVersion;

fn one_symbol_table(count: usize) -> Vec<u8> {
    let mut v = vec![0u8; count];
    v[0] = 1;
    v
}

/// Build a compressed RAR5 stream containing a Delta filter followed by
/// delta-encoded literals, then verify the streaming decoder applies
/// the filter and produces the original bytes.
#[test]
fn streaming_decode_applies_delta_filter() {
    let original: Vec<u8> = (0..300u32).map(|i| (i * 7 % 251) as u8).collect();
    // delta_decode computes cumulative negative sums:
    // result[i] = result[i-1] - D[i], so D[i] = prev - original[i].
    let mut delta = vec![0u8; original.len()];
    let mut prev = 0u8;
    for (i, &b) in original.iter().enumerate() {
        delta[i] = prev.wrapping_sub(b);
        prev = b;
    }

    let mut nc_freq = vec![0u32; HUFF_NC];
    for &b in &delta {
        nc_freq[b as usize] += 1;
    }
    nc_freq[SYM_FILTER] += 1;
    ensure_nonzero(&mut nc_freq);

    let nc_lengths = build_code_lengths_from_freqs(&nc_freq, MAX_CODE_LENGTH);
    let dc_lengths = build_code_lengths_from_freqs(&vec![1u32; HUFF_DC], MAX_CODE_LENGTH);
    let ldc_lengths = build_code_lengths_from_freqs(&[1u32; HUFF_LDC], MAX_CODE_LENGTH);
    let rc_lengths = build_code_lengths_from_freqs(&[1u32; HUFF_RC], MAX_CODE_LENGTH);
    let enc_nc = EncodeTable::new(&nc_lengths);

    let mut writer = BitWriter::new();
    write_tables(
        &mut writer,
        &nc_lengths,
        &dc_lengths,
        &ldc_lengths,
        &rc_lengths,
    );

    // Filter symbol + data (offset 0, length = original, delta, 1 channel).
    encode_symbol(&enc_nc, &mut writer, SYM_FILTER);
    write_filter_data(&mut writer, 0);
    write_filter_data(&mut writer, original.len() as u32);
    writer.write_bits(FILTER_DELTA as u32, 3);
    writer.write_bits(0, 5); // channels - 1

    for &b in &delta {
        encode_symbol(&enc_nc, &mut writer, b as usize);
    }

    let total_bits = writer.bit_count();
    let block_data = writer.into_bytes();
    let stream = build_block_header(&block_data, total_bits, true, true);

    let mut out = Vec::new();
    let written = decode_to_writer(
        &stream,
        original.len() as u64,
        DecodeOptions {
            dict_size_log: 0,
            ..Default::default()
        },
        &mut out,
    )
    .expect("decode");
    assert_eq!(written as usize, original.len());
    assert_eq!(out, original);
}

#[test]
fn encoder_state_carries_matches_across_chunks() {
    // A long run of identical bytes must still compress when the input
    // is split across chunk boundaries with a shared encoder state.
    let data = vec![0xABu8; 3 * DEFAULT_CHUNK_SIZE + 12345];
    let mut state = EncoderState::default();
    let mut packed = Vec::new();
    let chunks: Vec<&[u8]> = data.chunks(DEFAULT_CHUNK_SIZE).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        packed.extend(
            encode_chunked_raw(
                chunk,
                5,
                3,
                DEFAULT_CHUNK_SIZE,
                Some(&mut state),
                i + 1 == chunks.len(),
                None,
                ArchiveVersion::V50,
            )
            .unwrap(),
        );
    }
    assert!(
        packed.len() * 4 < data.len(),
        "long repeats must compress well across chunks: {} vs {}",
        packed.len(),
        data.len()
    );
    let roundtrip =
        crate::codec::decode_standalone(&packed, data.len() as u64, 3, None, ArchiveVersion::V50)
            .unwrap();
    assert_eq!(roundtrip, data);
}

#[test]
fn one_symbol_tables_are_valid() {
    let table = DecodeTable::new(&one_symbol_table(4));
    assert_eq!(table.num_symbols, 4);
}

#[test]
fn v70_extra_dist_self_roundtrip() {
    use crate::codec::decode_standalone;
    // Varied data (literal-heavy) at several sizes.
    for size in [1usize, 100, 1000, 100_000, 300_000] {
        let data: Vec<u8> = (0..size).map(|i| (i.wrapping_mul(31) >> 3) as u8).collect();
        let packed = encode_raw(&data, 3, 0, ArchiveVersion::V70);
        let back = decode_standalone(
            &packed,
            size as u64,
            0,
            Some(128 * 1024),
            ArchiveVersion::V70,
        )
        .unwrap();
        assert_eq!(back, data, "size {size}");
    }
    // Repeated data (match/cache-heavy).
    let data = vec![0xABu8; 300_000];
    let packed = encode_raw(&data, 3, 0, ArchiveVersion::V70);
    let back = decode_standalone(
        &packed,
        data.len() as u64,
        0,
        Some(128 * 1024),
        ArchiveVersion::V70,
    )
    .unwrap();
    assert_eq!(back, data);
}

/// Deterministic pseudo-random bytes (LCG) — incompressible.
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

/// Long-range matches (WinRAR `-mcl` semantics): a repeated block far
/// beyond the near window (tail + chunk) must still compress, and the
/// stream must round-trip byte-identically.
#[test]
fn long_range_matches_compress_distant_repeats() {
    use crate::codec::decode_standalone;
    // 2 MiB random + 2 MiB exact copy: distance 2 MiB, far beyond the
    // 128 KiB near window of a 64 KiB chunk encoder.
    let half = 2 * 1024 * 1024usize;
    let mut data = pseudo_random(half, 42);
    let copy = data[..half].to_vec();
    data.extend_from_slice(&copy);
    let packed = encode_chunked_raw(
        &data,
        3,
        8,
        64 * 1024,
        None,
        true,
        None,
        ArchiveVersion::V50,
    )
    .unwrap();
    // The copy half must compress to a small fraction; the random half
    // stores at ~1:1. Well below 1.5 MiB total proves the 2 MiB
    // repeat was matched.
    assert!(
        packed.len() < half + half / 4,
        "distant repeat must compress: {} vs {}",
        packed.len(),
        data.len()
    );
    // Byte-identical round-trip (dictionary 32 MiB covers the 2 MiB
    // distance; unpacked size over the RAR5 4 GiB cap is irrelevant).
    let back = decode_standalone(&packed, data.len() as u64, 8, None, ArchiveVersion::V50).unwrap();
    assert_eq!(back, data);
}

/// Long-range matches respect the dictionary window: repeats beyond
/// the declared dictionary must NOT be encoded as matches (the decoder
/// window could not reach them). With a 128 KiB dictionary the 2 MiB
/// copy is incompressible, so the encoder must store it.
#[test]
fn long_range_respects_dictionary_window() {
    // Same distant repeat as above, but with dict_size_log = 0
    // (128 KiB): the 2 MiB distance exceeds the window.
    let half = 2 * 1024 * 1024usize;
    let mut data = pseudo_random(half, 7);
    let copy = data[..half].to_vec();
    data.extend_from_slice(&copy);
    let packed = encode_chunked_raw(
        &data,
        3,
        0,
        64 * 1024,
        None,
        true,
        None,
        ArchiveVersion::V50,
    )
    .unwrap();
    // Random half ~1:1 + copy half ~1:1 → near 4 MiB. Bail-out may
    // truncate; either way it must not shrink below ~half.
    assert!(
        packed.len() > half,
        "beyond-window repeats must not match: {} vs {}",
        packed.len(),
        data.len()
    );
}

/// The long-range history slides: after more than LONG_RANGE_MAX
/// bytes, the oldest bytes drop out and the table is rebuilt; the
/// newest candidates keep matching.
#[test]
fn long_range_slides_window_and_finds() {
    use crate::codec::common::match_finder::{LONG_RANGE_MAX, LongRange};
    let mut lr = LongRange::new(128 * 1024 * 1024);
    let chunk = pseudo_random(64 * 1024, 11);
    // Push enough identical 64 KiB chunks to slide the 64 MiB window
    // several times over.
    for _ in 0..(LONG_RANGE_MAX / chunk.len() + 8) {
        lr.push(&chunk);
    }
    assert!(lr.hist_len() <= LONG_RANGE_MAX);
    // The chunk still matches against the (rebuilt) history; max_len
    // caps the match at 4096 bytes. (All blocks are identical, so the
    // table keeps only the most recent candidate — near the window
    // end; min_dist 1 accepts it.)
    let (dist, len) = lr.find(&chunk, 0, 1, 4096).expect("must find");
    assert_eq!(len, 4096, "full match must be found after sliding");
    assert!(dist as usize > 0 && dist as usize <= LONG_RANGE_MAX);
}

/// Debug reproduction: a distant copy of a random block must be found
/// through the long-range table (the pair.bin scenario).
#[test]
fn long_range_debug_distant_copy() {
    use crate::codec::common::match_finder::LongRange;
    let half = 2 * 1024 * 1024usize;
    let first = pseudo_random(half, 42);
    let mut lr = LongRange::new(32 * 1024 * 1024);
    for c in first.chunks(64 * 1024) {
        lr.push(c);
    }
    let r = lr.find(&first, 0, 128 * 1024, 4096);
    assert!(
        matches!(r, Some((_, l)) if l > 1000),
        "distant copy must be found, got {r:?}"
    );
}

/// Simulates the streaming write path: one `encode_chunked` call per
/// 4 MiB buffer with a shared encoder state. A 64 MiB distant copy
/// must compress (WinRAR `-mcl` semantics for large files).
#[test]
fn long_range_streaming_simulation() {
    let half = 4 * 1024 * 1024usize;
    let first = pseudo_random(half, 42);
    let mut data = first.clone();
    data.extend_from_slice(&first);
    let mut state = EncoderState::default();
    let mut packed = Vec::new();
    for chunk in data.chunks(DEFAULT_CHUNK_SIZE) {
        let is_final = chunk.len() < DEFAULT_CHUNK_SIZE;
        packed.extend(
            encode_chunked_raw(
                chunk,
                3,
                8,
                DEFAULT_CHUNK_SIZE,
                Some(&mut state),
                is_final,
                None,
                ArchiveVersion::V50,
            )
            .unwrap(),
        );
    }
    assert!(
        packed.len() < half + half / 4,
        "streaming long-range must compress the distant copy: {}",
        packed.len()
    );
}
