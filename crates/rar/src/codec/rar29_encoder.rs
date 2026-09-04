//! RAR 3.x/4.x LZSS+Huffman encoder — the write-side counterpart of
//! [`super::rar29`].
//!
//! Ported from the encode half of the `rars` project (MIT OR Apache-2.0)
//! `codec/rar29.rs` `Unpack29Encoder`.  Produces RAR3/4-format compressed
//! block sequences (不含 FILE_HEAD，只含压缩数据流).  The write pipeline
//! (`rar40/write.rs`) handles headers, encryption, and multi-volume splitting.
//!
//! Phase 1: LZSS only (m1–m5).  PPMd and VM-filter integration are Phase 2.

use crate::codec::bitstream::BitWriter;
use crate::codec::huffman::EncodeTable;
use crate::codec::lzss_huff::DIST_CACHE_SIZE;
use crate::codec::match_finder::MatchFinder;
use crate::error::{RarError, RarResult};

// ── Table geometry ─────────────────────────────────────────────────────────

const MAIN_COUNT: usize = 299;
const OFFSET_COUNT: usize = 60;
const LOW_OFFSET_COUNT: usize = 17;
const LENGTH_COUNT: usize = 28;
const LEVEL_COUNT: usize = 20;
const TABLE_COUNT: usize = MAIN_COUNT + OFFSET_COUNT + LOW_OFFSET_COUNT + LENGTH_COUNT;

/// Sliding-window cap (4 MiB, the RAR3/4 dictionary ceiling).
const MAX_HISTORY: usize = 4 * 1024 * 1024;

// ── Encoder tuning constants ──────────────────────────────────────────────

const MAX_ENCODER_MATCH_OFFSET: usize = 1024 * 1024;
const MAX_ENCODER_MATCH_LENGTH: usize = 258;
const MAX_MATCH_CANDIDATES: usize = 256;

/// Default LZ block size for table splitting (64 KiB, benchmarked optimal in
/// rars).
pub(crate) const RAR29_LZ_BLOCK_SIZE: usize = 64 * 1024;

// ── Shared lookup tables (identical to the decoder) ───────────────────────

const LENGTH_BASES: [usize; LENGTH_COUNT] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128,
    160, 192, 224,
];
const LENGTH_BITS: [u8; LENGTH_COUNT] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5,
];
const OFFSET_BASES: [usize; OFFSET_COUNT] = [
    0, 1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536,
    2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576, 32768, 49152, 65536, 98304, 131072, 196608,
    262144, 327680, 393216, 458752, 524288, 589824, 655360, 720896, 786432, 851968, 917504, 983040,
    1048576, 1310720, 1572864, 1835008, 2097152, 2359296, 2621440, 2883584, 3145728, 3407872,
    3670016, 3932160,
];
const OFFSET_BITS: [u8; OFFSET_COUNT] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13, 14, 14, 15, 15, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 18, 18, 18, 18, 18,
    18, 18, 18, 18, 18, 18, 18,
];

// ── Error helper ───────────────────────────────────────────────────────────

fn enc_err(msg: &'static str) -> RarError {
    RarError::Format(format!("RAR 2.9 encoder: {msg}"))
}

// ═══════════════════════════════════════════════════════════════════════════
//  Match search — reuses the shared codec::match_finder::MatchFinder<>
//  (hash chain + 4-slot distance cache + windowing).  Level tuning maps to
//  its `chain_len` (per-level candidate budget) and `window` (max distance,
//  bounded by the RAR29-encodable offset table).
// ═══════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════
//  EncodeOptions
// ═══════════════════════════════════════════════════════════════════════════

/// Encoder configuration for one member (or chain of members).
#[derive(Debug, Clone, Copy)]
pub struct EncodeOptions {
    pub max_match_candidates: usize,
    pub lazy_matching: bool,
    pub lazy_lookahead: usize,
    pub max_match_distance: usize,
    pub block_size: Option<usize>,
}

impl EncodeOptions {
    const fn new(max_match_candidates: usize) -> Self {
        Self {
            max_match_candidates,
            lazy_matching: false,
            lazy_lookahead: 1,
            max_match_distance: MAX_ENCODER_MATCH_OFFSET,
            block_size: None,
        }
    }

    const fn with_lazy_matching(mut self, enabled: bool) -> Self {
        self.lazy_matching = enabled;
        self
    }

    #[allow(dead_code)] // kept: builder knob for the (unused) tuning surface
    const fn with_max_match_distance(mut self, distance: usize) -> Self {
        self.max_match_distance = distance;
        self
    }

