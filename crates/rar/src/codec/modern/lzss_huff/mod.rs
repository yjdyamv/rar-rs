//! RAR5 (and RAR7/v70) native LZSS+Huffman codec.
//!
//! Clean-room implementation for software conservation and educational
//! purposes. Bitstream format derived from analysis of libarchive's
//! archive_read_support_format_rar5.c by Grzegorz Antoniak (2018),
//! an independent BSD-2-Clause licensed implementation.
//!
//! License: BSD-2-Clause

mod decode;
mod decoder;
mod encode;
mod encoder;

pub use decode::{
    BlockStat, DecodeOptions, DecoderState, MAX_STREAMING_FILTER_BUFFER, StreamAnalysis,
    TraceSymbol, analyze_stream, decode, decode_raw, decode_standalone,
    decode_standalone_to_writer, decode_to_writer, trace_stream,
};
#[cfg(feature = "parallel")]
pub(crate) use encode::encode_chunked_mt_with_progress;
pub(crate) use encode::encode_chunked_raw_with_lead;
#[cfg(all(test, feature = "parallel"))]
pub(crate) use encode::set_fast_path_enabled;
pub use encode::{
    DEFAULT_CHUNK_SIZE, EncodeOptions, EncoderState, FilterSpec, MAX_FILTER_BLOCK_LENGTH, encode,
    encode_chunked, encode_chunked_mt, encode_chunked_raw, encode_raw,
    encode_with_auto_delta_filter, encode_with_auto_x86_filter, encode_with_filters,
    encode_with_filters_mt, encode_with_progress_raw, pick_delta_channel,
};
pub(crate) use encoder::{Symbol, delta_stream_window, merge_ranges, x86_stream_window};

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

