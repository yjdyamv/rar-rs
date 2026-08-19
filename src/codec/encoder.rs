/// RAR5 block encoder — compresses data into RAR5 compressed bitstream blocks.
///
/// Bitstream format derived from analysis of libarchive's
/// archive_read_support_format_rar5.c by Grzegorz Antoniak (2018),
/// an independent BSD-2-Clause licensed implementation.
use super::bitstream::BitWriter;
use super::filters::apply_filter_encode;
use super::huffman::{EncodeTable, build_code_lengths_from_freqs, encode_symbol};
use super::lz_match::{self, MatchFinder};
use super::tables::*;

// ── Compression level parameters ───────────────────────────────────────────

// (chain_len, lazy_threshold, max_match)
const LEVEL_PARAMS: [(usize, usize, usize); 6] = [
    (0, 0, 0),         // 0: store (unused)
    (4, 0, 0x1001),    // 1: fastest
    (16, 0, 0x1001),   // 2: fast
    (96, 8, 0x1001),   // 3: normal
    (256, 8, 0x1001),  // 4: good
    (1024, 8, 0x1001), // 5: best
];

const MAX_BLOCK_SIZE: usize = 0x20000; // 128 KB

/// Maximum length of one RAR5 filter block.
///
/// RARLAB readers (unrar/WinRAR) refuse filter regions larger than 256 KiB
/// (`0x40000`); the reference writer splits members into filter blocks of at
/// most `0x3FFFF` bytes. Same value as the `rars` project's
/// `MAX_FILTER_BLOCK_LENGTH` (MIT OR Apache-2.0).
pub const MAX_FILTER_BLOCK_LENGTH: u32 = 0x3FFFF;

/// Default input chunk size for the encoder. Processing input in bounded
/// slices keeps the symbol table (and match finder) memory proportional to
/// the chunk size instead of the whole file.
pub const DEFAULT_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// A RAR5 output filter applied to a region of the decompressed member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FilterSpec {
    /// FILTER_DELTA, FILTER_E8, FILTER_E8E9 or FILTER_ARM.
    pub filter_type: u8,
    /// Delta channel count (1-4); ignored for other filter types.
    pub channels: u8,
    /// Start offset of the filtered region within the member.
    pub block_start: u32,
    /// Length of the filtered region.
    pub block_length: u32,
}

impl FilterSpec {
    pub fn new(filter_type: u8, channels: u8, block_start: u32, block_length: u32) -> Self {
        Self {
            filter_type,
            channels,
            block_start,
            block_length,
        }
    }
}

/// Symbol representation for the match finder output.
#[derive(Clone)]
enum Symbol {
    Literal(u8),
    Match {
        distance: u32,
        length: u32,
    },
    CacheRef {
        index: usize,
        length: u32,
    },
    Repeat,
    Filter {
        block_start: u32,
        block_length: u32,
        filter_type: u8,
        channels: u8,
    },
}

/// Persistent encoder state for solid archive support.
///
/// Carries the lookbehind window tail, distance cache and last length
/// across files (and across chunks within a file) so consecutive
/// compressed members share one LZ window. Also carries the long-range
/// match history (WinRAR `-mcl` style sampled table over the recent
/// input) so distant repeated blocks compress across chunk boundaries.
#[derive(Default)]
pub struct EncoderState {
    tail: Vec<u8>,
    dist_cache: [u32; DIST_CACHE_SIZE],
    last_length: u32,
    /// Long-range match history; `None` for compression levels where the
    /// long range search is disabled (method 1, like WinRAR).
    long_range: Option<lz_match::LongRange>,
}

impl EncoderState {
    /// Reset the solid chain. Call after any member that does not
    /// participate in the LZ window (directories, STORE files, empty
    /// files).
    pub fn reset(&mut self) {
        self.tail.clear();
        self.dist_cache = [0; DIST_CACHE_SIZE];
        self.last_length = 0;
        if let Some(lr) = self.long_range.as_mut() {
            lr.reset();
        }
    }
}

/// Encode raw data into RAR5/RAR7 compressed format. `extra_dist` selects
/// the RAR7 (v70) 80-entry distance code table (RAR5 uses 64).
pub fn encode(data: &[u8], method: u8, dict_size_log: u8, extra_dist: bool) -> Vec<u8> {
    encode_chunked(
        data,
        method,
        dict_size_log,
        DEFAULT_CHUNK_SIZE,
        None,
        true,
        None,
        extra_dist,
    )
    .unwrap_or_default()
}

