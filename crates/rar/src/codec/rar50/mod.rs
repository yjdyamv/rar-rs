//! RAR5 (and RAR7/v70) native LZSS+Huffman codec.
//!
//! Clean-room implementation for software conservation and educational
//! purposes. Bitstream format derived from analysis of libarchive's
//! archive_read_support_format_rar5.c by Grzegorz Antoniak (2018),
//! an independent BSD-2-Clause licensed implementation.
//!
//! License: BSD-2-Clause

mod decoder;
mod encoder;

pub use decoder::*;
pub use encoder::pick_delta_channel;
pub use encoder::*;

use crate::rar50::{COMP_METHOD_BEST, COMP_METHOD_FASTEST, COMP_METHOD_STORE};

// ── Tables / format constants ──────────────────────────────────────────────

/// Huffman table symbol counts.
pub const HUFF_BC: usize = 20;
pub const HUFF_NC: usize = 306;
pub const HUFF_DC: usize = 64;
/// RAR7 (v70) extended distance codes: 80 codes cover distances up to
/// ~1 TB (the RAR5 table stops at 4 GB).
pub const HUFF_DCX: usize = 80;
pub const HUFF_LDC: usize = 16;
pub const HUFF_RC: usize = 44;

/// Maximum Huffman code bit length.
pub const MAX_CODE_LENGTH: usize = 15;

/// Quick lookup table size (2^QUICK_BITS entries).
pub const QUICK_BITS: usize = 10;
pub const QUICK_SIZE: usize = 1 << QUICK_BITS;

/// Special symbols in the NC table.
pub const SYM_FILTER: usize = 256;
pub const SYM_REPEAT: usize = 257;
pub const SYM_CACHE_BASE: usize = 258;
pub const SYM_MATCH_BASE: usize = 262;

/// Distance cache size.
pub const DIST_CACHE_SIZE: usize = 4;

/// Filter types.
pub const FILTER_DELTA: u8 = 0;
pub const FILTER_E8: u8 = 1;
pub const FILTER_E8E9: u8 = 2;
pub const FILTER_ARM: u8 = 3;

/// Block header checksum seed.
pub const BLOCK_CHECKSUM_SEED: u8 = 0x5A;

/// Nibble-based RLE escape value for Huffman table encoding.
pub const NIBBLE_ESCAPE: u8 = 15;

// ── High-level dispatch ────────────────────────────────────────────────────

/// Compress `data` using the specified RAR5 compression method.
pub fn compress(data: &[u8], method: u8, dict_size_log: u8) -> Result<Vec<u8>, String> {
    compress_with_progress(data, method, dict_size_log, None)
}

