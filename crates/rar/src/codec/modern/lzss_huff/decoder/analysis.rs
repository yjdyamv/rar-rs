//! Symbol-stream analysis and tracing tooling.
//!
//! `analyze_stream` replays a packed block sequence and buckets literal,
//! match and cache-reference statistics; `trace_stream` additionally records
//! per-symbol traces. Both consume
//! [`SymbolReader`](super::symbols::SymbolReader) and never touch the solid
//! state.

use super::*;

use super::symbols::{MatchKind, Symbol, SymbolReader, SymbolState};
use crate::error::RarResult;

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
/// statistics without materializing the output.
#[doc(hidden)]
pub fn analyze_stream(
    data: &[u8],
    unpacked_size: u64,
    _dict_size_log: u8,
    variant: ArchiveVersion,
) -> RarResult<StreamAnalysis> {
    let mut state = SymbolState::default();
    let mut reader = SymbolReader::new(data, variant, 0, unpacked_size, &mut state);
    let mut out = StreamAnalysis::default();
    let mut block = BlockStat::default();
    let mut in_block = false;
    let mut produced = 0u64;

    while let Some(symbol) = reader.next()? {
        match symbol {
            Symbol::BlockStart(info) => {
                if in_block {
                    out.blocks.push(std::mem::take(&mut block));
                }
                block = BlockStat {
                    block_size: info.block_size,
                    table_present: info.table_present,
                    nc: info.nc,
                    dc: info.dc,
                    ldc: info.ldc,
                    rc: info.rc,
                    ..Default::default()
                };
                in_block = true;
            }
            Symbol::Literal(_) => {
                block.literals += 1;
                block.out_bytes += 1;
                produced += 1;
            }
            Symbol::Filter(filt) => {
                block.filters += 1;
                // Record the filter region's member-relative coverage.
                out.filter_regions
                    .push((filt.block_start, filt.block_length));
            }
            Symbol::Match { dist, len, kind } => {
                block.out_bytes += len as u64;
                produced += len as u64;
                match kind {
                    MatchKind::Repeat => block.repeats += 1,
                    MatchKind::Cache => {
                        block.cache_matches += 1;
                        bucket_len(&mut out, len);
                        bucket_dist(&mut out, dist as u32);
                    }
                    MatchKind::Match => {
                        block.matches += 1;
                        bucket_len(&mut out, len);
                        bucket_dist(&mut out, dist.min(u32::MAX as u64) as u32);
                        if len == 2 || len == 3 {
                            bucket_dist_short(&mut out, len, dist.min(u32::MAX as u64) as u32);
                        }
                    }
                }
            }
        }
    }
    if in_block {
        out.blocks.push(block);
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
    let mut state = SymbolState::default();
    let mut reader = SymbolReader::new(data, variant, 0, unpacked_size, &mut state);
    let mut out = Vec::new();
    let mut pos = 0u64;

    while let Some(symbol) = reader.next()? {
        match symbol {
            Symbol::BlockStart(_) | Symbol::Filter(_) => {}
            Symbol::Literal(_) => {
                let p = pos;
                pos += 1;
                if p < want_end && p + 1 > want_start {
                    out.push(TraceSymbol {
                        out_pos: p,
                        kind: "lit",
                        dist: 0,
                        len: 0,
                    });
                }
            }
            Symbol::Match { dist, len, kind } => {
                let p = pos;
                pos += len as u64;
                if p < want_end && p + len as u64 > want_start {
                    let kind = match kind {
                        MatchKind::Repeat => "repeat",
                        MatchKind::Cache => "cache",
                        MatchKind::Match => "match",
                    };
                    out.push(TraceSymbol {
                        out_pos: p,
                        kind,
                        dist,
                        len,
                    });
                }
            }
        }
    }
    Ok(out)
}