#[cfg(all(test, feature = "parallel"))]
mod mt_tests {
    use super::*;
    use crate::version::ArchiveVersion;

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
                ArchiveVersion::V50,
            );
            let out = decode_standalone(&packed, data.len() as u64, log, None, ArchiveVersion::V50)
                .unwrap();
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
            ArchiveVersion::V70,
        );
        let out = decode_standalone(
            &packed,
            data.len() as u64,
            6,
            Some(48 * 1024 * 1024),
            ArchiveVersion::V70,
        )
        .unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn windows_continue_the_chain_like_sequential() {
        let w1 = prng_block(2 * DEFAULT_CHUNK_SIZE + 777, 11);
        let mut w2 = prng_block(DEFAULT_CHUNK_SIZE + 123, 22);
        w2[1000..2000].copy_from_slice(&w1[1000..2000]);
        let mut st = EncoderState::default();
        let mut packed = encode_chunked_mt(
            &w1,
            3,
            6,
            DEFAULT_CHUNK_SIZE,
            &mut st,
            3,
            false,
            ArchiveVersion::V50,
        );
        packed.extend(encode_chunked_mt(
            &w2,
            3,
            6,
            DEFAULT_CHUNK_SIZE,
            &mut st,
            3,
            true,
            ArchiveVersion::V50,
        ));
        let mut full = w1;
        full.extend(&w2);
        let out =
            decode_standalone(&packed, full.len() as u64, 6, None, ArchiveVersion::V50).unwrap();
        assert_eq!(out, full);
    }

    /// Two members that share content: `second` opens with a verbatim copy of
    /// `first`'s last 512 KiB and continues with fresh incompressible bytes.
    /// Both halves are uncompressible on their own, so an encoder that really
    /// keeps the window across the member boundary collapses the copied half
    /// into one long match while an independent member has to emit it raw.
    fn shared_pair() -> (Vec<u8>, Vec<u8>) {
        let borrow = 512 * 1024;
        let first = prng_block(3 * 1024 * 1024, 4242);
        let mut second = first[first.len() - borrow..].to_vec();
        second.extend(prng_block(borrow, 99));
        (first, second)
    }

    /// Reproduces the e2e3 corpus: three *identical* members, each
    /// `[shared 1 MiB][unique 11 MiB incompressible][shared 1 MiB]`. At 13 MiB a
    /// member is larger than the near finder's reach (8 MiB tail + 4 MiB chunk =
    /// 12 MiB), so the third member's only reference to the previous member sits
    /// at `~|member|` (here 13 MiB) — squarely in the long-range table's slot.
    /// That table must retain the full window; if it drops to half (its old
    /// behaviour) the third member loses the reference and stops chaining, falling
    /// back to re-compressing the whole member independently.
    #[test]
    fn sequential_solid_chain_random_shared_blocks() {
        const LOG: u8 = 7; // 16 MiB dict -> 8 MiB near-finder window
        let shared = prng_block(1024 * 1024, 11);
        let mk = |seed: u64| {
            let mut m = shared.clone();
            m.extend(prng_block(11 * 1024 * 1024, seed));
            m.extend(&shared);
            m
        };
        // Identical members (e2e3 copies one buffer three times).
        let members = vec![mk(1), mk(1), mk(1)];
        let mut st = EncoderState::default();
        let mut packed = Vec::new();
        let last = members.len() - 1;
        let mut sizes = Vec::new();
        for (i, member) in members.iter().enumerate() {
            st.begin_member();
            let out = encode_chunked(
                member,
                EncodeOptions {
                    chunk_size: DEFAULT_CHUNK_SIZE,
                    state: Some(&mut st),
                    is_final: i == last,
                    variant: ArchiveVersion::V50,
                    ..EncodeOptions::new(3, LOG)
                },
            )
            .unwrap();
            sizes.push(out.len());
            packed.extend(out);
        }
        println!("serial chain member sizes: {sizes:?}");

        let mut full = Vec::new();
        for m in &members {
            full.extend(m);
        }
        let out =
            decode_standalone(&packed, full.len() as u64, LOG, None, ArchiveVersion::V50).unwrap();
        assert_eq!(out, full);
        // Every member after the first is one copy of the previous member, so a
        // working chain collapses it to a few dozen KiB (just the copy/flag
        // overhead). The long-range table must retain the full window to reach
        // across `|member|`; if it sheds half of it, the third member can no
        // longer reference the previous member and falls back to re-compressing
        // the whole ~13 MiB member independently (~5 MiB).
        assert!(
            sizes[1] < 256 * 1024,
            "2nd member lost the window: {} bytes",
            sizes[1]
        );
        assert!(
            sizes[2] < 256 * 1024,
            "3rd member lost the window: {} bytes",
            sizes[2]
        );
    }

    /// Simulates exactly what `add_file` does: chunk the member *externally*
    /// and call `encode_chunked` once per chunk (with `skip_incompressible_probe`
    /// set, as the write path does), instead of once per whole member. If the
    /// chain breaks on member three here too, the bug is in how the state is
    /// carried across those repeated calls.
    #[test]
    fn cli_like_external_chunking_serial_chain() {
        const LOG: u8 = 7;
        let shared = prng_block(2 * 1024 * 1024, 11);
        let mk = |seed: u64| {
            let mut m = shared.clone();
            m.extend(prng_block(10 * 1024 * 1024, seed));
            m.extend(&shared);
            m
        };
        let members = [mk(1), mk(2), mk(3)];
        let mut st = EncoderState::default();
        let mut packed = Vec::new();
        let mut sizes = Vec::new();
        for member in &members {
            st.begin_member();
            let mut bytes_read = 0u64;
            let mut member_packed = Vec::new();
            for chunk in member.chunks(DEFAULT_CHUNK_SIZE) {
                bytes_read += chunk.len() as u64;
                let out = encode_chunked(
                    chunk,
                    EncodeOptions {
                        chunk_size: DEFAULT_CHUNK_SIZE,
                        state: Some(&mut st),
                        is_final: bytes_read >= member.len() as u64,
                        variant: ArchiveVersion::V50,
                        skip_incompressible_probe: true,
                        ..EncodeOptions::new(3, LOG)
                    },
                )
                .unwrap();
                member_packed.extend(out);
            }
            sizes.push(member_packed.len());
            packed.extend(member_packed);
        }
        println!("cli-like external-chunk sizes: {sizes:?}");
        // External chunking yields concatenated per-chunk streams; the decoder
        // keeps state across them, so the whole member still decodes — but
        // `decode_standalone` treats the concatenation as a single block
        // stream and fails here. We only assert on the compression ratio,
        // which is what the solid chain is about.
        assert!(
            sizes[2] + 256 * 1024 < sizes[0],
            "3rd member lost the window (cli-like chunking): {} vs {}",
            sizes[2],
            sizes[0]
        );
    }

    /// A solid chain whose members are all MT-encoded: the seed state must
    /// carry the window from one `encode_chunked_mt` call into the next
    /// (tail + long-range table), not just within one window.
    #[test]
    fn solid_members_share_the_window_through_mt() {
        const LOG: u8 = 6; // 8 MiB window: covers all of `first` as lookbehind
        const BORROW: usize = 512 * 1024;
        let borrow_margin = BORROW / 2;
        let (first, second) = shared_pair();
        let mut st = EncoderState::default();
        let mut packed = encode_chunked_mt(
            &first,
            3,
            LOG,
            DEFAULT_CHUNK_SIZE,
            &mut st,
            4,
            false,
            ArchiveVersion::V50,
        );
        let chained = encode_chunked_mt(
            &second,
            3,
            LOG,
            DEFAULT_CHUNK_SIZE,
            &mut st,
            4,
            true,
            ArchiveVersion::V50,
        );
        // The second member must actually reach back into the first: an
        // independent member (fresh window) cannot find those matches.
        let alone = encode_chunked_mt(
            &second,
            3,
            LOG,
            DEFAULT_CHUNK_SIZE,
            &mut EncoderState::default(),
            4,
            true,
            ArchiveVersion::V50,
        );
        assert!(
            chained.len() + borrow_margin < alone.len(),
            "second member did not share the window: chained {} vs alone {}",
            chained.len(),
            alone.len()
        );
        packed.extend(chained);

        let mut full = first;
        full.extend(&second);
        let out =
            decode_standalone(&packed, full.len() as u64, LOG, None, ArchiveVersion::V50).unwrap();
        assert_eq!(out, full);
    }

    /// A chain may switch parse tiers mid-way (only members above the write
    /// path's size threshold go MT, and STORE/small members stay
    /// sequential). Both orders must decode: the MT window leaves the seed's
    /// persistent tree keyed to a discarded frame, which the sequential
    /// member would otherwise rebase against the wrong history.
    #[test]
    fn chain_survives_switching_between_mt_and_sequential() {
        const LOG: u8 = 6;
        let (first, second) = shared_pair();
        let mut full = first.clone();
        full.extend(&second);

        for mt_first in [true, false] {
            let mut st = EncoderState::default();
            let mut packed = Vec::new();
            for (i, member) in [&first, &second].iter().enumerate() {
                let is_final = i == 1;
                let mt = if mt_first { i == 0 } else { i == 1 };
                if mt {
                    packed.extend(encode_chunked_mt(
                        member,
                        3,
                        LOG,
                        DEFAULT_CHUNK_SIZE,
                        &mut st,
                        4,
                        is_final,
                        ArchiveVersion::V50,
                    ));
                } else {
                    packed.extend(
                        encode_chunked(
                            member,
                            EncodeOptions {
                                chunk_size: DEFAULT_CHUNK_SIZE,
                                state: Some(&mut st),
                                is_final,
                                variant: ArchiveVersion::V50,
                                ..EncodeOptions::new(3, LOG)
                            },
                        )
                        .unwrap(),
                    );
                }
            }
            let out = decode_standalone(&packed, full.len() as u64, LOG, None, ArchiveVersion::V50)
                .unwrap();
            assert_eq!(out, full, "mt_first={mt_first}");
        }
    }

    /// Sequential solid chain, three members deep: the second member must
    /// match into the first, and — the case that used to regress — so must
    /// the third. Members share one incompressible block, so anything that
    /// loses the window shows up immediately as a ~-sized member.
    #[test]
    fn sequential_chain_keeps_matching_every_member() {
        const LOG: u8 = 5; // 4 MiB window, comfortably past one member
        let shared = prng_block(512 * 1024, 77);
        let filler: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
        let members: Vec<Vec<u8>> = (0..3)
            .map(|_| {
                let mut m = shared.clone();
                m.extend_from_slice(&filler);
                m
            })
            .collect();

        let mut st = EncoderState::default();
        let mut packed = Vec::new();
        let mut sizes = Vec::new();
        let last = members.len() - 1;
        for (i, member) in members.iter().enumerate() {
            st.begin_member();
            let out = encode_chunked(
                member,
                EncodeOptions {
                    chunk_size: DEFAULT_CHUNK_SIZE,
                    state: Some(&mut st),
                    is_final: i == last,
                    ..EncodeOptions::new(3, LOG)
                },
            )
            .unwrap();
            sizes.push(out.len());
            packed.extend(out);
        }
        println!("sequential chain member sizes: {sizes:?}");

        let mut full = Vec::new();
        for m in &members {
            full.extend(m);
        }
        let out =
            decode_standalone(&packed, full.len() as u64, LOG, None, ArchiveVersion::V50).unwrap();
        assert_eq!(out, full);
        // Every member after the first is one copy of the shared block plus
        // its own filler; losing the window costs the whole shared block.
        for (i, size) in sizes.iter().enumerate().skip(1) {
            assert!(
                *size + 256 * 1024 < sizes[0],
                "member {i} lost the shared window: {size} vs {}",
                sizes[0]
            );
        }
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
            ArchiveVersion::V50,
        );
        let b = encode_chunked_mt(
            &data,
            3,
            6,
            DEFAULT_CHUNK_SIZE,
            &mut EncoderState::default(),
            4,
            true,
            ArchiveVersion::V50,
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
        // Large dict + many chunks: 4-byte hash collisions are dense here, so the
        // *relaxed* matchless path (not the strict one) is what fires.
        corpora.push(("random-big".into(), prng_block(8 * DEFAULT_CHUNK_SIZE, 42)));
        // Sparse 4-byte repeats at pseudo-random distances over random data:
        // isolated length-4 matches whose first occurrence has a dead repeat
        // cache. This is exactly the case the relaxed path must still get
        // byte-identical — if a length-4 match can beat four literals there, the
        // relaxation is too aggressive and this assertion fails.
        {
            let mut s = prng_block(DEFAULT_CHUNK_SIZE * 3, 123);
            for i in (0..s.len()).step_by(137).take(4000) {
                if i + 4 <= s.len() {
                    s[i..i + 4].copy_from_slice(b"ZAP!");
                }
            }
            corpora.push(("sparse-4byte".into(), s));
        }
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
                (2u8, 6u8, ArchiveVersion::V50),
                (3, 6, ArchiveVersion::V50),
                (5, 6, ArchiveVersion::V50),
                (3, 3, ArchiveVersion::V70),
                (2, 15, ArchiveVersion::V50),
                (3, 15, ArchiveVersion::V50),
                (5, 15, ArchiveVersion::V50),
                (3, 15, ArchiveVersion::V70),
                (5, 15, ArchiveVersion::V70),
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
