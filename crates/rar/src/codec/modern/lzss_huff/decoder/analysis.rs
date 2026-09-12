//! Symbol-stream analysis and tracing tooling.
//!
//! `analyze_stream` replays a packed block sequence and buckets literal,
//! match and cache-reference statistics; `trace_stream` additionally records
//! per-symbol traces. Both share the bit-level primitives in
//! [`super::tables`] and never touch the solid state.

use super::*;

use super::super::{BLOCK_CHECKSUM_SEED, SYM_CACHE_BASE, SYM_FILTER, SYM_MATCH_BASE, SYM_REPEAT};
use super::tables::{
    apply_length_bonus_u64, decode_distance, decode_length, dist_cache_push, dist_cache_touch,
    parse_filter, read_tables,
};
use crate::codec::common::bitstream::BitReader;
use crate::codec::common::huffman::decode_symbol;
use crate::error::{RarError, RarResult};
// ── Symbol-stream analysis (tooling) ───────────────────────────────────────

/// Per-block symbol statistics, used by the analysis examples to dissect
/// WinRAR-produced streams and compare them with ours. Not a public API;
/// hidden because it exists only for the interop gap work.
#[doc(hidden)]
#[derive(Default, Clone, Debug)]
pub struct BlockStat {
    pub block_size: u32,
    pub table_present: bool,
    pub nc: usize,
    pub dc: usize,
    pub ldc: usize,
    pub rc: usize,
    pub literals: u64,
    pub matches: u64,
    pub cache_matches: u64,
    pub repeats: u64,
    pub filters: u64,
    pub out_bytes: u64,
}

/// Whole-stream symbol statistics.
#[doc(hidden)]
#[derive(Default, Clone, Debug)]
pub struct StreamAnalysis {
    pub blocks: Vec<BlockStat>,
    pub unpacked: u64,
    /// Match length buckets: <2, 2, 3, 4-15, 16-63, 64-255, 256-1023, 1024+.
    pub len_hist: [u64; 8],
    /// Match distance buckets: <4K, 4K-64K, 64K-1M, 1M-4M, 4M+.
    pub dist_hist: [u64; 5],
    /// Distance buckets (<16, <256, <4K, <64K, 64K+) for len-2 and len-3
    /// matches: `[len-2][bucket]`. WinRAR's short matches are almost all at
    /// short distances, so this separates the cheap ones from the rest.
    pub short_dist: [[u64; 5]; 2],
    /// Filter regions as `(member-relative start, length)`.
    pub filter_regions: Vec<(u64, u64)>,
}