/// Encode raw data into RAR5/RAR7 compressed format, reporting match-finder
/// progress as `(bytes_processed, total_bytes)`.
pub fn encode_with_progress(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    progress: Option<&mut dyn FnMut(u64, u64)>,
    extra_dist: bool,
) -> Vec<u8> {
    encode_chunked(
        data,
        method,
        dict_size_log,
        DEFAULT_CHUNK_SIZE,
        None,
        true,
        progress,
        extra_dist,
    )
    .unwrap_or_default()
}

/// Encode `data` in bounded chunks, optionally carrying encoder state
/// across calls (solid archives and multi-chunk files). `is_final` marks
/// the last call of one member so only its final block carries the
/// end-of-stream flag. Returns the compressed stream; callers fall back to
/// STORE when the result is not smaller than the input.
///
/// `extra_dist` selects the RAR7 (v70) distance code table.
pub fn encode_chunked(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    chunk_size: usize,
    state: Option<&mut EncoderState>,
    is_final: bool,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
    extra_dist: bool,
) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Ok(encode_empty_block(extra_dist));
    }

    let level = (method as usize).clamp(1, 5);
    let (chain_len, lazy_thresh, max_match) = LEVEL_PARAMS[level];
    let dict_size = 128 * 1024 * (1usize << dict_size_log as u32);
    // WinRAR applies the long range search to -m2..-m5 and ignores it
    // for -m1 (fastest); it is automatic (no -mcl switch needed) and
    // mandatory for v70 dictionaries.
    let long_range = level >= 2;

    let mut local_state = EncoderState::default();
    let state = state.unwrap_or(&mut local_state);
    let chunk_size = chunk_size.max(1);

    let mut output = Vec::new();
    let mut chunk_start = 0usize;
    let mut next_report = 0u64;

    while chunk_start < data.len() {
        let chunk_end = (chunk_start + chunk_size).min(data.len());
        let chunk = &data[chunk_start..chunk_end];
        let symbols = find_matches_with_tail(
            state,
            chunk,
            chain_len,
            lazy_thresh,
            max_match,
            dict_size,
            long_range,
        );

        let mut block_start = 0usize;
        while block_start < symbols.len() {
            let (block_end, _) = find_block_end(&symbols, block_start, MAX_BLOCK_SIZE);
            let is_last = is_final && chunk_end >= data.len() && block_end >= symbols.len();
            let block_data = encode_block(&symbols[block_start..block_end], is_last, extra_dist);
            output.extend(block_data);
            // Early bail-out: once the compressed stream already exceeds
            // the input size it can never beat STORE — stop before wasting
            // more time (callers fall back to STORE on oversized output).
            if !is_last && output.len() > data.len() {
                break;
            }
            block_start = block_end;
        }
        if output.len() > data.len() {
            break;
        }

        chunk_start = chunk_end;
        if let Some(cb) = progress.as_deref_mut()
            && chunk_end as u64 >= next_report
        {
            cb(chunk_end as u64, data.len() as u64);
            next_report = chunk_end as u64 + 0x10000;
        }
    }

    Ok(output)
}

/// Encode `data` as a single RAR5 member with output filters applied.
///
/// The filters are recorded at the start of the symbol stream (the decoder
/// applies each filter to its region once the region is fully produced, so
/// emitting the records early is equivalent to inline emission). `data` is
/// forward-transformed per filter spec before match finding. The caller is
/// responsible for comparing the packed size against unfiltered output and
/// falling back to STORE.
pub fn encode_with_filters(
    data: &[u8],
    method: u8,
    dict_size_log: u8,
    filters: &[FilterSpec],
    extra_dist: bool,
) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Ok(encode_empty_block(extra_dist));
    }
    if filters.is_empty() {
        return encode_chunked(
            data,
            method,
            dict_size_log,
            data.len(),
            None,
            true,
            None,
            extra_dist,
        );
    }

    // Split over-long regions: RARLAB readers reject filter blocks above
    // MAX_FILTER_BLOCK_LENGTH. Each piece is an independent filter record
    // with its own transform state, so splitting is byte-exact.
    let mut specs: Vec<FilterSpec> = Vec::new();
    for f in filters {
        let mut start = f.block_start;
        let mut remaining = f.block_length;
        while remaining > 0 {
            let len = remaining.min(MAX_FILTER_BLOCK_LENGTH);
            specs.push(FilterSpec::new(f.filter_type, f.channels, start, len));
            start = start.saturating_add(len);
            remaining = remaining.saturating_sub(len);
        }
    }

    // 1. Forward-transform each region. Regions must be disjoint; the
    //    transform reads only its own slice, and E8/ARM file offsets are
    //    absolute member positions.
    let mut transformed = data.to_vec();
    for f in &specs {
        let start = f.block_start as usize;
        let end = (start + f.block_length as usize).min(transformed.len());
        if start >= end {
            continue;
        }
        let t = apply_filter_encode(
            f.filter_type,
            &mut transformed[start..end],
            f.channels,
            f.block_start as u64,
        );
        transformed[start..end].copy_from_slice(&t);
    }

    // 2. Match-find on the transformed data.
    let level = (method as usize).clamp(1, 5);
    let (chain_len, lazy_thresh, max_match) = LEVEL_PARAMS[level];
    let dict_size = 128 * 1024 * (1usize << dict_size_log as u32);

    let mut finder = MatchFinder::new(&transformed, 2, max_match, chain_len, dict_size);
    let mut dist_cache = [0u32; DIST_CACHE_SIZE];
    let mut last_length = 0u32;
    let mut symbols: Vec<Symbol> = specs
        .iter()
        .map(|f| Symbol::Filter {
            block_start: f.block_start,
            block_length: f.block_length,
            filter_type: f.filter_type,
            channels: f.channels,
        })
        .collect();
    symbols.extend(find_matches_in_range(
        &transformed,
        &mut finder,
        0,
        transformed.len(),
        lazy_thresh,
        &mut dist_cache,
        &mut last_length,
        max_match,
        None,
    ));

    // 3. Emit blocks (filters live in the first block).
    let mut output = Vec::new();
    let mut block_start = 0usize;
    while block_start < symbols.len() {
        let (block_end, _) = find_block_end(&symbols, block_start, MAX_BLOCK_SIZE);
        let is_last = block_end >= symbols.len();
        let block_data = encode_block(&symbols[block_start..block_end], is_last, extra_dist);
        output.extend(block_data);
        if !is_last && output.len() > data.len() {
            break;
        }
        block_start = block_end;
    }
    Ok(output)
}