/// Compress `data` using the specified RAR5 compression method, reporting
/// progress as `(bytes_processed, total_bytes)` through `progress`.
pub fn compress_with_progress(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    progress: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<Vec<u8>, String> {
    if method == COMP_METHOD_STORE {
        if let Some(cb) = progress {
            cb(data.len() as u64, data.len() as u64);
        }
        return Ok(data.to_vec());
    }
    if (COMP_METHOD_FASTEST..=COMP_METHOD_BEST).contains(&method) {
        return compress_chunked(
            data,
            method,
            dict_size_log,
            DEFAULT_CHUNK_SIZE,
            None,
            true,
            progress,
            false,
        );
    }
    Err(format!("unknown compression method: {method}"))
}

/// Compress `data` in bounded chunks, optionally carrying encoder state
/// across files (solid archives). The symbol table and match finder stay
/// proportional to `chunk_size` instead of the whole file.
///
/// `extra_dist` selects the RAR7 (v70) distance code table (80 entries
/// instead of 64); set it when the member header declares a dictionary
/// above 4 GiB.
#[allow(clippy::too_many_arguments)]
pub fn compress_chunked(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    chunk_size: usize,
    state: Option<&mut EncoderState>,
    is_final: bool,
    progress: Option<&mut dyn FnMut(u64, u64)>,
    extra_dist: bool,
) -> Result<Vec<u8>, String> {
    if method == COMP_METHOD_STORE {
        if let Some(cb) = progress {
            cb(data.len() as u64, data.len() as u64);
        }
        return Ok(data.to_vec());
    }
    if (COMP_METHOD_FASTEST..=COMP_METHOD_BEST).contains(&method) {
        return encode_chunked(
            data,
            method,
            dict_size_log,
            chunk_size,
            state,
            is_final,
            progress,
            extra_dist,
        );
    }
    Err(format!("unknown compression method: {method}"))
}

/// Decompress `data` using the specified RAR5 compression method.
pub fn decompress(
    data: &[u8],
    method: u8,
    unpacked_size: u64,
    dict_size_log: u8,
    state: Option<&mut DecoderState>,
) -> Result<Vec<u8>, String> {
    if method == COMP_METHOD_STORE {
        return Ok(data.to_vec());
    }
    if (COMP_METHOD_FASTEST..=COMP_METHOD_BEST).contains(&method) {
        let result = decode(
            data,
            unpacked_size,
            DecodeOptions {
                dict_size_log,
                state,
                ..Default::default()
            },
        )?;
        if result.len() != unpacked_size as usize {
            return Err(format!(
                "decompressed size mismatch: expected {unpacked_size}, got {}",
                result.len()
            ));
        }
        return Ok(result);
    }
    Err(format!("unknown compression method: {method}"))
}

#[cfg(all(test, feature = "parallel"))]
mod mt_tests {
    use super::*;

    /// Deterministic pseudo-random block (xorshift64), so runs repeat.
    fn prng_block(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect()
    }

    /// Data spanning several chunks with a far copy of the head plus
    /// text-like filler: exercises tail context and the shared long-range
    /// table across slice boundaries.
    fn mixed_data() -> Vec<u8> {
        let mut data = prng_block(300_000, 7);
        let far_copy = data[..300_000].to_vec();
        data.extend(far_copy);
        data.extend(b"hello world ".repeat(40_000));
        data
    }

    #[test]
    fn roundtrips_across_dictionary_sizes() {
        for &log in &[0u8, 3, 6] {
            let data = mixed_data();
            let packed = encode_chunked_mt(
                &data,
                3,
                log,
                DEFAULT_CHUNK_SIZE,
                &mut EncoderState::default(),
                4,
                true,
                false,
            );
            let out = decode_standalone(&packed, data.len() as u64, log, None, false).unwrap();
            assert_eq!(out, data, "dict log {log}");
        }
    }

    #[test]
    fn v70_extra_dist_roundtrip() {
        let data = mixed_data();
        let packed = encode_chunked_mt(
            &data,
            3,
            6,
            DEFAULT_CHUNK_SIZE,
            &mut EncoderState::default(),
            3,
            true,
            true,
        );
        let out =
            decode_standalone(&packed, data.len() as u64, 6, Some(48 * 1024 * 1024), true).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn windows_continue_the_chain_like_sequential() {
        let w1 = prng_block(2 * DEFAULT_CHUNK_SIZE + 777, 11);
        let mut w2 = prng_block(DEFAULT_CHUNK_SIZE + 123, 22);
        w2[1000..2000].copy_from_slice(&w1[1000..2000]);
        let mut st = EncoderState::default();
        let mut packed = encode_chunked_mt(&w1, 3, 6, DEFAULT_CHUNK_SIZE, &mut st, 3, false, false);
        packed.extend(encode_chunked_mt(
            &w2,
            3,
            6,
            DEFAULT_CHUNK_SIZE,
            &mut st,
            3,
            true,
            false,
        ));
        let mut full = w1;
        full.extend(&w2);
        let out = decode_standalone(&packed, full.len() as u64, 6, None, false).unwrap();
        assert_eq!(out, full);
    }

    #[test]
    fn deterministic_across_runs() {
        let data = mixed_data();
        let a = encode_chunked_mt(
            &data,
            3,
            6,
            DEFAULT_CHUNK_SIZE,
            &mut EncoderState::default(),
            4,
            true,
            false,
        );
        let b = encode_chunked_mt(
            &data,
            3,
            6,
            DEFAULT_CHUNK_SIZE,
            &mut EncoderState::default(),
            4,
            true,
            false,
        );
        assert_eq!(a, b);
    }

    /// The matchless fast path must be byte-identical to the full pricing
    /// passes: toggle it off, encode every corpus, toggle it back on, and
    /// compare. Corpora cover the fast path's trigger (random), its
    /// fallback triggers (text, repeats, structured data) and mixes.
    #[test]
    fn matchless_fast_path_is_byte_identical() {
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                set_fast_path_enabled(true);
            }
        }
        let _g = Guard;

        let mut corpora: Vec<(String, Vec<u8>)> = Vec::new();
        corpora.push(("random".into(), prng_block(3 * DEFAULT_CHUNK_SIZE, 42)));
        corpora.push((
            "text".into(),
            b"the quick brown fox jumps over the lazy dog\n".repeat(200_000),
        ));
        corpora.push(("mixed".into(), mixed_data()));
        let mut rep_random = prng_block(DEFAULT_CHUNK_SIZE + 4096, 99);
        rep_random.extend(rep_random[..DEFAULT_CHUNK_SIZE].to_vec());
        corpora.push(("self-copy".into(), rep_random));
        let mut zipped = prng_block(DEFAULT_CHUNK_SIZE, 7);
        for (i, b) in b"hello world ".iter().cycle().take(4096).enumerate() {
            zipped[2 * i] = *b;
        }
        corpora.push(("structured".into(), zipped));

        for (name, data) in &corpora {
            for (level, dict_log, extra) in [
                (2u8, 6u8, false),
                (3, 6, false),
                (5, 6, false),
                (3, 3, true),
            ] {
                set_fast_path_enabled(true);
                let fast = encode_chunked_mt(
                    data,
                    level,
                    dict_log,
                    DEFAULT_CHUNK_SIZE,
                    &mut EncoderState::default(),
                    3,
                    true,
                    extra,
                );
                set_fast_path_enabled(false);
                let full = encode_chunked_mt(
                    data,
                    level,
                    dict_log,
                    DEFAULT_CHUNK_SIZE,
                    &mut EncoderState::default(),
                    3,
                    true,
                    extra,
                );
                assert_eq!(
                    fast, full,
                    "{name} l{level} dict{dict_log} extra{extra}: fast path diverged"
                );
                let out =
                    decode_standalone(&fast, data.len() as u64, dict_log, None, extra).unwrap();
                assert_eq!(out, *data, "{name} l{level}: fast-path decode mismatch");
            }
        }
    }
}