/// Walk a RAR5/RAR7 compressed member stream and record per-block symbol
/// statistics without materializing the output. Mirrors `decode_inner`'s
/// state machine (block headers, table reads, cache/repeat semantics) so
/// the recorded streams are the exact ones a decoder would execute.
#[doc(hidden)]
pub fn analyze_stream(
    data: &[u8],
    unpacked_size: u64,
    _dict_size_log: u8,
    variant: ArchiveVersion,
) -> RarResult<StreamAnalysis> {
    let mut reader = BitReader::new(data);
    let mut dist_cache = [0u64; DIST_CACHE_SIZE];
    let mut last_length = 0u32;
    let mut table_nc: Option<DecodeTable> = None;
    let mut table_dc: Option<DecodeTable> = None;
    let mut table_ldc: Option<DecodeTable> = None;
    let mut table_rc: Option<DecodeTable> = None;
    let mut out = StreamAnalysis::default();
    let mut produced = 0u64;

    while produced < unpacked_size {
        let block_flags_byte = reader
            .read_byte()
            .map_err(|e| RarError::Format(e.to_string()))?;
        let table_present = (block_flags_byte >> 7) & 1 != 0;
        let is_last_block = (block_flags_byte >> 6) & 1 != 0;
        let byte_count = ((block_flags_byte >> 3) & 3) + 1;
        let bit_size = block_flags_byte & 7;
        let checksum_byte = reader
            .read_byte()
            .map_err(|e| RarError::Format(e.to_string()))?;
        let mut block_size: u32 = 0;
        for i in 0..byte_count {
            let b = reader
                .read_byte()
                .map_err(|e| RarError::Format(e.to_string()))?;
            block_size |= (b as u32) << (i * 8);
        }
        let mut expected_ck = BLOCK_CHECKSUM_SEED ^ block_flags_byte;
        for i in 0..byte_count {
            expected_ck ^= (block_size >> (i * 8)) as u8;
        }
        if checksum_byte != expected_ck {
            return Err(RarError::Format(format!(
                "block checksum mismatch: got {checksum_byte:#x}, expected {expected_ck:#x}"
            )));
        }
        if block_size == 0 {
            return Err(RarError::Format("zero-length block".into()));
        }
        let block_bits = ((block_size as u64) - 1) * 8 + (1 + bit_size as u64);
        let block_start_bits = reader.byte_position() as u64 * 8 + reader.bit_position() as u64;

        let (mut nc, mut dc, mut ldc, mut rc) = (0usize, 0usize, 0usize, 0usize);
        if table_present {
            let (tnc, tdc, tldc, trc) = read_tables(&mut reader, variant)?;
            nc = tnc.num_symbols;
            dc = tdc.num_symbols;
            ldc = tldc.num_symbols;
            rc = trc.num_symbols;
            table_nc = Some(tnc);
            table_dc = Some(tdc);
            table_ldc = Some(tldc);
            table_rc = Some(trc);
        }
        let t_nc = table_nc
            .as_ref()
            .ok_or(RarError::Format("no Huffman tables defined".into()))?;
        let t_dc = table_dc
            .as_ref()
            .ok_or(RarError::Format("no Huffman tables defined".into()))?;
        let t_ldc = table_ldc
            .as_ref()
            .ok_or(RarError::Format("no Huffman tables defined".into()))?;
        let t_rc = table_rc
            .as_ref()
            .ok_or(RarError::Format("no Huffman tables defined".into()))?;

        let mut stat = BlockStat {
            block_size,
            table_present,
            nc,
            dc,
            ldc,
            rc,
            ..Default::default()
        };
        while produced < unpacked_size {
            let cur_bits = reader.byte_position() as u64 * 8 + reader.bit_position() as u64;
            if cur_bits - block_start_bits >= block_bits {
                break;
            }
            let sym =
                decode_symbol(t_nc, &mut reader).map_err(|e| RarError::Format(e.to_string()))?;
            if sym < 256 {
                stat.literals += 1;
                stat.out_bytes += 1;
                produced += 1;
            } else if sym == SYM_FILTER {
                stat.filters += 1;
                let filt = parse_filter(&mut reader, produced)?;
                // Record the filter region's member-relative coverage.
                out.filter_regions
                    .push((filt.block_start, filt.block_length));
            } else if sym == SYM_REPEAT {
                stat.repeats += 1;
                if last_length > 0 && dist_cache[0] > 0 {
                    stat.out_bytes += last_length as u64;
                    produced += last_length as u64;
                }
            } else if (SYM_CACHE_BASE..=SYM_CACHE_BASE + 3).contains(&sym) {
                let cache_idx = sym - SYM_CACHE_BASE;
                let dist = dist_cache_touch(&mut dist_cache, cache_idx);
                let len_slot = decode_symbol(t_rc, &mut reader)
                    .map_err(|e| RarError::Format(e.to_string()))?;
                let length = decode_length(len_slot, &mut reader)?;
                last_length = length;
                stat.cache_matches += 1;
                stat.out_bytes += length as u64;
                produced += length as u64;
                bucket_len(&mut out, length);
                bucket_dist(&mut out, dist as u32);
            } else if sym >= SYM_MATCH_BASE {
                let len_slot = sym - SYM_MATCH_BASE;
                let mut length = decode_length(len_slot, &mut reader)?;
                let dist_slot = decode_symbol(t_dc, &mut reader)
                    .map_err(|e| RarError::Format(e.to_string()))?;
                let dist = decode_distance(dist_slot, &mut reader, t_ldc)?;
                length = apply_length_bonus_u64(length, dist);
                last_length = length;
                dist_cache_push(&mut dist_cache, dist);
                stat.matches += 1;
                stat.out_bytes += length as u64;
                produced += length as u64;
                bucket_len(&mut out, length);
                bucket_dist(&mut out, dist.min(u32::MAX as u64) as u32);
                if length == 2 || length == 3 {
                    bucket_dist_short(&mut out, length, dist.min(u32::MAX as u64) as u32);
                }
            }
        }
        let block_end_bits = block_start_bits + block_bits;
        reader.set_position((block_end_bits / 8) as usize, (block_end_bits % 8) as u8);
        out.blocks.push(stat);
        if is_last_block {
            break;
        }
    }
    out.unpacked = produced;
    Ok(out)
}