// ── Match finding ──────────────────────────────────────────────────────────

/// Find matches for `chunk`, searching against `state.tail` as lookbehind.
/// Advances `state` so a following chunk/file continues the LZ window.
/// When `long_range` is set, distances beyond the near window are found
/// through the sampled long-range history (WinRAR `-mcl` semantics).
fn find_matches_with_tail(
    state: &mut EncoderState,
    chunk: &[u8],
    chain_len: usize,
    lazy_thresh: usize,
    max_match: usize,
    window: usize,
    long_range: bool,
) -> Vec<Symbol> {
    let tail_len = state.tail.len();
    let mut combined = Vec::with_capacity(tail_len + chunk.len());
    combined.extend_from_slice(&state.tail);
    combined.extend_from_slice(chunk);

    let mut finder = MatchFinder::new(&combined, 2, max_match, chain_len, window);
    for pos in 0..tail_len {
        finder.insert(pos);
    }

    // Borrow the long-range state read-only for the search; the history
    // is only updated (mutably) after the symbol stream is produced.
    let lr = if long_range {
        let lr = state
            .long_range
            .get_or_insert_with(|| lz_match::LongRange::new(window));
        // The near finder covers distances up to tail + chunk; long-range
        // candidates only matter beyond that.
        let near_max = tail_len + chunk.len();
        Some((&*lr, near_max))
    } else {
        None
    };

    let mut dist_cache = state.dist_cache;
    let mut last_length = state.last_length;
    let symbols = find_matches_in_range(
        &combined,
        &mut finder,
        tail_len,
        combined.len(),
        lazy_thresh,
        &mut dist_cache,
        &mut last_length,
        max_match,
        lr,
    );

    if long_range {
        if let Some(lr) = state.long_range.as_mut() {
            lr.push(chunk);
        }
    }

    // The near window (tail) only needs to cover short-distance matches:
    // longer distances come from the sampled long-range history. Capping
    // the tail keeps the per-chunk rebuild cost (inserting the whole
    // tail into the hash chain) bounded instead of O(window) per chunk.
    const NEAR_WINDOW_MAX: usize = 8 * 1024 * 1024;
    let keep = window.min(NEAR_WINDOW_MAX).min(combined.len());
    state.tail = combined[combined.len() - keep..].to_vec();
    state.dist_cache = dist_cache;
    state.last_length = last_length;
    symbols
}