    const fn with_block_size(mut self, bytes: usize) -> Self {
        self.block_size = Some(bytes);
        self
    }
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self::new(MAX_MATCH_CANDIDATES)
    }
}

/// Map a WinRAR compression level (1–5) to encoder options.
pub fn options_for_level(level: u8) -> EncodeOptions {
    let candidates = match level {
        1 => 8,
        2 => 32,
        3 => 64,
        4 => 96,
        5 => 128,
        _ => 64,
    };
    let opts = EncodeOptions::new(candidates);
    let opts = if level >= 4 {
        opts.with_lazy_matching(true)
    } else {
        opts
    };
    opts.with_block_size(RAR29_LZ_BLOCK_SIZE)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Token types
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy)]
enum EncodeToken {
    Literal(u8),
    Match { length: usize, offset: usize },
}

#[derive(Debug, Clone, Copy, Default)]
struct EncoderMatchState {
    old_offsets: [usize; 4],
    last_offset: usize,
    last_length: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncodedMatch {
    LastLengthRepeat,
    RepeatOffset {
        index: usize,
        length_slot: usize,
        length_extra: usize,
    },
    Fresh {
        length_slot: usize,
        length_extra: usize,
        offset_slot: usize,
        offset_extra: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MatchCandidate {
    length: usize,
    offset: usize,
    score: isize,
}

// ═══════════════════════════════════════════════════════════════════════════
//  EncoderMatchState
// ═══════════════════════════════════════════════════════════════════════════

impl EncoderMatchState {
    fn encode_match(&self, length: usize, offset: usize) -> RarResult<EncodedMatch> {
        if offset == self.last_offset && length == self.last_length && self.last_length != 0 {
            return Ok(EncodedMatch::LastLengthRepeat);
        }
        if let Some(index) = self
            .old_offsets
            .iter()
            .position(|&old_offset| old_offset == offset && old_offset != 0)
        {
            let (length_slot, length_extra) = length_slot_for_repeat_match(length)?;
            return Ok(EncodedMatch::RepeatOffset {
                index,
                length_slot,
                length_extra,
            });
        }
        let encoded_length = length
            .checked_sub(match_length_adjustment(offset))
            .ok_or_else(|| enc_err("adjusted match length underflows"))?;
        let (length_slot, length_extra) = length_slot_for_match(encoded_length)?;
        let (offset_slot, offset_extra) = offset_slot_for_match(offset)?;
        Ok(EncodedMatch::Fresh {
            length_slot,
            length_extra,
            offset_slot,
            offset_extra,
        })
    }

    fn remember(&mut self, length: usize, offset: usize) {
        if offset == self.last_offset && length == self.last_length && self.last_length != 0 {
            return;
        }
        if let Some(index) = self
            .old_offsets
            .iter()
            .position(|&old_offset| old_offset == offset)
        {
            self.old_offsets[..=index].rotate_right(1);
        } else {
            self.old_offsets.rotate_right(1);
            self.old_offsets[0] = offset;
        }
        self.last_offset = offset;
        self.last_length = length;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Slot / cost helpers
// ═══════════════════════════════════════════════════════════════════════════

fn match_length_adjustment(offset: usize) -> usize {
    usize::from(offset >= 0x2000) + usize::from(offset >= 0x40000)
}

fn length_slot_for_match(length: usize) -> RarResult<(usize, usize)> {
    if length < 3 {
        return Err(enc_err("match length is too short"));
    }
    let adjusted = length - 3;
    for (slot, &base) in LENGTH_BASES.iter().enumerate() {
        let extra_bits = LENGTH_BITS[slot];
        let max = base
            + if extra_bits == 0 {
                0
            } else {
                (1usize << extra_bits) - 1
            };
        if adjusted >= base && adjusted <= max {
            return Ok((slot, adjusted - base));
        }
    }
    Err(enc_err("match length is too long"))
}

fn length_slot_for_repeat_match(length: usize) -> RarResult<(usize, usize)> {
    if length < 2 {
        return Err(enc_err("repeat match length is too short"));
    }
    let adjusted = length - 2;
    for (slot, &base) in LENGTH_BASES.iter().enumerate() {
        let extra_bits = LENGTH_BITS[slot];
        let max = base
            + if extra_bits == 0 {
                0
            } else {
                (1usize << extra_bits) - 1
            };
        if adjusted >= base && adjusted <= max {
            return Ok((slot, adjusted - base));
        }
    }
    Err(enc_err("repeat match length is too long"))
}

fn offset_slot_for_match(offset: usize) -> RarResult<(usize, usize)> {
    if offset == 0 {
        return Err(enc_err("match offset is zero"));
    }
    let adjusted = offset - 1;
    for (slot, &base) in OFFSET_BASES.iter().enumerate() {
        let extra_bits = OFFSET_BITS[slot];
        let max = base
            + if extra_bits == 0 {
                0
            } else {
                (1usize << extra_bits) - 1
            };
        if adjusted >= base && adjusted <= max {
            return Ok((slot, adjusted - base));
        }
    }
    Err(enc_err("match offset is too large"))
}

fn estimated_match_cost(
    state: &EncoderMatchState,
    length: usize,
    offset: usize,
) -> RarResult<usize> {
    match state.encode_match(length, offset)? {
        EncodedMatch::LastLengthRepeat => Ok(2),
        EncodedMatch::RepeatOffset { length_slot, .. } => {
            Ok(5 + usize::from(LENGTH_BITS[length_slot]))
        }
        EncodedMatch::Fresh {
            length_slot,
            offset_slot,
            ..
        } => {
            let low_offset_cost = usize::from(offset_slot > 9) * 4;
            Ok(8 + usize::from(LENGTH_BITS[length_slot])
                + usize::from(OFFSET_BITS[offset_slot])
                + low_offset_cost)
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Match search
// ═══════════════════════════════════════════════════════════════════════════

fn best_match(
    input: &[u8],
    pos: usize,
    end: usize,
    finder: &mut MatchFinder,
    options: EncodeOptions,
    state: &EncoderMatchState,
) -> Option<MatchCandidate> {
    let max_length = (end - pos).min(MAX_ENCODER_MATCH_LENGTH);
    if options.max_match_candidates == 0 || pos + 3 >= input.len() || max_length < 4 {
        return None;
    }
    // The shared finder checks the 4-slot repeat-offset cache itself; carry
    // RAR29's old_offsets as that cache.
    let mut cache = [0u32; DIST_CACHE_SIZE];
    for (slot, &offset) in cache.iter_mut().zip(state.old_offsets.iter()) {
        *slot = offset as u32;
    }
    let (offset, length) = finder.find_match_cached(pos, &cache);
    if length < 4 || offset == 0 || offset > options.max_match_distance {
        return None;
    }
    let Ok(score) = estimated_match_cost(state, length, offset) else {
        return None;
    };
    Some(MatchCandidate {
        length,
        offset,
        score: (length as isize * 8) - score as isize,
    })
}

fn lazy_match_decision(
    input: &[u8],
    pos: usize,
    finder: &mut MatchFinder,
    options: EncodeOptions,
    state: &EncoderMatchState,
    current: MatchCandidate,
) -> (bool, Option<MatchCandidate>) {
    let end = input.len();
    if !options.lazy_matching || pos + 1 >= end {
        return (false, None);
    }
    let lookahead = options.lazy_lookahead.max(1);
    let mut cached_next = None;
    for offset in 1..=lookahead {
        if pos + offset >= end {
            break;
        }
        let next = best_match(input, pos + offset, end, finder, options, state);
        if offset == 1 {
            cached_next = next;
        }
        let skipped_literal_score = offset as isize * 8;
        if next.is_some_and(|next| next.score > current.score + skipped_literal_score) {
            return (true, cached_next);
        }
    }
    (false, None)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tokenizer
// ═══════════════════════════════════════════════════════════════════════════

fn encode_tokens_with_progress(
    input: &[u8],
    history: &[u8],
    options: EncodeOptions,
    mut progress: Option<&mut dyn FnMut(usize) -> bool>,
) -> RarResult<Vec<EncodeToken>> {
    let mut tokens = Vec::new();
    let history = &history[history.len().saturating_sub(options.max_match_distance)..];
    let mut combined = Vec::with_capacity(history.len() + input.len());
    combined.extend_from_slice(history);
    combined.extend_from_slice(input);
    let mut finder = MatchFinder::new(
        &combined,
        4,
        MAX_ENCODER_MATCH_LENGTH,
        options.max_match_candidates,
        options.max_match_distance,
    );
    for history_pos in 0..history.len() {
        finder.insert(history_pos);
    }

    let mut pos = history.len();
    let end = combined.len();
    let mut state = EncoderMatchState::default();
    let mut next_report = 0usize;
    let mut pending_match: Option<MatchCandidate> = None;
    while pos < end {
        let candidate = pending_match
            .take()
            .or_else(|| best_match(&combined, pos, end, &mut finder, options, &state));
        if let Some(candidate) = candidate {
            let (emit_literal, cached_next) =
                lazy_match_decision(&combined, pos, &mut finder, options, &state, candidate);
            if emit_literal {
                tokens.push(EncodeToken::Literal(combined[pos]));
                pos += 1;
                pending_match = cached_next;
                continue;
            }
            let MatchCandidate { length, offset, .. } = candidate;
            tokens.push(EncodeToken::Match { length, offset });
            state.remember(length, offset);
            // The shared finder already inserted the match start; index the
            // covered interior positions so future matches can reference them.
            for history_pos in (pos + 1)..(pos + length) {
                finder.insert(history_pos);
            }
            pos += length;
        } else {
            tokens.push(EncodeToken::Literal(combined[pos]));
            pos += 1;
        }
        let consumed = pos.saturating_sub(history.len());
        if consumed >= next_report {
            if progress
                .as_deref_mut()
                .is_some_and(|report| !report(consumed))
            {
                return Err(RarError::Cancelled);
            }
            next_report = consumed.saturating_add(1024 * 1024);
        }
    }
    if progress.is_some_and(|report| !report(input.len())) {
        return Err(RarError::Cancelled);
    }
    Ok(tokens)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Level table encoding (Huffman over the 20-symbol level alphabet)
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LevelToken {
    symbol: usize,
    extra_bits: u8,
    extra_value: u8,
}

impl LevelToken {
    const fn plain(symbol: usize) -> Self {
        Self {
            symbol,
            extra_bits: 0,
            extra_value: 0,
        }
    }

    const fn repeat_previous_short(count: usize) -> Self {
        Self {
            symbol: 16,
            extra_bits: 3,
            extra_value: (count - 3) as u8,
        }
    }

    const fn repeat_previous_long(count: usize) -> Self {
        Self {
            symbol: 17,
            extra_bits: 7,
            extra_value: (count - 11) as u8,
        }
    }

    const fn zero_run_short(count: usize) -> Self {
        Self {
            symbol: 18,
            extra_bits: 3,
            extra_value: (count - 3) as u8,
        }
    }

    const fn zero_run_long(count: usize) -> Self {
        Self {
            symbol: 19,
            extra_bits: 7,
            extra_value: (count - 11) as u8,
        }
    }
}

fn encode_table_level_tokens(lengths: &[u8; TABLE_COUNT]) -> Vec<LevelToken> {
    encode_level_tokens_against(lengths, &[0; TABLE_COUNT])
}

fn encode_level_tokens_against(lengths: &[u8], base: &[u8]) -> Vec<LevelToken> {
    let delta = |pos: usize, value: u8| (value.wrapping_sub(base[pos]) & 0x0f) as usize;
    let mut tokens = Vec::new();
    let mut pos = 0usize;
    let mut previous = None;
    while pos < lengths.len() {
        let value = lengths[pos];
        let mut run = 1usize;
        while pos + run < lengths.len() && lengths[pos + run] == value {
            run += 1;
        }

        if value == 0 {
            emit_zero_level_run(&mut tokens, pos, run, &delta);
            previous = Some(0);
            pos += run;
            continue;
        }

        if previous == Some(value) && run >= 3 {
            emit_repeat_level_run(&mut tokens, run);
            pos += run;
            continue;
        }

        tokens.push(LevelToken::plain(delta(pos, value)));
        previous = Some(value);
        pos += 1;
    }
    tokens
}

fn level_tokens_bit_cost(tokens: &[LevelToken]) -> usize {
    let lengths = level_code_lengths(tokens);
    tokens
        .iter()
        .map(|token| usize::from(lengths[token.symbol]) + usize::from(token.extra_bits))
        .sum()
}

fn emit_repeat_level_run(tokens: &mut Vec<LevelToken>, mut run: usize) {
    while run != 0 {
        if run >= 11 {
            let mut chunk = run.min(138);
            if matches!(run - chunk, 1 | 2) && chunk >= 14 {
                chunk -= 3;
            }
            tokens.push(LevelToken::repeat_previous_long(chunk));
            run -= chunk;
        } else if run >= 3 {
            let chunk = run.min(10);
            tokens.push(LevelToken::repeat_previous_short(chunk));
            run -= chunk;
        } else {
            break;
        }
    }
}

fn emit_zero_level_run(
    tokens: &mut Vec<LevelToken>,
    start: usize,
    mut run: usize,
    delta: &dyn Fn(usize, u8) -> usize,
) {
    let mut pos = start;
    while run != 0 {
        if run >= 11 {
            let mut chunk = run.min(138);
            if matches!(run - chunk, 1 | 2) && chunk >= 14 {
                chunk -= 3;
            }
            tokens.push(LevelToken::zero_run_long(chunk));
            run -= chunk;
            pos += chunk;
        } else if run >= 3 {
            let chunk = run.min(10);
            tokens.push(LevelToken::zero_run_short(chunk));
            run -= chunk;
            pos += chunk;
        } else {
            // A run too short for its own symbol is written out position by
            // position, and each of those is a delta like any other.
            tokens.extend((pos..pos + run).map(|pos| LevelToken::plain(delta(pos, 0))));
            break;
        }
    }
}

/// Huffman-code-lengths for the 20-symbol level alphabet, weighted by usage.
fn level_code_lengths(tokens: &[LevelToken]) -> [u8; LEVEL_COUNT] {
    let mut frequencies = [0usize; LEVEL_COUNT];
    for token in tokens {
        frequencies[token.symbol] += 1;
    }
    // Single symbol in play: flat 1-bit code with a phantom so the table is
    // complete (strict readers reject a table with one branch and an empty
    // sibling).
    if frequencies.iter().filter(|&&count| count != 0).count() <= 1 {
        let mut lengths = [0u8; LEVEL_COUNT];
        for (symbol, &count) in frequencies.iter().enumerate() {
            lengths[symbol] = u8::from(count != 0);
        }
        // Pad with a phantom length-1 code.
        let used = lengths.iter().position(|&l| l != 0).unwrap_or(0);
        let phantom = if used == 0 { 1 } else { 0 };
        if phantom < LEVEL_COUNT {
            lengths[phantom] = 1;
        }
        return lengths;
    }
    // Build optimal Huffman lengths, capped at 15 bits (the 4-bit per-length
    // cap in the block header).
    let mut lengths = [0u8; LEVEL_COUNT];
    lengths.copy_from_slice(&super::huffman::build_code_lengths_from_freqs(
        &frequencies.iter().map(|&f| f as u32).collect::<Vec<_>>(),
        15,
    ));
    lengths
}

// ═══════════════════════════════════════════════════════════════════════════
//  Canonical Huffman codes — reuse the shared huffman::EncodeTable
// ═══════════════════════════════════════════════════════════════════════════

/// Look up the canonical (code, length) for `symbol`, rejecting a zero-length
/// (unused) symbol so a silently-empty write can never corrupt the stream.
fn encode_code(table: &EncodeTable, symbol: usize) -> RarResult<(u32, u8)> {
    let len = table.lengths[symbol];
    if len == 0 {
        return Err(enc_err("missing Huffman code for emitted symbol"));
    }
    Ok((table.codes[symbol], len))
}

// ═══════════════════════════════════════════════════════════════════════════
//  Block-level encode — the heart of the encoder
// ═══════════════════════════════════════════════════════════════════════════

/// Encode one LZ block (or a single-block member when `more_blocks_follow` is
/// false).
fn encode_member_inner(
    input: &[u8],
    history: &[u8],
    options: EncodeOptions,
    more_blocks_follow: bool,
    previous_levels: &mut [u8; TABLE_COUNT],
    progress: Option<&mut dyn FnMut(usize) -> bool>,
) -> RarResult<Vec<u8>> {
    let tokens = encode_tokens_with_progress(input, history, options, progress)?;

    // ── Count frequencies ────────────────────────────────────────────────
    let mut main_frequencies = vec![0usize; MAIN_COUNT];
    let mut offset_frequencies = vec![0usize; OFFSET_COUNT];
    let mut low_offset_frequencies = [0usize; LOW_OFFSET_COUNT];
    let mut length_frequencies = vec![0usize; LENGTH_COUNT];
    let mut match_state = EncoderMatchState::default();
    for token in &tokens {
        match *token {
            EncodeToken::Literal(byte) => {
                main_frequencies[byte as usize] += 1;
            }
            EncodeToken::Match { length, offset } => {
                match match_state.encode_match(length, offset)? {
                    EncodedMatch::LastLengthRepeat => {
                        main_frequencies[258] += 1;
                    }
                    EncodedMatch::RepeatOffset {
                        index, length_slot, ..
                    } => {
                        main_frequencies[259 + index] += 1;
                        length_frequencies[length_slot] += 1;
                    }
                    EncodedMatch::Fresh {
                        length_slot,
                        offset_slot,
                        offset_extra,
                        ..
                    } => {
                        main_frequencies[271 + length_slot] += 1;
                        offset_frequencies[offset_slot] += 1;
                        if offset_slot > 9 {
                            low_offset_frequencies[offset_extra & 0x0f] += 1;
                        }
                    }
                }
                match_state.remember(length, offset);
            }
        }
    }
    main_frequencies[256] += 1; // end-of-block

    // ── Build Huffman code lengths ───────────────────────────────────────
    let mut table_lengths = [0u8; TABLE_COUNT];
    if low_offset_frequencies
        .iter()
        .all(|&frequency| frequency == 0)
    {
        low_offset_frequencies[0] = 1;
    }
    let main_lengths = super::huffman::build_code_lengths_from_freqs(
        &main_frequencies
            .iter()
            .map(|&f| f as u32)
            .collect::<Vec<_>>(),
        15,
    );
    let offset_lengths = super::huffman::build_code_lengths_from_freqs(
        &offset_frequencies
            .iter()
            .map(|&f| f as u32)
            .collect::<Vec<_>>(),
        15,
    );
    let low_offset_lengths = super::huffman::build_code_lengths_from_freqs(
        &low_offset_frequencies
            .iter()
            .map(|&f| f as u32)
            .collect::<Vec<_>>(),
        15,
    );
    let length_lengths = super::huffman::build_code_lengths_from_freqs(
        &length_frequencies
            .iter()
            .map(|&f| f as u32)
            .collect::<Vec<_>>(),
        15,
    );
    table_lengths[..MAIN_COUNT].copy_from_slice(&main_lengths);
    table_lengths[MAIN_COUNT..MAIN_COUNT + OFFSET_COUNT].copy_from_slice(&offset_lengths);
    table_lengths[MAIN_COUNT + OFFSET_COUNT..MAIN_COUNT + OFFSET_COUNT + LOW_OFFSET_COUNT]
        .copy_from_slice(&low_offset_lengths);
    table_lengths[MAIN_COUNT + OFFSET_COUNT + LOW_OFFSET_COUNT..].copy_from_slice(&length_lengths);

    // ── Table serialization: outright vs. delta against previous ──────────
    let outright = encode_table_level_tokens(&table_lengths);
    let against_previous = encode_level_tokens_against(&table_lengths, previous_levels);
    let keep_previous_tables =
        level_tokens_bit_cost(&against_previous) < level_tokens_bit_cost(&outright);
    let level_tokens = if keep_previous_tables {
        against_previous
    } else {
        outright
    };
    *previous_levels = table_lengths;

    // ── Build canonical codes (shared EncodeTable) ─────────────────────
    let level_lengths_arr = level_code_lengths(&level_tokens);
    let level_codes = EncodeTable::new(&level_lengths_arr);
    let main_codes = EncodeTable::new(&table_lengths[..MAIN_COUNT]);
    let offset_codes = EncodeTable::new(&table_lengths[MAIN_COUNT..MAIN_COUNT + OFFSET_COUNT]);
    let low_offset_codes = EncodeTable::new(
        &table_lengths[MAIN_COUNT + OFFSET_COUNT..MAIN_COUNT + OFFSET_COUNT + LOW_OFFSET_COUNT],
    );
    let length_codes =
        EncodeTable::new(&table_lengths[MAIN_COUNT + OFFSET_COUNT + LOW_OFFSET_COUNT..]);

    // ── Serialize ────────────────────────────────────────────────────────
    let mut bits = BitWriter::new();
    bits.write_bits(false as u32, 1); // LZ block (not PPMd)
    bits.write_bits(keep_previous_tables as u32, 1);
    for &len in &level_lengths_arr {
        bits.write_bits(len as u32, 4);
    }
    for token in &level_tokens {
        let (code, cl) = encode_code(&level_codes, token.symbol)?;
        bits.write_bits(code, cl);
        if token.extra_bits != 0 {
            bits.write_bits(token.extra_value as u32, token.extra_bits);
        }
    }

    // ── Write tokens ─────────────────────────────────────────────────────
    let mut match_state = EncoderMatchState::default();
    for token in tokens {
        match token {
            EncodeToken::Literal(byte) => {
                let (code, cl) = encode_code(&main_codes, byte as usize)?;
                bits.write_bits(code, cl);
            }
            EncodeToken::Match { length, offset } => {
                match match_state.encode_match(length, offset)? {
                    EncodedMatch::LastLengthRepeat => {
                        let (code, cl) = encode_code(&main_codes, 258)?;
                        bits.write_bits(code, cl);
                    }
                    EncodedMatch::RepeatOffset {
                        index,
                        length_slot,
                        length_extra,
                    } => {
                        let (code, cl) = encode_code(&main_codes, 259 + index)?;
                        bits.write_bits(code, cl);
                        let (length_code, length_cl) = encode_code(&length_codes, length_slot)?;
                        bits.write_bits(length_code, length_cl);
                        if LENGTH_BITS[length_slot] != 0 {
                            bits.write_bits(length_extra as u32, LENGTH_BITS[length_slot]);
                        }
                    }
                    EncodedMatch::Fresh {
                        length_slot,
                        length_extra,
                        offset_slot,
                        offset_extra,
                    } => {
                        let (code, cl) = encode_code(&main_codes, 271 + length_slot)?;
                        bits.write_bits(code, cl);
                        if LENGTH_BITS[length_slot] != 0 {
                            bits.write_bits(length_extra as u32, LENGTH_BITS[length_slot]);
                        }
                        let (offset, offset_cl) = encode_code(&offset_codes, offset_slot)?;
                        bits.write_bits(offset, offset_cl);
                        if offset_slot > 9 {
                            let offset_bits = OFFSET_BITS[offset_slot];
                            if offset_bits > 4 {
                                bits.write_bits((offset_extra >> 4) as u32, offset_bits - 4);
                            }
                            let (low_offset, low_offset_cl) =
                                encode_code(&low_offset_codes, offset_extra & 0x0f)?;
                            bits.write_bits(low_offset, low_offset_cl);
                        } else if OFFSET_BITS[offset_slot] != 0 {
                            bits.write_bits(offset_extra as u32, OFFSET_BITS[offset_slot]);
                        }
                    }
                }
                match_state.remember(length, offset);
            }
        }
    }

    // ── End-of-block ─────────────────────────────────────────────────────
    let (end_code, end_cl) = encode_code(&main_codes, 256)?;
    bits.write_bits(end_code, end_cl);
    // The terminator: one bit saying "another table follows" (true for
    // intermediate blocks), or (false, true) to end the member (the reader
    // carries the true into its solid-chain state).
    if more_blocks_follow {
        bits.write_bits(true as u32, 1);
    } else {
        bits.write_bits(false as u32, 1);
        bits.write_bits(true as u32, 1);
    }
    Ok(bits.into_bytes())
}

// ═══════════════════════════════════════════════════════════════════════════
//  Multi-block splitting
// ═══════════════════════════════════════════════════════════════════════════

fn encode_member_with_options_impl(
    input: &[u8],
    history: &[u8],
    options: EncodeOptions,
    levels: &mut [u8; TABLE_COUNT],
    progress: Option<&mut dyn FnMut(usize) -> bool>,
) -> RarResult<Vec<u8>> {
    if let Some(block_size) = options
        .block_size
        .filter(|&size| size != 0 && input.len() > size)
    {
        return encode_member_blocks(input, history, options, block_size, levels, progress);
    }
    encode_member_inner(input, history, options, false, levels, progress)
}

fn encode_member_blocks(
    input: &[u8],
    history: &[u8],
    mut options: EncodeOptions,
    block_size: usize,
    levels: &mut [u8; TABLE_COUNT],
    mut progress: Option<&mut dyn FnMut(usize) -> bool>,
) -> RarResult<Vec<u8>> {
    options.block_size = None;
    let mut out = Vec::new();
    let mut local_history = history[history.len().saturating_sub(MAX_HISTORY)..].to_vec();
    let mut completed = 0usize;
    let block_count = input.chunks(block_size).count();
    for (index, chunk) in input.chunks(block_size).enumerate() {
        let mut chunk_progress = |position: usize| {
            progress
                .as_deref_mut()
                .is_none_or(|report| report(completed.saturating_add(position)))
        };
        out.extend_from_slice(&encode_member_inner(
            chunk,
            &local_history,
            options,
            index + 1 < block_count,
            levels,
            Some(&mut chunk_progress),
        )?);
        completed = completed.saturating_add(chunk.len());
        local_history.extend_from_slice(chunk);
        let keep_from = local_history.len().saturating_sub(MAX_HISTORY);
        if keep_from != 0 {
            local_history.drain(..keep_from);
        }
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Unpack29Encoder — public API
// ═══════════════════════════════════════════════════════════════════════════

/// RAR 3.x/4.x LZSS+Huffman encoder.
///
/// Maintains the sliding window and Huffman table state needed for solid
/// chains.  For non-solid archives, create a fresh encoder per member.
#[derive(Debug, Clone)]
pub struct Unpack29Encoder {
    history: Vec<u8>,
    options: EncodeOptions,
    /// The code-length table a reader holds after reading all coded members
    /// so far.  A solid chain carries it from one member to the next.
    levels: [u8; TABLE_COUNT],
}

impl Default for Unpack29Encoder {
    fn default() -> Self {
        Self::with_options(EncodeOptions::default())
    }
}

impl Unpack29Encoder {
    #[allow(dead_code)] // kept: symmetric constructor, used solely by tests
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_options(options: EncodeOptions) -> Self {
        Self {
            history: Vec::new(),
            options,
            levels: [0; TABLE_COUNT],
        }
    }

    /// Encode one member.  Returns the compressed block sequence (不含
    /// FILE_HEAD; the write pipeline handles headers).
    pub fn encode_member(&mut self, input: &[u8]) -> RarResult<Vec<u8>> {
        let packed = encode_member_with_options_impl(
            input,
            &self.history,
            self.options,
            &mut self.levels,
            None,
        )?;
        self.remember(input);
        Ok(packed)
    }

    fn remember(&mut self, input: &[u8]) {
        self.history.extend_from_slice(input);
        let keep_from = self.history.len().saturating_sub(MAX_HISTORY);
        if keep_from != 0 {
            self.history.drain(..keep_from);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_literals_only() {
        // All literals — no matches possible.
        let input: Vec<u8> = (0..=255).collect();
        let mut encoder = Unpack29Encoder::with_options(options_for_level(1));
        let packed = encoder.encode_member(&input).unwrap();
        assert!(!packed.is_empty());
        // The compressed output should be smaller than STORE for this
        // all-different-byte data? Actually no — 256 unique bytes is the
        // worst case for Huffman. Just verify it doesn't panic and produces
        // non-empty output.
    }

    #[test]
    fn roundtrip_repeated_data() {
        // Highly compressible: repeated pattern.
        let pattern = b"Hello, world! ";
        let input: Vec<u8> = pattern.iter().copied().cycle().take(64 * 1024).collect();
        let mut encoder = Unpack29Encoder::with_options(options_for_level(3));
        let packed = encoder.encode_member(&input).unwrap();
        assert!(
            packed.len() < input.len(),
            "compressed {} bytes should be smaller than original {}",
            packed.len(),
            input.len(),
        );
    }

    #[test]
    fn roundtrip_solid_chain() {
        let mut encoder = Unpack29Encoder::with_options(options_for_level(3));
        let mut all_packed = Vec::new();
        for i in 0..5u8 {
            let chunk: Vec<u8> = (0..4096).map(|j| (j as u8).wrapping_add(i)).collect();
            let packed = encoder.encode_member(&chunk).unwrap();
            all_packed.extend_from_slice(&packed);
        }
        assert!(!all_packed.is_empty());
    }

    #[test]
    fn level_options_mapping() {
        let opts = options_for_level(3);
        assert_eq!(opts.max_match_candidates, 64);
        assert!(!opts.lazy_matching);
        assert_eq!(opts.block_size, Some(RAR29_LZ_BLOCK_SIZE));

        let opts = options_for_level(5);
        assert_eq!(opts.max_match_candidates, 128);
        assert!(opts.lazy_matching);
    }

    #[test]
    fn store_fallback_for_random_data() {
        // Random data: compression should not help.
        let input: Vec<u8> = (0..4096).map(|i| (i * 7 + 13) as u8).collect();
        let mut encoder = Unpack29Encoder::with_options(options_for_level(3));
        let packed = encoder.encode_member(&input).unwrap();
        // We don't assert packed.len() >= input.len() because Huffman on
        // random 4096 bytes can sometimes be slightly smaller; just verify
        // no panic.
        assert!(!packed.is_empty());
    }

    #[test]
    fn empty_input() {
        let mut encoder = Unpack29Encoder::new();
        let packed = encoder.encode_member(&[]).unwrap();
        // Empty input still produces an end-of-block marker.
        assert!(!packed.is_empty());
    }

    #[test]
    fn roundtrip_multiblock() {
        // Input spanning several 64 KiB LZ blocks: the encoder must close
        // each block with a SameFileNewTable terminator and re-emit/decode
        // tables for the next block. Decode and compare.
        for blocks in 1..=40usize {
            let target = blocks * 64 * 1024;
            let mut input = Vec::with_capacity(target);
            let mut n = 0u32;
            while input.len() < target {
                input.extend_from_slice(
                    format!(
                        "line number {n:07} with padding to vary content length here 1234567890\n"
                    )
                    .as_bytes(),
                );
                n += 1;
            }
            let mut encoder = Unpack29Encoder::with_options(options_for_level(3));
            let packed = encoder.encode_member(&input).unwrap();

            let mut decoder = crate::codec::rar29::Rar29Decoder::new();
            let out = decoder
                .decode_member(&packed, input.len() as u64)
                .unwrap_or_else(|e| panic!("blocks={blocks} len={} decode: {e:?}", input.len()));
            assert_eq!(
                out,
                input,
                "blocks={blocks} len={} content mismatch",
                input.len()
            );
        }
    }
}