fn bucket_len(out: &mut StreamAnalysis, len: u32) {
    let b = if len < 2 {
        0
    } else if len == 2 {
        1
    } else if len == 3 {
        2
    } else if len < 16 {
        3
    } else if len < 64 {
        4
    } else if len < 256 {
        5
    } else if len < 1024 {
        6
    } else {
        7
    };
    out.len_hist[b] += 1;
}

fn bucket_dist(out: &mut StreamAnalysis, dist: u32) {
    let b = if dist < 4096 {
        0
    } else if dist < 65536 {
        1
    } else if dist < 1 << 20 {
        2
    } else if dist < 4 << 20 {
        3
    } else {
        4
    };
    out.dist_hist[b] += 1;
}

/// Distance buckets for len-2 / len-3 matches (`[len_idx][bucket]`).
fn bucket_dist_short(out: &mut StreamAnalysis, len: u32, dist: u32) {
    let b = if dist < 16 {
        0
    } else if dist < 256 {
        1
    } else if dist < 4096 {
        2
    } else if dist < 65536 {
        3
    } else {
        4
    };
    let idx = (len - 2) as usize;
    out.short_dist[idx][b] += 1;
}

/// One decoded symbol with its output position (debug tooling).
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct TraceSymbol {
    pub out_pos: u64,
    pub kind: &'static str,
    pub dist: u64,
    pub len: u32,
}