/// Match-finding loop over `data[start..end]` with a distance cache.
/// `lr` (when present) adds long-range candidates from the sampled
/// history: `(long_range, near_max)` where `near_max` is the largest
/// distance the near finder can produce (tail + chunk), so long-range
/// hits are only considered beyond it.
fn find_matches_in_range(
    data: &[u8],
    finder: &mut MatchFinder<'_>,
    start: usize,
    end: usize,
    lazy_thresh: usize,
    dist_cache: &mut [u32; DIST_CACHE_SIZE],
    last_length: &mut u32,
    max_match: usize,
    lr: Option<(&lz_match::LongRange, usize)>,
) -> Vec<Symbol> {
    let mut symbols = Vec::with_capacity(end - start);
    let mut pos = start;

    // After this many consecutive non-matching positions the finder
    // switches to fast mode: every position is still inserted into the
    // hash chain (the window stays complete for later matches), the
    // literal is emitted directly, and only the sampled long-range table
    // is probed (on its 16-byte grid). Incompressible runs (random data,
    // media) then cost one hash insertion per byte instead of a full
    // match attempt with its cache-missing random accesses, while the
    // distant repeats that justify the compression pass are still found.
    const FAST_MODE_AFTER: usize = 64 * 1024;
    let mut no_match_run = 0usize;
    let mut fast = false;

    while pos < end {
        let (mut dist, mut length) = if fast {
            finder.insert(pos);
            let mut d = 0usize;
            let mut l = 0usize;
            if let Some((long_range, near_max)) = lr
                && pos + 4 <= end
                && (pos & (lz_match::LONG_RANGE_STEP - 1)) == 0
            {
                let chunk_off = pos - start;
                if let Some((ld, ll)) =
                    long_range.find(&data[start..end], chunk_off, near_max + 1, max_match)
                {
                    d = ld as usize;
                    l = ll;
                }
            }
            (d, l)
        } else {
            let (mut d, mut l) = finder.find_match_cached(pos, dist_cache);

            // Long-range candidate: only when the near window found
            // nothing useful (a good near match is never worse than a
            // far one).
            if let Some((long_range, near_max)) = lr
                && l < 64
                && pos + 4 <= end
            {
                let chunk_off = pos - start;
                if let Some((ld, ll)) =
                    long_range.find(&data[start..end], chunk_off, near_max + 1, max_match)
                    && ll > l
                {
                    d = ld as usize;
                    l = ll;
                }
            }
            (d, l)
        };

        if dist > 0 && lazy_thresh > 0 && length < lazy_thresh && pos + 1 < end {
            let (dist2, length2) = finder.find_match_cached(pos + 1, dist_cache);
            if length2 > length {
                symbols.push(Symbol::Literal(data[pos]));
                *last_length = 0;
                pos += 1;
                dist = dist2;
                length = length2;
            }
        }

        if dist > 0 {
            if fast {
                // A match (long-range) resumes full matching.
                fast = false;
                no_match_run = 0;
            }
            let cache_idx = cache_find(dist_cache, dist as u32);
            if let Some(idx) = cache_idx {
                if idx == 0 && length as u32 == *last_length && *last_length > 0 {
                    symbols.push(Symbol::Repeat);
                } else {
                    symbols.push(Symbol::CacheRef {
                        index: idx,
                        length: length as u32,
                    });
                    *last_length = length as u32;
                }
                cache_touch(dist_cache, idx);
            } else {
                let raw_length = remove_length_bonus(length as u32, dist as u32);
                if raw_length >= 2 {
                    symbols.push(Symbol::Match {
                        distance: dist as u32,
                        length: raw_length,
                    });
                    cache_push(dist_cache, dist as u32);
                    *last_length = apply_length_bonus(raw_length, dist as u32);
                } else {
                    for i in 0..length {
                        symbols.push(Symbol::Literal(data[pos + i]));
                        finder.insert(pos + i);
                    }
                    *last_length = 0;
                    pos += length;
                    continue;
                }
            }

            for i in 1..length {
                finder.insert(pos + i);
            }
            pos += length;
        } else {
            symbols.push(Symbol::Literal(data[pos]));
            *last_length = 0;
            no_match_run += 1;
            if !fast && no_match_run >= FAST_MODE_AFTER {
                fast = true;
            }
            pos += 1;
        }
    }

    symbols
}

fn find_block_end(symbols: &[Symbol], start: usize, max_uncompressed: usize) -> (usize, usize) {
    let mut count = 0usize;
    let mut last_len = 0u32;
    for (offset, symbol) in symbols[start..].iter().enumerate() {
        let i = start + offset;
        match symbol {
            Symbol::Literal(_) => {
                count += 1;
                last_len = 0;
            }
            Symbol::Match { distance, length } => {
                last_len = apply_length_bonus(*length, *distance);
                count += last_len as usize;
            }
            Symbol::CacheRef { length, .. } => {
                last_len = *length;
                count += *length as usize;
            }
            Symbol::Repeat => {
                count += last_len as usize;
            }
            Symbol::Filter { .. } => {}
        }
        if count >= max_uncompressed {
            return (i + 1, count);
        }
    }
    (symbols.len(), count)
}

// ── Block encoding ─────────────────────────────────────────────────────────

fn encode_block(symbols: &[Symbol], is_last: bool, extra_dist: bool) -> Vec<u8> {
    // Collect frequencies
    let dc_count = if extra_dist { HUFF_DCX } else { HUFF_DC };
    let mut nc_freq = vec![0u32; HUFF_NC];
    let mut dc_freq = vec![0u32; dc_count];
    let mut ldc_freq = vec![0u32; HUFF_LDC];
    let mut rc_freq = vec![0u32; HUFF_RC];

    for sym in symbols {
        match sym {
            Symbol::Literal(b) => nc_freq[*b as usize] += 1,
            Symbol::Match { distance, length } => {
                let len_slot = encode_length_slot(*length);
                nc_freq[SYM_MATCH_BASE + len_slot] += 1;
                let (dist_slot, _, _) = encode_distance_slot(*distance, extra_dist);
                dc_freq[dist_slot] += 1;
                if dist_slot >= 4 {
                    let dbits = dist_slot / 2 - 1;
                    if dbits >= 4 {
                        let base = (2 | (dist_slot & 1)) << dbits;
                        let low_dist = ((*distance as usize) - 1 - base) & 0xF;
                        ldc_freq[low_dist] += 1;
                    }
                }
            }
            Symbol::CacheRef { index: _, length } => {
                nc_freq[SYM_CACHE_BASE] += 1; // simplified — actual index varies
                let len_slot = encode_length_slot(*length);
                rc_freq[len_slot] += 1;
            }
            Symbol::Repeat => nc_freq[SYM_REPEAT] += 1,
            Symbol::Filter { .. } => nc_freq[SYM_FILTER] += 1,
        }
    }

    // Re-count cache refs with actual indices
    // (The above was a simplification; let's do it properly)
    nc_freq[SYM_CACHE_BASE] = 0;
    for sym in symbols {
        if let Symbol::CacheRef { index, .. } = sym {
            nc_freq[SYM_CACHE_BASE + index] += 1;
        }
    }
    // Subtract the earlier simplified count was only for SYM_CACHE_BASE,
    // but we zeroed it — the per-index counts are correct now.

    ensure_nonzero(&mut nc_freq);
    ensure_nonzero(&mut dc_freq);
    ensure_nonzero(&mut ldc_freq);
    ensure_nonzero(&mut rc_freq);

    let nc_lengths = build_code_lengths_from_freqs(&nc_freq, MAX_CODE_LENGTH);
    let dc_lengths = build_code_lengths_from_freqs(&dc_freq, MAX_CODE_LENGTH);
    let ldc_lengths = build_code_lengths_from_freqs(&ldc_freq, MAX_CODE_LENGTH);
    let rc_lengths = build_code_lengths_from_freqs(&rc_freq, MAX_CODE_LENGTH);

    let enc_nc = EncodeTable::new(&nc_lengths);
    let enc_dc = EncodeTable::new(&dc_lengths);
    let enc_ldc = EncodeTable::new(&ldc_lengths);
    let enc_rc = EncodeTable::new(&rc_lengths);

    let mut writer = BitWriter::new();

    write_tables(
        &mut writer,
        &nc_lengths,
        &dc_lengths,
        &ldc_lengths,
        &rc_lengths,
    );

    for sym in symbols {
        match sym {
            Symbol::Literal(b) => encode_symbol(&enc_nc, &mut writer, *b as usize),
            Symbol::Match { distance, length } => {
                write_match(
                    &mut writer,
                    &enc_nc,
                    &enc_dc,
                    &enc_ldc,
                    *distance,
                    *length,
                    extra_dist,
                );
            }
            Symbol::CacheRef { index, length } => {
                write_cache_ref(&mut writer, &enc_nc, &enc_rc, *index, *length);
            }
            Symbol::Repeat => encode_symbol(&enc_nc, &mut writer, SYM_REPEAT),
            Symbol::Filter {
                block_start,
                block_length,
                filter_type,
                channels,
            } => {
                encode_symbol(&enc_nc, &mut writer, SYM_FILTER);
                write_filter_data(&mut writer, *block_start);
                write_filter_data(&mut writer, *block_length);
                writer.write_bits(*filter_type as u32, 3);
                if *filter_type == FILTER_DELTA {
                    writer.write_bits(channels.saturating_sub(1) as u32, 5);
                }
            }
        }
    }

    let total_bits = writer.bit_count();
    let block_data = writer.into_bytes();

    let (total_bits, block_data) = if total_bits == 0 {
        (8, vec![0u8])
    } else {
        (total_bits, block_data)
    };

    build_block_header(&block_data, total_bits, is_last, true)
}