/// Walk the stream like [`analyze_stream`] but record every symbol whose
/// output span intersects `[want_start, want_end)`, in stream order. Used
/// to find which symbol corrupted a member's output.
#[doc(hidden)]
pub fn trace_stream(
    data: &[u8],
    unpacked_size: u64,
    variant: ArchiveVersion,
    want_start: u64,
    want_end: u64,
) -> RarResult<Vec<TraceSymbol>> {
    let mut reader = BitReader::new(data);
    let mut dist_cache = [0u64; DIST_CACHE_SIZE];
    let mut last_length = 0u32;
    let mut table_nc: Option<DecodeTable> = None;
    let mut table_dc: Option<DecodeTable> = None;
    let mut table_ldc: Option<DecodeTable> = None;
    let mut table_rc: Option<DecodeTable> = None;
    let mut out = Vec::new();
    let mut produced = 0u64;

    while produced < unpacked_size {
        let block_flags_byte = reader
            .read_byte()
            .map_err(|e| RarError::Format(e.to_string()))?;
        let is_last_block = (block_flags_byte >> 6) & 1 != 0;
        let byte_count = ((block_flags_byte >> 3) & 3) + 1;
        let bit_size = block_flags_byte & 7;
        reader
            .read_byte()
            .map_err(|e| RarError::Format(e.to_string()))?; // checksum
        let mut block_size: u32 = 0;
        for i in 0..byte_count {
            let b = reader
                .read_byte()
                .map_err(|e| RarError::Format(e.to_string()))?;
            block_size |= (b as u32) << (i * 8);
        }
        let block_bits = ((block_size as u64) - 1) * 8 + (1 + bit_size as u64);
        let block_start_bits = reader.byte_position() as u64 * 8 + reader.bit_position() as u64;

        if (block_flags_byte >> 7) & 1 != 0 {
            let (tnc, tdc, tldc, trc) = read_tables(&mut reader, variant)?;
            table_nc = Some(tnc);
            table_dc = Some(tdc);
            table_ldc = Some(tldc);
            table_rc = Some(trc);
        }
        let t_nc = table_nc
            .as_ref()
            .ok_or(RarError::Format("no Huffman tables defined".into()))?;
        let t_dc = table_dc
            .as_ref()
            .ok_or(RarError::Format("no Huffman tables defined".into()))?;
        let t_ldc = table_ldc
            .as_ref()
            .ok_or(RarError::Format("no Huffman tables defined".into()))?;
        let t_rc = table_rc
            .as_ref()
            .ok_or(RarError::Format("no Huffman tables defined".into()))?;
        while produced < unpacked_size {
            let cur_bits = reader.byte_position() as u64 * 8 + reader.bit_position() as u64;
            if cur_bits - block_start_bits >= block_bits {
                break;
            }
            let sym =
                decode_symbol(t_nc, &mut reader).map_err(|e| RarError::Format(e.to_string()))?;
            if sym < 256 {
                let p = produced;
                produced += 1;
                if p < want_end && p + 1 > want_start {
                    out.push(TraceSymbol {
                        out_pos: p,
                        kind: "lit",
                        dist: 0,
                        len: 0,
                    });
                }
            } else if sym == SYM_FILTER {
                parse_filter(&mut reader, produced)?;
            } else if sym == SYM_REPEAT {
                let p = produced;
                if last_length > 0 && dist_cache[0] > 0 {
                    produced += last_length as u64;
                }
                if p < want_end && p + last_length as u64 > want_start {
                    out.push(TraceSymbol {
                        out_pos: p,
                        kind: "repeat",
                        dist: dist_cache[0],
                        len: last_length,
                    });
                }
            } else if (SYM_CACHE_BASE..=SYM_CACHE_BASE + 3).contains(&sym) {
                let cache_idx = sym - SYM_CACHE_BASE;
                let dist = dist_cache_touch(&mut dist_cache, cache_idx);
                let len_slot = decode_symbol(t_rc, &mut reader)
                    .map_err(|e| RarError::Format(e.to_string()))?;
                let length = decode_length(len_slot, &mut reader)?;
                last_length = length;
                let p = produced;
                produced += length as u64;
                if p < want_end && p + length as u64 > want_start {
                    out.push(TraceSymbol {
                        out_pos: p,
                        kind: "cache",
                        dist,
                        len: length,
                    });
                }
            } else if sym >= SYM_MATCH_BASE {
                let len_slot = sym - SYM_MATCH_BASE;
                let mut length = decode_length(len_slot, &mut reader)?;
                let dist_slot = decode_symbol(t_dc, &mut reader)
                    .map_err(|e| RarError::Format(e.to_string()))?;
                let dist = decode_distance(dist_slot, &mut reader, t_ldc)?;
                length = apply_length_bonus_u64(length, dist);
                last_length = length;
                dist_cache_push(&mut dist_cache, dist);
                let p = produced;
                produced += length as u64;
                if p < want_end && p + length as u64 > want_start {
                    out.push(TraceSymbol {
                        out_pos: p,
                        kind: "match",
                        dist,
                        len: length,
                    });
                }
            }
        }
        let block_end_bits = block_start_bits + block_bits;
        reader.set_position((block_end_bits / 8) as usize, (block_end_bits % 8) as u8);
        if is_last_block {
            break;
        }
    }
    Ok(out)
}