fn encode_empty_block(extra_dist: bool) -> Vec<u8> {
    let mut writer = BitWriter::new();
    let nc_lengths = {
        let mut v = vec![0u8; HUFF_NC];
        v[0] = 1;
        v
    };
    let dc_lengths = {
        let mut v = vec![0u8; if extra_dist { HUFF_DCX } else { HUFF_DC }];
        v[0] = 1;
        v
    };
    let ldc_lengths = {
        let mut v = vec![0u8; HUFF_LDC];
        v[0] = 1;
        v
    };
    let rc_lengths = {
        let mut v = vec![0u8; HUFF_RC];
        v[0] = 1;
        v
    };

    write_tables(
        &mut writer,
        &nc_lengths,
        &dc_lengths,
        &ldc_lengths,
        &rc_lengths,
    );

    let total_bits = writer.bit_count();
    let block_data = writer.into_bytes();
    let (total_bits, block_data) = if total_bits == 0 {
        (8, vec![0u8])
    } else {
        (total_bits, block_data)
    };

    build_block_header(&block_data, total_bits, true, true)
}

fn build_block_header(
    block_data: &[u8],
    total_bits: usize,
    is_last: bool,
    table_present: bool,
) -> Vec<u8> {
    let block_size = block_data.len();
    let valid_last_bits = total_bits - (block_size - 1) * 8;
    let bit_size = if valid_last_bits > 0 {
        (valid_last_bits - 1) as u8
    } else {
        7
    };

    let byte_count: u8 = if block_size <= 0xFF {
        1
    } else if block_size <= 0xFFFF {
        2
    } else {
        3
    };

    let mut flags: u8 = 0;
    if table_present {
        flags |= 1 << 7;
    }
    if is_last {
        flags |= 1 << 6;
    }
    flags |= (byte_count - 1) << 3;
    flags |= bit_size & 7;

    let mut size_bytes = vec![0u8; byte_count as usize];
    for (i, byte) in size_bytes.iter_mut().enumerate() {
        *byte = ((block_size >> (i * 8)) & 0xFF) as u8;
    }

    let mut checksum = BLOCK_CHECKSUM_SEED ^ flags;
    for &b in &size_bytes {
        checksum ^= b;
    }

    let mut header = Vec::with_capacity(2 + size_bytes.len() + block_data.len());
    header.push(flags);
    header.push(checksum);
    header.extend(&size_bytes);
    header.extend(block_data);
    header
}

// ── Huffman table writing ──────────────────────────────────────────────────

fn write_tables(
    writer: &mut BitWriter,
    nc_lengths: &[u8],
    dc_lengths: &[u8],
    ldc_lengths: &[u8],
    rc_lengths: &[u8],
) {
    let mut all_lengths = Vec::with_capacity(HUFF_NC + HUFF_DC + HUFF_LDC + HUFF_RC);
    all_lengths.extend_from_slice(nc_lengths);
    all_lengths.extend_from_slice(dc_lengths);
    all_lengths.extend_from_slice(ldc_lengths);
    all_lengths.extend_from_slice(rc_lengths);

    let rle_symbols = rle_encode_lengths(&all_lengths);

    let mut bc_freq = vec![0u32; HUFF_BC];
    for item in &rle_symbols {
        bc_freq[item.0 as usize] += 1;
    }
    ensure_nonzero(&mut bc_freq);

    let bc_lengths = build_code_lengths_from_freqs(&bc_freq, MAX_CODE_LENGTH);
    write_bc_nibbles(writer, &bc_lengths);

    let enc_bc = EncodeTable::new(&bc_lengths);
    for item in &rle_symbols {
        encode_symbol(&enc_bc, writer, item.0 as usize);
        match item.0 {
            16 => writer.write_bits(item.1 as u32 - 3, 3),
            17 => writer.write_bits(item.1 as u32 - 11, 7),
            18 => writer.write_bits(item.1 as u32 - 3, 3),
            19 => writer.write_bits(item.1 as u32 - 11, 7),
            _ => {}
        }
    }
}

fn write_bc_nibbles(writer: &mut BitWriter, bc_lengths: &[u8]) {
    let mut i = 0;
    while i < HUFF_BC {
        let val = bc_lengths[i];
        if val == 0 {
            let mut run = 0;
            while i + run < HUFF_BC && bc_lengths[i + run] == 0 {
                run += 1;
            }
            while run > 0 {
                if run >= 3 {
                    let count = run.min(16);
                    writer.write_bits(NIBBLE_ESCAPE as u32, 4);
                    writer.write_bits((count - 2) as u32, 4);
                    run -= count;
                    i += count;
                } else {
                    writer.write_bits(0, 4);
                    run -= 1;
                    i += 1;
                }
            }
        } else if val == 15 {
            writer.write_bits(NIBBLE_ESCAPE as u32, 4);
            writer.write_bits(0, 4);
            i += 1;
        } else {
            writer.write_bits(val as u32, 4);
            i += 1;
        }
    }
}

/// RLE item: (symbol, repeat_count). For symbols 0-15, count is 0.
struct RleItem(u8, usize);

fn rle_encode_lengths(lengths: &[u8]) -> Vec<RleItem> {
    let mut result = Vec::new();
    let n = lengths.len();
    let mut i = 0;

    while i < n {
        let val = lengths[i];
        if val == 0 {
            let mut run = 0;
            while i + run < n && lengths[i + run] == 0 {
                run += 1;
            }
            while run > 0 {
                if run >= 11 {
                    let count = run.min(138);
                    result.push(RleItem(19, count));
                    run -= count;
                    i += count;
                } else if run >= 3 {
                    let count = run.min(10);
                    result.push(RleItem(18, count));
                    run -= count;
                    i += count;
                } else {
                    result.push(RleItem(0, 0));
                    run -= 1;
                    i += 1;
                }
            }
        } else {
            result.push(RleItem(val, 0));
            i += 1;
            let mut run = 0;
            while i + run < n && lengths[i + run] == val {
                run += 1;
            }
            while run > 0 {
                if run >= 11 {
                    let count = run.min(138);
                    result.push(RleItem(17, count));
                    run -= count;
                    i += count;
                } else if run >= 3 {
                    let count = run.min(10);
                    result.push(RleItem(16, count));
                    run -= count;
                    i += count;
                } else {
                    break;
                }
            }
        }
    }

    result
}

// ── Match/distance encoding helpers ────────────────────────────────────────

fn write_match(
    writer: &mut BitWriter,
    enc_nc: &EncodeTable,
    enc_dc: &EncodeTable,
    enc_ldc: &EncodeTable,
    dist: u32,
    length: u32,
    extra_dist: bool,
) {
    let len_slot = encode_length_slot(length);
    encode_symbol(enc_nc, writer, SYM_MATCH_BASE + len_slot);
    write_length_extra(writer, length, len_slot);

    let (dist_slot, extra, dbits) = encode_distance_slot(dist, extra_dist);
    encode_symbol(enc_dc, writer, dist_slot);
    write_distance_extra(writer, enc_ldc, dist_slot, extra, dbits);
}

fn write_cache_ref(
    writer: &mut BitWriter,
    enc_nc: &EncodeTable,
    enc_rc: &EncodeTable,
    cache_idx: usize,
    length: u32,
) {
    encode_symbol(enc_nc, writer, SYM_CACHE_BASE + cache_idx);
    let len_slot = encode_length_slot(length);
    encode_symbol(enc_rc, writer, len_slot);
    write_length_extra(writer, length, len_slot);
}

fn encode_length_slot(length: u32) -> usize {
    if length < 2 {
        return 0;
    }
    if length <= 9 {
        return (length - 2) as usize;
    }
    let val = length - 2;
    let high_bit = 32 - val.leading_zeros() - 1;
    if high_bit < 2 {
        return (length - 2) as usize;
    }
    let lbits = high_bit - 2;
    let sub = (val >> lbits) & 3;
    let slot = 4 * (lbits + 1) + sub;
    slot.min(HUFF_RC as u32 - 1) as usize
}

fn write_length_extra(writer: &mut BitWriter, length: u32, slot: usize) {
    if slot >= 8 {
        let lbits = (slot / 4 - 1) as u8;
        let base = 2 + ((4 | (slot & 3)) << lbits) as u32;
        let extra = length - base;
        if lbits > 0 {
            writer.write_bits(extra, lbits);
        }
    }
}

/// Map a match distance to its distance-code slot.
///
/// RAR5 caps the table at 64 entries (`HUFF_DC`), RAR7 (v70) at 80
/// (`HUFF_DCX`), which covers distances up to 1 TiB. `dbits >= 4` slots
/// split their extra bits between a raw prefix and the low nibble encoded
/// through the LDC table.
fn encode_distance_slot(dist: u32, extra_dist: bool) -> (usize, u32, usize) {
    if dist <= 4 {
        return ((dist - 1) as usize, 0, 0);
    }
    let val = dist - 1;
    let high_bit = (32 - val.leading_zeros() - 1) as usize;
    if high_bit < 1 {
        return ((dist - 1) as usize, 0, 0);
    }
    let dbits = high_bit - 1;
    let sub = (val >> dbits) & 1;
    let slot = 2 * (dbits + 1) + sub as usize;
    let base = (2 | sub) << dbits;
    let extra = val - base;
    let max_slot = if extra_dist {
        HUFF_DCX - 1
    } else {
        HUFF_DC - 1
    };
    (slot.min(max_slot), extra, dbits)
}

fn write_distance_extra(
    writer: &mut BitWriter,
    enc_ldc: &EncodeTable,
    dist_slot: usize,
    extra: u32,
    dbits: usize,
) {
    if dist_slot >= 4 && dbits > 0 {
        if dbits >= 4 {
            if dbits > 4 {
                let upper = extra >> 4;
                writer.write_bits(upper, (dbits - 4) as u8);
            }
            let low = (extra & 0xF) as usize;
            encode_symbol(enc_ldc, writer, low);
        } else {
            writer.write_bits(extra, dbits as u8);
        }
    }
}

// ── Cache helpers ──────────────────────────────────────────────────────────

fn cache_find(cache: &[u32; DIST_CACHE_SIZE], dist: u32) -> Option<usize> {
    cache.iter().position(|&d| d == dist)
}

fn cache_push(cache: &mut [u32; DIST_CACHE_SIZE], dist: u32) {
    cache[3] = cache[2];
    cache[2] = cache[1];
    cache[1] = cache[0];
    cache[0] = dist;
}

fn cache_touch(cache: &mut [u32; DIST_CACHE_SIZE], idx: usize) {
    let val = cache[idx];
    for i in (1..=idx).rev() {
        cache[i] = cache[i - 1];
    }
    cache[0] = val;
}

fn apply_length_bonus(length: u32, dist: u32) -> u32 {
    let mut l = length;
    if dist > 0x100 {
        l += 1;
    }
    if dist > 0x2000 {
        l += 1;
    }
    if dist > 0x40000 {
        l += 1;
    }
    l
}

fn remove_length_bonus(length: u32, dist: u32) -> u32 {
    let mut l = length;
    if dist > 0x100 {
        l -= 1;
    }
    if dist > 0x2000 {
        l -= 1;
    }
    if dist > 0x40000 {
        l -= 1;
    }
    l
}

fn ensure_nonzero(freq: &mut [u32]) {
    let nonzero = freq.iter().filter(|&&f| f > 0).count();
    if nonzero < 2 {
        let mut added = 0;
        for f in freq.iter_mut() {
            if *f == 0 {
                *f = 1;
                added += 1;
                if nonzero + added >= 2 {
                    break;
                }
            }
        }
    }
}

/// Emit RAR5 filter data: `[2 bits byte_count-1][LE bytes]`.
fn write_filter_data(writer: &mut BitWriter, value: u32) {
    let bytes = value.to_le_bytes();
    let count = if value <= 0xFF {
        1
    } else if value <= 0xFFFF {
        2
    } else if value <= 0xFF_FFFF {
        3
    } else {
        4
    };
    writer.write_bits((count - 1) as u32, 2);
    for &b in &bytes[..count] {
        writer.write_bits(b as u32, 8);
    }
}

#[cfg(test)]
mod tests {
    use super::super::huffman::DecodeTable;
    use super::*;
    use crate::codec::decoder::decode_to_writer;

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
            crate::codec::DecodeOptions {
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
                encode_chunked(
                    chunk,
                    5,
                    3,
                    DEFAULT_CHUNK_SIZE,
                    Some(&mut state),
                    i + 1 == chunks.len(),
                    None,
                    false,
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
            crate::codec::decode_standalone(&packed, data.len() as u64, 3, None, false).unwrap();
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
            let packed = encode(&data, 3, 0, true);
            let back = decode_standalone(&packed, size as u64, 0, Some(128 * 1024), true).unwrap();
            assert_eq!(back, data, "size {size}");
        }
        // Repeated data (match/cache-heavy).
        let data = vec![0xABu8; 300_000];
        let packed = encode(&data, 3, 0, true);
        let back =
            decode_standalone(&packed, data.len() as u64, 0, Some(128 * 1024), true).unwrap();
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
        data.extend_from_slice(&data[..half].to_vec());
        let packed = encode_chunked(&data, 3, 8, 64 * 1024, None, true, None, false).unwrap();
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
        let back = decode_standalone(&packed, data.len() as u64, 8, None, false).unwrap();
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
        data.extend_from_slice(&data[..half].to_vec());
        let packed = encode_chunked(&data, 3, 0, 64 * 1024, None, true, None, false).unwrap();
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
        use super::lz_match::{LONG_RANGE_MAX, LongRange};
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
        use super::lz_match::LongRange;
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
                encode_chunked(
                    chunk,
                    3,
                    8,
                    DEFAULT_CHUNK_SIZE,
                    Some(&mut state),
                    is_final,
                    None,
                    false,
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
}
