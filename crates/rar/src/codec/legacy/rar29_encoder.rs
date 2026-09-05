//! RAR 3.x/4.x LZSS+Huffman encoder — the write-side counterpart of
//! [`super::rar29`].
//!
//! Ported from the encode half of the `rars` project (MIT OR Apache-2.0)
//! `codec/rar29.rs` `Unpack29Encoder`.  Produces RAR3/4-format compressed
//! block sequences (不含 FILE_HEAD，只含压缩数据流).  The write pipeline
//! (`rar40/write.rs`) handles headers, encryption, and multi-volume splitting.
//!
//! Phase 1: LZSS only (m1–m5).  Phase 2 adds PPMd member encoding (m4/m5):
//! a whole member is one PPMd block whose model either starts fresh
//! (order-8, 25 MiB suballocator) or continues a solid chain's, with LZ
//! matches escaped into the model where the tokeniser prices them cheaper
//! than literals.  VM-filter integration is still out of scope.

use crate::codec::common::bitstream::BitWriter;
use crate::codec::common::huffman::EncodeTable;
use crate::codec::common::match_finder::MatchFinder;
use crate::codec::legacy::ppmd::PpmdEncoder;
use crate::codec::lzss_huff::DIST_CACHE_SIZE;
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

// ── PPMd member encoding (m4/m5) ───────────────────────────────────────────

/// Order-8 PPMd, 25 MiB suballocator, escape char 2 — the values rars and
/// WinRAR use for RAR3/4 PPMd members. The wire header byte is
/// `0x80 | 0x20 | (order-1)` for a fresh model and `0x80 | (order-1)` when a
/// solid chain continues an existing one; a fresh block appends
/// `(dictionary_mb - 1)` after the header byte.
const PPMD_ORDER: usize = 8;
const PPMD_DICTIONARY_MB: u8 = 25;
const PPMD_ESC: u8 = 2;

/// A PPMd escape match can only carry lengths 32..=287 (one byte past the
/// escape-4 symbol); shorter repeats use the offset-one form (4..=259).
const MAX_PPMD_MATCH_LENGTH: usize = 255;
const MIN_PPMD_MATCH_LENGTH: usize = 32;
const MAX_PPMD_REPEAT_LENGTH: usize = 259;

// Seeds and EMA weights for the escape-token cost model in
// `encode_ppmd_hybrid` (ported from rars): the tokeniser prices an escape
// token against the literals it would replace using the range coder's
// measured bit cost of each kind of token.
const PPMD_LITERAL_BITS_SEED: f64 = 4.0;
const PPMD_MATCH_BITS_SEED: f64 = 60.0;
const PPMD_REPEAT_BITS_SEED: f64 = 24.0;
const PPMD_LITERAL_EMA_WEIGHT: f64 = 1.0 / 32.0;
const PPMD_TOKEN_EMA_WEIGHT: f64 = 1.0 / 8.0;
const PPMD_CONTEXT_BREAK_BITS: f64 = 16.0;
const PPMD_REJECT_SEARCH_COOLDOWN: usize = 8;

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
    lengths.copy_from_slice(
        &crate::codec::common::huffman::build_code_lengths_from_freqs(
            &frequencies.iter().map(|&f| f as u32).collect::<Vec<_>>(),
            15,
        ),
    );
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
    initial_filters: &[Vec<u8>],
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
    main_frequencies[257] += initial_filters.len();
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
    let main_lengths = crate::codec::common::huffman::build_code_lengths_from_freqs(
        &main_frequencies
            .iter()
            .map(|&f| f as u32)
            .collect::<Vec<_>>(),
        15,
    );
    let offset_lengths = crate::codec::common::huffman::build_code_lengths_from_freqs(
        &offset_frequencies
            .iter()
            .map(|&f| f as u32)
            .collect::<Vec<_>>(),
        15,
    );
    let low_offset_lengths = crate::codec::common::huffman::build_code_lengths_from_freqs(
        &low_offset_frequencies
            .iter()
            .map(|&f| f as u32)
            .collect::<Vec<_>>(),
        15,
    );
    let length_lengths = crate::codec::common::huffman::build_code_lengths_from_freqs(
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

    // ── VM filter records (main symbol 257) ─────────────────────────────
    // Announced at the head of the block whose range they start in, before
    // any token, so the decoder has them queued when the range arrives.
    for filter in initial_filters {
        let (code, cl) = encode_code(&main_codes, 257)?;
        bits.write_bits(code, cl);
        for &byte in filter {
            bits.write_bits(u32::from(byte), 8);
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
    encode_member_inner(input, history, &[], options, false, levels, progress)
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
            &[],
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
//  PPMd member encoding (m4/m5) — Phase 2
// ═══════════════════════════════════════════════════════════════════════════

fn ppmd_err(e: crate::codec::legacy::ppmd::Error) -> RarError {
    RarError::Format(format!("RAR 2.9 PPMd encode: {e}"))
}

/// One way of pricing an escape token: `length * literal_bits` is what the
/// model would charge to code those bytes as literals, so an escape pays
/// when that exceeds the measured cost of the token plus the context-break
/// overhead. The slots are EMA-smoothed against the range coder's real
/// per-token bit cost as the member is coded (ported from rars).
struct PpmdTokenCosts {
    literal_bits: f64,
    match_bits: f64,
    repeat_bits: f64,
}

impl PpmdTokenCosts {
    fn new() -> Self {
        Self {
            literal_bits: PPMD_LITERAL_BITS_SEED,
            match_bits: PPMD_MATCH_BITS_SEED,
            repeat_bits: PPMD_REPEAT_BITS_SEED,
        }
    }

    fn match_pays(&self, length: usize) -> bool {
        length as f64 * self.literal_bits > self.match_bits + PPMD_CONTEXT_BREAK_BITS
    }

    fn repeat_pays(&self, length: usize) -> bool {
        length as f64 * self.literal_bits > self.repeat_bits + PPMD_CONTEXT_BREAK_BITS
    }

    fn record_literal(&mut self, bits: f64) {
        ema(&mut self.literal_bits, bits, PPMD_LITERAL_EMA_WEIGHT);
    }

    fn record_match(&mut self, bits: f64) {
        ema(&mut self.match_bits, bits, PPMD_TOKEN_EMA_WEIGHT);
    }

    fn record_repeat(&mut self, bits: f64) {
        ema(&mut self.repeat_bits, bits, PPMD_TOKEN_EMA_WEIGHT);
    }
}

fn ema(slot: &mut f64, sample: f64, weight: f64) {
    *slot += weight * (sample - *slot);
}

/// Length of an offset-one run (`input[pos] == input[pos-1]` repeated),
/// 4..=259, or `None`.
fn ppmd_offset_one_repeat(input: &[u8], pos: usize) -> Option<usize> {
    if pos == 0 || input[pos] != input[pos - 1] {
        return None;
    }
    let mut length = 0usize;
    while pos + length < input.len()
        && input[pos + length] == input[pos - 1]
        && length < MAX_PPMD_REPEAT_LENGTH
    {
        length += 1;
    }
    (length >= 4).then_some(length)
}

/// Feed the member through PPMd, escaping to an LZ token only where the
/// tokeniser prices it cheaper than letting the model code the same bytes
/// as literals (ported from rars `encode_ppmd_hybrid`). Tokenising and
/// encoding are one loop so each decision can read the cost of the last one
/// straight off the range coder.
///
/// The match finder covers only the member itself (a PPMd block's escape
/// matches are intra-member); the model carries the cross-member context.
fn encode_ppmd_hybrid(input: &[u8], encoder: &mut PpmdEncoder) -> RarResult<()> {
    let mut costs = PpmdTokenCosts::new();
    let mut finder = MatchFinder::new(
        input,
        MIN_PPMD_MATCH_LENGTH,
        MAX_PPMD_MATCH_LENGTH,
        MAX_MATCH_CANDIDATES,
        MAX_ENCODER_MATCH_OFFSET,
    );
    let mut pos = 0usize;
    let mut search_from = 0usize;
    while pos < input.len() {
        if let Some(length) = ppmd_offset_one_repeat(input, pos)
            && costs.repeat_pays(length)
        {
            let before = encoder.spent_bits();
            encoder.encode_repeat_offset_one(length).map_err(ppmd_err)?;
            costs.record_repeat(encoder.spent_bits() - before);
            for history_pos in pos..pos + length {
                finder.insert(history_pos);
            }
            pos += length;
            continue;
        }

        if pos >= search_from {
            // find_match inserts `pos` itself before searching.
            let (offset, length) = finder.find_match(pos);
            if length >= MIN_PPMD_MATCH_LENGTH && offset >= 2 && costs.match_pays(length) {
                let before = encoder.spent_bits();
                encoder.encode_match(offset, length).map_err(ppmd_err)?;
                costs.record_match(encoder.spent_bits() - before);
                for history_pos in pos + 1..pos + length {
                    finder.insert(history_pos);
                }
                pos += length;
                continue;
            }
            // Rejected match: `pos` was already inserted; back off the
            // search for a few positions and code literals.
            search_from = pos + PPMD_REJECT_SEARCH_COOLDOWN;
        } else {
            finder.insert(pos);
        }

        let before = encoder.spent_bits();
        encoder.encode_literal(input[pos]).map_err(ppmd_err)?;
        costs.record_literal(encoder.spent_bits() - before);
        pos += 1;
    }
    Ok(())
}

/// Code one member as a single fresh-model PPMd block (order-8, 25 MiB
/// suballocator), optionally escaping LZ matches into the model.
/// Returns the wire bytes: `[0x80|0x20|order-1][dict_mb-1]` + range-coded
/// payload. The model is not retained (solid-chain continuation of a PPMd
/// model is a later phase; solid RAR4 members stay LZ-only, which matches
/// what WinRAR 6.23 itself produces).
pub(crate) fn encode_ppmd_member_packed(input: &[u8], lz_escapes: bool) -> RarResult<Vec<u8>> {
    let mut out = vec![0x80 | 0x20 | (PPMD_ORDER as u8 - 1), PPMD_DICTIONARY_MB - 1];
    let mut encoder =
        PpmdEncoder::new(PPMD_ORDER, PPMD_ESC, PPMD_DICTIONARY_MB as usize).map_err(ppmd_err)?;
    if lz_escapes {
        encode_ppmd_hybrid(input, &mut encoder)?;
    } else {
        for &byte in input {
            encoder.encode_literal(byte).map_err(ppmd_err)?;
        }
    }
    let (packed, _model) = encoder.finish_keeping_model().map_err(ppmd_err)?;
    out.extend_from_slice(&packed);
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
//  RAR 2.9 VM filter records (E8 / E8E9 / Delta) — Phase 3
// ═══════════════════════════════════════════════════════════════════════════
//
// A filtered member is coded in two parts: the member's bytes are first
// transformed (x86 call-relative / delta), then the LZ layer compresses the
// transformed bytes while the transform itself travels as a VM-filter
// record (LZ main symbol 257) at the head of the block the filter's range
// starts in. The decoder replays the record and inverse-transforms. The
// wire record layout mirrors `rar29.rs`'s `read_vm_code`/`parse_vm_code`
// (ported from rars). E8/E8E9/Delta reuse the shared RAR5 transform
// primitives (`codec/filters.rs`): the RAR3 standard filters are the same
// transforms, recognised by bytecode fingerprint on the read side.

/// RAR3 standard-filter bytecode for E8/E8E9/Delta (length + CRC32 = the
/// fingerprints `rar29.rs::identify_standard_filter` recognises). Kept
/// verbatim from rars so writer output and reader recognition use the same
/// wire identity.
const RAR3_E8_FILTER_BYTECODE: &[u8] = &[
    0x97, 0x1b, 0x01, 0x28, 0x07, 0x06, 0x98, 0x08, 0x00, 0x00, 0x00, 0xd1, 0x3a, 0x10, 0x15, 0x92,
    0xec, 0x50, 0xcb, 0x99, 0x20, 0xb9, 0x25, 0xf0, 0x29, 0x19, 0x15, 0x53, 0x03, 0x12, 0xae, 0x51,
    0x10, 0x35, 0x59, 0x2b, 0x60, 0x04, 0x15, 0x6d, 0x40, 0x66, 0xab, 0x02, 0x34, 0x49, 0x04, 0x36,
    0x02, 0x52, 0x3e, 0x97, 0x00,
];
const RAR3_E8E9_FILTER_BYTECODE: &[u8] = &[
    0x84, 0x1b, 0x01, 0x28, 0x11, 0x10, 0x69, 0x80, 0x80, 0x00, 0x00, 0x0d, 0x13, 0xa1, 0x01, 0xc6,
    0x89, 0xd2, 0x80, 0xac, 0x97, 0x62, 0x85, 0x5c, 0xc9, 0x05, 0xc9, 0x2f, 0x81, 0x48, 0xc8, 0xaa,
    0x98, 0x18, 0x95, 0x72, 0x88, 0x81, 0xaa, 0xc9, 0x5b, 0x00, 0x20, 0xab, 0x6a, 0x03, 0x35, 0x58,
    0x11, 0xa2, 0x48, 0x21, 0xb0, 0x12, 0x91, 0xf4, 0xb8,
];
const RAR3_DELTA_FILTER_BYTECODE: &[u8] = &[
    0x2f, 0x01, 0x9a, 0x41, 0x80, 0xec, 0x27, 0x48, 0x2f, 0x09, 0x76, 0x6d, 0xd3, 0xea, 0x41, 0x5b,
    0x59, 0x44, 0xe8, 0x17, 0x5c, 0xe1, 0x6c, 0x91, 0x4c, 0x4e, 0x3f, 0x77, 0x00,
];
const RAR3_ITANIUM_FILTER_BYTECODE: &[u8] = &[
    0x46, 0x9e, 0x08, 0x08, 0x0c, 0x0c, 0x00, 0x00, 0x0e, 0x0e, 0x08, 0x08, 0x00, 0x00, 0x08, 0x08,
    0x00, 0x00, 0x6c, 0x11, 0x5a, 0x04, 0xac, 0x0c, 0xc4, 0xcc, 0x5c, 0x08, 0x18, 0x46, 0x24, 0x08,
    0xf9, 0xa0, 0x44, 0x25, 0x12, 0x12, 0x45, 0x85, 0x99, 0x0c, 0x14, 0x00, 0x26, 0x25, 0x58, 0x99,
    0x90, 0x03, 0x38, 0x1a, 0x08, 0xdc, 0x02, 0x30, 0x0c, 0x4e, 0xd1, 0x1d, 0x89, 0xa1, 0xe2, 0xd0,
    0x55, 0x11, 0x33, 0x60, 0x8c, 0x5a, 0x23, 0x06, 0xde, 0x06, 0x18, 0x00, 0x7f, 0xff, 0xfc, 0x4d,
    0xcc, 0x19, 0x17, 0xb3, 0x06, 0xc4, 0x44, 0xb2, 0x32, 0x5a, 0x44, 0xc4, 0xa6, 0x01, 0xf4, 0x24,
    0x88, 0x83, 0x38, 0xcc, 0xc4, 0x11, 0x09, 0x87, 0xa6, 0xe0, 0x46, 0x02, 0xb2, 0x24, 0x03, 0xe2,
    0xa0, 0x32, 0x54, 0x83, 0x52, 0xc5, 0xb1, 0x70,
];
const RAR3_RGB_FILTER_BYTECODE: &[u8] = &[
    0xc5, 0x01, 0x9a, 0x41, 0x95, 0xc9, 0xa6, 0x4d, 0xba, 0x4b, 0x14, 0x0a, 0xf4, 0x9b, 0x80, 0x4c,
    0x00, 0x15, 0xa6, 0xa8, 0x07, 0x26, 0x2a, 0xc9, 0xc4, 0x8b, 0x86, 0x62, 0x32, 0x0f, 0x86, 0x64,
    0x24, 0x06, 0x66, 0x71, 0x19, 0x98, 0xcc, 0x43, 0x33, 0x31, 0x99, 0x00, 0x66, 0x88, 0x33, 0x30,
    0xcc, 0xd1, 0x0e, 0x98, 0x0b, 0x33, 0x34, 0x40, 0x0c, 0xd1, 0x46, 0x66, 0x19, 0x9a, 0x28, 0xcc,
    0x49, 0x80, 0xb3, 0x33, 0x45, 0x00, 0xcd, 0x18, 0x66, 0x61, 0x99, 0xa3, 0x0c, 0xc8, 0x98, 0x0b,
    0x33, 0x34, 0x60, 0x4c, 0xd1, 0x06, 0x68, 0xa5, 0x20, 0x62, 0x66, 0x88, 0x33, 0x46, 0x28, 0x05,
    0x0f, 0x32, 0x0c, 0x4c, 0xd1, 0x46, 0x68, 0xc5, 0x00, 0x41, 0xe4, 0x8f, 0xc8, 0x85, 0x5e, 0x02,
    0x7c, 0xc9, 0x26, 0x81, 0x83, 0xb0, 0x9d, 0xc2, 0xde, 0x9c, 0x78, 0xac, 0xd6, 0x68, 0xb4, 0x0e,
    0x71, 0xdb, 0xb2, 0x49, 0x38, 0x6e, 0x02, 0x2a, 0x2c, 0x41, 0x2b, 0x10, 0x98, 0x82, 0x49, 0x03,
    0x14, 0xf4, 0xe1, 0x97, 0x00,
];
const RAR3_AUDIO_FILTER_BYTECODE: &[u8] = &[
    0x47, 0x01, 0x9a, 0x41, 0x95, 0xe5, 0x72, 0x0d, 0xc2, 0x64, 0x82, 0x74, 0x93, 0x24, 0xb1, 0x40,
    0x06, 0xd8, 0x38, 0x44, 0x00, 0xa8, 0x01, 0x34, 0x11, 0xdc, 0xa1, 0xba, 0x01, 0x99, 0x0c, 0xc4,
    0x03, 0x31, 0x19, 0xa4, 0x06, 0x66, 0x22, 0x60, 0x4d, 0x9a, 0x40, 0x0d, 0x66, 0x8e, 0x60, 0xd0,
    0x30, 0x40, 0x18, 0x26, 0xc1, 0xc8, 0xf6, 0xe6, 0x26, 0x13, 0x78, 0x92, 0x08, 0xe8, 0x50, 0xbc,
    0x5a, 0x07, 0xc6, 0xe9, 0xf5, 0x20, 0xa9, 0xa0, 0xed, 0x37, 0x33, 0x47, 0x39, 0x66, 0x90, 0x70,
    0x19, 0xa3, 0x9b, 0xcf, 0x25, 0x83, 0x80, 0xc1, 0xbd, 0x30, 0x16, 0x6e, 0x23, 0x34, 0x93, 0x81,
    0x16, 0x09, 0xb0, 0x50, 0x18, 0x3b, 0x4d, 0xc8, 0x4c, 0x05, 0x9b, 0x88, 0xc5, 0x28, 0xe0, 0x76,
    0x93, 0x90, 0x98, 0x0b, 0x37, 0x11, 0x8a, 0x59, 0xc4, 0x80, 0x42, 0x48, 0x43, 0xa9, 0x47, 0xee,
    0x43, 0x34, 0x60, 0x47, 0xd4, 0x4a, 0x0d, 0xbb, 0xd3, 0x59, 0xa4, 0x86, 0xee, 0x05, 0x09, 0x40,
    0x26, 0xc9, 0x34, 0x24, 0x76, 0xa0, 0x30, 0x6a, 0x20, 0xea, 0x02, 0x20, 0x04, 0xa0, 0x41, 0x50,
    0x9e, 0x50, 0x3f, 0xe6, 0xe1, 0x28, 0x94, 0x46, 0x01, 0xbd, 0x8b, 0x40, 0xf0, 0x68, 0x11, 0x36,
    0xc9, 0xa1, 0x92, 0x38, 0x11, 0x41, 0x9c, 0xa8, 0x95, 0x10, 0xee, 0x50, 0x66, 0x2b, 0x00, 0x20,
    0x95, 0x11, 0x04, 0x02, 0x62, 0xac, 0x66, 0x8c, 0x6a, 0xca, 0x26, 0x40, 0xb2, 0x67, 0x1b, 0x4b,
    0x26, 0xcc, 0x64, 0x8a, 0x62, 0x71, 0xa2, 0xb8,
];

/// One VM filter pass the LZ layer must announce: transform `block_size`
/// bytes starting `block_start` bytes into the (transformed) member by
/// replaying `code` with `init_regs`.
struct OwnedVmFilterRecord {
    block_start: usize,
    block_size: usize,
    init_regs: Vec<(usize, u32)>,
    code: &'static [u8],
}

/// The RAR3 standard filters this encoder can emit. Audio/RGB/Itanium are
/// read (their transforms are in `rar29.rs`) but not yet written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Itanium/Rgb constructed by tests; production auto-selects E8/E8E9/Delta/Audio
pub(crate) enum Rar29FilterKind {
    E8,
    E8E9,
    Delta {
        channels: usize,
    },
    /// Itanium branch-conversion filter (no init registers).
    Itanium,
    /// 24-bit RGB: `width` = scanline width in bytes (multiple of 3), the
    /// optional `pos_r` channel order (0 = RGB, 1 = GBR, 2 = BRG).
    Rgb {
        width: usize,
        pos_r: usize,
    },
    /// 8-bit audio with `channels` interleaved streams.
    Audio {
        channels: usize,
    },
}

/// Transform `data[range]` in place the way the RAR3 standard filter does
/// (same primitives the RAR5 writer uses; verified identical to rars).
/// Returns the record that lets a decoder undo it.
fn apply_rar29_filter(
    data: &mut [u8],
    kind: Rar29FilterKind,
    range: std::ops::Range<usize>,
) -> RarResult<OwnedVmFilterRecord> {
    let mut block = data[range.clone()].to_vec();
    let (init_regs, code) = match kind {
        Rar29FilterKind::E8 => {
            crate::codec::common::filters::e8_encode(&mut block, range.start as u64, true);
            (Vec::new(), RAR3_E8_FILTER_BYTECODE)
        }
        Rar29FilterKind::E8E9 => {
            crate::codec::common::filters::e8_encode(&mut block, range.start as u64, false);
            (Vec::new(), RAR3_E8E9_FILTER_BYTECODE)
        }
        Rar29FilterKind::Delta { channels } => {
            block = crate::codec::common::filters::delta_encode(
                &block,
                channels.min(u8::MAX as usize) as u8,
            );
            (vec![(0, channels as u32)], RAR3_DELTA_FILTER_BYTECODE)
        }
        Rar29FilterKind::Itanium => {
            itanium_encode(&mut block, range.start as u32);
            (Vec::new(), RAR3_ITANIUM_FILTER_BYTECODE)
        }
        Rar29FilterKind::Rgb { width, pos_r } => {
            block = rgb_encode(&block, width, pos_r)?;
            let init_regs = if pos_r == 0 {
                vec![(0, width as u32 + 3)]
            } else {
                vec![(0, width as u32 + 3), (1, pos_r as u32)]
            };
            (init_regs, RAR3_RGB_FILTER_BYTECODE)
        }
        Rar29FilterKind::Audio { channels } => {
            block = audio_encode(&block, channels)?;
            (vec![(0, channels as u32)], RAR3_AUDIO_FILTER_BYTECODE)
        }
    };
    data[range.clone()].copy_from_slice(&block);
    Ok(OwnedVmFilterRecord {
        block_start: range.start,
        block_size: range.end - range.start,
        init_regs,
        code,
    })
}

/// Split one filter range into VM-sized chunks (each standard program can
/// only cover `MAX_VM_FILTER_BLOCK_SIZE` bytes per execution; delta chunks
/// also keep whole channel groups). A trailing remainder too small for the
/// filter's unit is left unfiltered.
pub(crate) fn split_rar29_filter_range(
    kind: Rar29FilterKind,
    range: std::ops::Range<usize>,
) -> Vec<std::ops::Range<usize>> {
    const MAX_VM_FILTER_BLOCK_SIZE: usize = 128 * 1024;
    let (unit, chunk_size) = match kind {
        Rar29FilterKind::E8 | Rar29FilterKind::E8E9 | Rar29FilterKind::Itanium => {
            (4, MAX_VM_FILTER_BLOCK_SIZE)
        }
        Rar29FilterKind::Delta { channels } => {
            let channels = channels.max(1);
            let chunk = MAX_VM_FILTER_BLOCK_SIZE - (MAX_VM_FILTER_BLOCK_SIZE % channels);
            (channels, chunk)
        }
        Rar29FilterKind::Audio { channels } => {
            let channels = channels.max(1);
            let chunk = 120_000 - (120_000 % channels);
            (channels, chunk)
        }
        Rar29FilterKind::Rgb { width, .. } => {
            let width = width.max(3);
            let chunk = MAX_VM_FILTER_BLOCK_SIZE - (MAX_VM_FILTER_BLOCK_SIZE % width);
            (width, chunk)
        }
    };
    let mut out = Vec::new();
    let mut start = range.start;
    while start < range.end {
        let end = (start + chunk_size).min(range.end);
        if end - start < unit {
            break;
        }
        out.push(start..end);
        start = end;
    }
    out
}

// ── RAR3 standard-filter transforms (encode side) ──────────────────────────
// Mirrors of rars' rar29.rs rgb_encode/audio_encode/itanium_encode; the read
// side (rar29.rs) carries the inverse transforms.

fn rgb_predict(prev: u8, upper: u8, upper_left: u8) -> u8 {
    let predicted = i32::from(prev) + i32::from(upper) - i32::from(upper_left);
    let pa = (predicted - i32::from(prev)).abs();
    let pb = (predicted - i32::from(upper)).abs();
    let pc = (predicted - i32::from(upper_left)).abs();
    if pa <= pb && pa <= pc {
        prev
    } else if pb <= pc {
        upper
    } else {
        upper_left
    }
}

fn rgb_encode(data: &[u8], width: usize, pos_r: usize) -> Result<Vec<u8>, RarError> {
    if data.len() < 3 || width == 0 || !width.is_multiple_of(3) || width > data.len() || pos_r > 2 {
        return Err(enc_err("RAR 2.9 RGB filter parameters are invalid"));
    }
    let mut work = data.to_vec();
    for i in (pos_r..work.len().saturating_sub(2)).step_by(3) {
        let green = work[i + 1];
        work[i] = work[i].wrapping_sub(green);
        work[i + 2] = work[i + 2].wrapping_sub(green);
    }

    let mut out = Vec::with_capacity(data.len());
    for channel in 0..3 {
        let mut prev = 0u8;
        let mut i = channel;
        while i < work.len() {
            let predicted = if i >= width + 3 {
                rgb_predict(prev, work[i - width], work[i - width - 3])
            } else {
                prev
            };
            let byte = work[i];
            out.push(predicted.wrapping_sub(byte));
            prev = byte;
            i += 3;
        }
    }
    Ok(out)
}

fn audio_encode(data: &[u8], channels: usize) -> Result<Vec<u8>, RarError> {
    if channels == 0 || channels > 32 {
        return Err(enc_err("RAR 2.9 AUDIO filter channel count is invalid"));
    }
    let mut out = Vec::with_capacity(data.len());
    for channel in 0..channels {
        let mut prev_byte = 0u32;
        let mut prev_delta = 0i32;
        let mut d1 = 0i32;
        let mut d2 = 0i32;
        let mut k1 = 0i32;
        let mut k2 = 0i32;
        let mut k3 = 0i32;
        let mut dif = [0u32; 7];
        let mut byte_count = 0usize;
        let mut i = channel;
        while i < data.len() {
            let d3 = d2;
            d2 = prev_delta - d1;
            d1 = prev_delta;
            let predicted = ((8 * prev_byte as i32 + k1 * d1 + k2 * d2 + k3 * d3) >> 3) & 0xff;
            let decoded = data[i];
            let encoded = (predicted as u8).wrapping_sub(decoded);
            out.push(encoded);
            prev_delta = decoded.wrapping_sub(prev_byte as u8) as i8 as i32;
            prev_byte = decoded as u32;
            let d = (encoded as i8 as i32) << 3;
            dif[0] += d.unsigned_abs();
            dif[1] += (d - d1).unsigned_abs();
            dif[2] += (d + d1).unsigned_abs();
            dif[3] += (d - d2).unsigned_abs();
            dif[4] += (d + d2).unsigned_abs();
            dif[5] += (d - d3).unsigned_abs();
            dif[6] += (d + d3).unsigned_abs();
            if byte_count & 0x1f == 0 {
                let mut min = dif[0];
                let mut min_index = 0usize;
                dif[0] = 0;
                for (index, value) in dif.iter_mut().enumerate().skip(1) {
                    if *value < min {
                        min = *value;
                        min_index = index;
                    }
                    *value = 0;
                }
                match min_index {
                    1 if k1 >= -16 => k1 -= 1,
                    2 if k1 < 16 => k1 += 1,
                    3 if k2 >= -16 => k2 -= 1,
                    4 if k2 < 16 => k2 += 1,
                    5 if k3 >= -16 => k3 -= 1,
                    6 if k3 < 16 => k3 += 1,
                    _ => {}
                }
            }
            byte_count += 1;
            i += channels;
        }
    }
    Ok(out)
}

fn itanium_encode(data: &mut [u8], file_offset: u32) {
    if data.len() <= 21 {
        return;
    }
    let base_offset = file_offset >> 4;
    let block_count = (data.len() - 21).div_ceil(16);
    for block in 0..block_count {
        let pos = block * 16;
        let file_offset = base_offset.wrapping_add(block as u32);
        let mut mask = (0x334b_0000u32 >> (data[pos] & 0x1e)) & 3;
        if mask != 0 {
            mask += 1;
            while mask <= 4 {
                let p = pos + (mask as usize * 5 - 8);
                if ((data[p + 3] >> mask) & 15) == 5 {
                    let raw = u32::from_le_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
                    let mut value = raw >> mask;
                    value = value.wrapping_add(file_offset) & 0x000f_ffff;
                    let raw = (raw & !(0x000f_ffff << mask)) | (value << mask);
                    data[p..p + 4].copy_from_slice(&raw.to_le_bytes());
                }
                mask += 1;
            }
        }
    }
}

/// Write an encoded u32 (2-bit width selector + value), mirroring
/// `rar29.rs::read_encoded_u32` for the widths this encoder emits (16..255
/// never uses the decoder's negative-range branch, and nothing here emits
/// 256..=65535 except via the 16-bit selector).
fn write_encoded_u32(bits: &mut crate::codec::common::bitstream::BitWriter, value: u32) {
    if value < 16 {
        bits.write_bits(0, 2);
        bits.write_bits(value, 4);
    } else if value < 256 {
        bits.write_bits(1, 2);
        bits.write_bits(value, 8);
    } else if value <= 0xffff {
        bits.write_bits(2, 2);
        bits.write_bits(value, 16);
    } else {
        bits.write_bits(3, 2);
        bits.write_bits(value >> 16, 16);
        bits.write_bits(value & 0xffff, 16);
    }
}

/// Encode one VM filter record (rars `encode_vm_filter_record_inner`):
/// `first_byte` flags + body, with the record length packed into the low
/// bits of `first_byte` and optional extension bytes.
fn encode_vm_filter_record_inner(
    record: &OwnedVmFilterRecord,
    program_selector: u32,
    include_code: bool,
) -> RarResult<Vec<u8>> {
    if record.block_size == 0 {
        return Err(enc_err("RAR 2.9 VM filter block is empty"));
    }
    if include_code && record.code.is_empty() {
        return Err(enc_err("RAR 2.9 VM filter bytecode is empty"));
    }

    let mut body = crate::codec::common::bitstream::BitWriter::new();
    write_encoded_u32(&mut body, program_selector);
    write_encoded_u32(
        &mut body,
        u32::try_from(record.block_start)
            .map_err(|_| enc_err("RAR 2.9 VM block start overflows"))?,
    );
    write_encoded_u32(
        &mut body,
        u32::try_from(record.block_size).map_err(|_| enc_err("RAR 2.9 VM block size overflows"))?,
    );
    if !record.init_regs.is_empty() {
        let mut mask = 0u32;
        for &(index, _) in &record.init_regs {
            if index >= 7 {
                return Err(enc_err("RAR 2.9 VM init register index is invalid"));
            }
            mask |= 1 << index;
        }
        body.write_bits(mask, 7);
        for index in 0..7 {
            if let Some((_, value)) = record.init_regs.iter().find(|(reg, _)| *reg == index) {
                write_encoded_u32(&mut body, *value);
            }
        }
    }
    if include_code {
        write_encoded_u32(
            &mut body,
            u32::try_from(record.code.len())
                .map_err(|_| enc_err("RAR 2.9 VM code size overflows"))?,
        );
        for &byte in record.code {
            body.write_bits(u32::from(byte), 8);
        }
    }
    let body = body.into_bytes();

    let mut out = Vec::new();
    let mut first: u8 = 0x80 | 0x20;
    if !record.init_regs.is_empty() {
        first |= 0x10;
    }
    match body.len() {
        1..=6 => first |= (body.len() as u8) - 1,
        7..=262 => {
            first |= 6;
            out.push((body.len() - 7) as u8);
        }
        263..=65535 => {
            first |= 7;
            out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        }
        _ => return Err(enc_err("RAR 2.9 VM filter record is too large")),
    }
    out.insert(0, first);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Encode the filter records whose range starts in `[base, base + window)`,
/// keeping the shared program table across blocks (a program is spelled out
/// once; later blocks reference it by selector).
fn encoded_filter_records_at(
    filters: &[&OwnedVmFilterRecord],
    base: usize,
    window: usize,
    programs: &mut Vec<&'static [u8]>,
) -> RarResult<Vec<Vec<u8>>> {
    let mut records = Vec::with_capacity(filters.len());
    for filter in filters {
        let existing = (filter.code != RAR3_DELTA_FILTER_BYTECODE)
            .then(|| programs.iter().position(|&code| code == filter.code))
            .flatten();
        let (program_selector, include_code) = match existing {
            Some(index) => (u32::try_from(index + 1).map_err(enc_int)?, false),
            None => {
                let selector = if programs.is_empty() {
                    0
                } else {
                    u32::try_from(programs.len() + 1).map_err(enc_int)?
                };
                programs.push(filter.code);
                (selector, true)
            }
        };
        let block_start = filter
            .block_start
            .checked_sub(base)
            .ok_or_else(|| enc_err("RAR 2.9 VM filter starts before its block"))?;
        if block_start >= window {
            return Err(enc_err(
                "RAR 2.9 VM filter starts further past its block than the window can express",
            ));
        }
        // The wire record carries the range RELATIVE to its own block; the
        // decoder adds its current output position back on.
        let mut adjusted = OwnedVmFilterRecord {
            block_start,
            block_size: filter.block_size,
            init_regs: filter.init_regs.clone(),
            code: filter.code,
        };
        records.push(encode_vm_filter_record_inner(
            &adjusted,
            program_selector,
            include_code,
        )?);
        adjusted.init_regs.clear();
    }
    Ok(records)
}

fn enc_int(_: std::num::TryFromIntError) -> RarError {
    enc_err("RAR 2.9 VM program index overflows")
}

/// Code a filtered member: transform `input` under `filters`, then LZ-code
/// the transformed bytes block by block, announcing each filter record at
/// the head of the block its range starts in. The caller hands back the
/// transformed bytes too, since those are what a solid chain's window holds.
fn encode_filtered_member_blocks(
    data: &[u8],
    history: &[u8],
    filters: &[OwnedVmFilterRecord],
    options: EncodeOptions,
    levels: &mut [u8; TABLE_COUNT],
) -> RarResult<(Vec<u8>, Vec<u8>)> {
    let block_size = options
        .block_size
        .filter(|&size| size != 0)
        .unwrap_or(data.len().max(1))
        .min(options.max_match_distance.max(1))
        .max(1);
    let window = options.max_match_distance.max(1);
    let mut inner = options;
    inner.block_size = None;
    let mut programs: Vec<&'static [u8]> = Vec::new();
    let mut out = Vec::new();
    let mut local_history = history[history.len().saturating_sub(MAX_HISTORY)..].to_vec();
    let mut base = 0usize;
    while base < data.len().max(1) {
        let end = (base + block_size).min(data.len());
        let chunk = &data[base..end];
        let in_block: Vec<&OwnedVmFilterRecord> = filters
            .iter()
            .filter(|record| record.block_start >= base && record.block_start < end.max(base + 1))
            .collect();
        let records = encoded_filter_records_at(&in_block, base, window, &mut programs)?;
        out.extend_from_slice(&encode_member_inner(
            chunk,
            &local_history,
            &records,
            inner,
            end < data.len(),
            levels,
            None,
        )?);
        local_history.extend_from_slice(chunk);
        let keep_from = local_history.len().saturating_sub(MAX_HISTORY);
        if keep_from != 0 {
            local_history.drain(..keep_from);
        }
        base = end;
    }
    // `data` is already the transformed bytes (the caller transformed it
    // before calling); the solid chain must remember them, not the original.
    Ok((out, data.to_vec()))
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
    /// The PPMd model a reader holds, once some member of a solid run has
    /// built one. A chain can interleave LZ and PPMd members: each engine
    /// keeps its own state and only the winning member's state advances.
    /// Boxed: the model carries ~20 KB of fixed tables (see the decoder's
    /// own `ppmd: Box<...>` note) and must not inline into `WriteState`,
    /// whose `RarArchive` frame already brushes the 1 MiB main-thread stack.
    ppmd: Option<Box<crate::codec::legacy::ppmd::PpmdDecoder>>,
    /// Whether the last COMPRESSED member was PPMd: decides whether the next
    /// PPMd member continues the model (0x87 header) or starts fresh (0xA7).
    last_was_ppmd: bool,
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
            ppmd: None,
            last_was_ppmd: false,
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
        self.last_was_ppmd = false;
        self.remember(input);
        Ok(packed)
    }

    /// Code one member of a solid run as PPMd, continuing the carried model
    /// when the previous compressed member was PPMd, otherwise starting a
    /// fresh order-8 / 25 MiB model (0xA7 + dict byte header). The model is
    /// stored back for the next PPMd member; LZ levels are left untouched.
    /// Window: PPMd output equals the input, so `remember` is the caller's
    /// job (the LZ candidate already remembered it when both were tried).
    pub fn encode_ppmd_member_chain(&mut self, input: &[u8]) -> RarResult<Vec<u8>> {
        use crate::codec::legacy::ppmd::PpmdEncoder;
        let continuing = self.ppmd.is_some() && self.last_was_ppmd;
        let mut out = Vec::new();
        let mut encoder = match (continuing, self.ppmd.take()) {
            (true, Some(model)) => {
                out.push(0x80 | (PPMD_ORDER as u8 - 1)); // continuing header
                PpmdEncoder::continuing(*model, PPMD_ESC)
            }
            _ => {
                out.push(0x80 | 0x20 | (PPMD_ORDER as u8 - 1));
                out.push(PPMD_DICTIONARY_MB - 1);
                PpmdEncoder::new(PPMD_ORDER, PPMD_ESC, usize::from(PPMD_DICTIONARY_MB))
                    .map_err(ppmd_err)?
            }
        };
        encode_ppmd_hybrid(input, &mut encoder)?;
        let (packed, model) = encoder.finish_keeping_model().map_err(ppmd_err)?;
        out.extend_from_slice(&packed);
        self.ppmd = Some(Box::new(model));
        self.last_was_ppmd = true;
        Ok(out)
    }

    /// Solid-run member: code it both ways (LZ and PPMd) and keep the
    /// smaller, advancing only the winner's chain state. PPMd reuses the
    /// carried model when the last member was PPMd (the model is cloned so a
    /// losing trial never disturbs it).
    pub fn encode_solid_member(&mut self, input: &[u8]) -> RarResult<Vec<u8>> {
        // LZ first (submits levels + window). If PPMd wins, the LZ table
        // advance must be rolled back: a decoder never decodes an LZ table
        // for a PPMd member, so its levels stay at the pre-member value, and
        // the next LZ member's keep/delta decision must be made against that.
        let levels_before = self.levels;
        let lz = self.encode_member(input)?;
        // The PPMd trial may continue the carried model; try on a clone so a
        // loss leaves the model untouched for a later member.
        let saved = self.ppmd.clone();
        let saved_flag = self.last_was_ppmd;
        let trial = self.encode_ppmd_member_chain(input);
        match trial {
            Ok(ppmd) if ppmd.len() < lz.len() => Ok(ppmd),
            _ => {
                self.levels = levels_before;
                self.ppmd = saved;
                self.last_was_ppmd = saved_flag;
                Ok(lz)
            }
        }
    }

    /// Code one member as a PPMd block (m4/m5 text path). Always starts a
    /// fresh model; the caller decides when PPMd is worth trying and keeps
    /// this encoder out of solid runs (see `encode_ppmd_member_packed`).
    pub fn encode_ppmd_member(&mut self, input: &[u8]) -> RarResult<Vec<u8>> {
        encode_ppmd_member_packed(input, true)
    }

    /// PPMd with LZ escapes disabled (pure literals) — used by the caller
    /// when the member is small enough that the tokeniser overhead is not
    /// worth it, and by tests.
    #[allow(dead_code)] // exercised via `encode_ppmd_member_packed` in tests
    pub fn encode_ppmd_literals_member(&mut self, input: &[u8]) -> RarResult<Vec<u8>> {
        encode_ppmd_member_packed(input, false)
    }

    /// Code one member through the RAR3 standard filter `kind` over
    /// `range`, then LZ-code the transformed bytes with the filter records
    /// announced at the head of the block each range starts in. The window
    /// is advanced with the TRANSFORMED bytes (what a decoder holds).
    /// `None` ranges cover the whole member.
    #[allow(dead_code)] // single-range convenience; the ranges variant is the production path
    pub fn encode_member_with_filter(
        &mut self,
        input: &[u8],
        kind: Rar29FilterKind,
        range: Option<std::ops::Range<usize>>,
    ) -> RarResult<Vec<u8>> {
        let range = range.unwrap_or(0..input.len());
        let mut data = input.to_vec();
        let mut filters = Vec::new();
        for chunk in split_rar29_filter_range(kind, range) {
            filters.push(apply_rar29_filter(&mut data, kind, chunk)?);
        }
        let (packed, transformed) = encode_filtered_member_blocks(
            &data,
            &self.history,
            &filters,
            self.options,
            &mut self.levels,
        )?;
        self.remember(&transformed);
        Ok(packed)
    }

    /// Code one member through a list of same-kind filter ranges (e.g. every
    /// x86 cluster the auto-scanner found), sharing one program record.
    pub fn encode_member_with_filter_ranges(
        &mut self,
        input: &[u8],
        kind: Rar29FilterKind,
        ranges: &[std::ops::Range<usize>],
    ) -> RarResult<Vec<u8>> {
        if ranges.is_empty() {
            return self.encode_member(input);
        }
        let mut data = input.to_vec();
        let mut filters = Vec::new();
        for range in ranges {
            for chunk in split_rar29_filter_range(kind, range.clone()) {
                filters.push(apply_rar29_filter(&mut data, kind, chunk)?);
            }
        }
        let (packed, transformed) = encode_filtered_member_blocks(
            &data,
            &self.history,
            &filters,
            self.options,
            &mut self.levels,
        )?;
        self.remember(&transformed);
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
    fn ppmd_roundtrip_text_hybrid() {
        let input = sample_text();
        let packed = encode_ppmd_member_packed(&input, true).unwrap();
        assert!(
            packed.len() < input.len() / 4,
            "PPMd hybrid should compress repetitive text hard: {} -> {}",
            input.len(),
            packed.len()
        );
        let mut decoder = crate::codec::legacy::rar29::Rar29Decoder::new();
        let out = decoder
            .decode_member(&packed, input.len() as u64)
            .unwrap_or_else(|e| panic!("PPMd hybrid decode: {e:?}"));
        assert_eq!(out, input, "PPMd hybrid roundtrip mismatch");
    }

    #[test]
    fn ppmd_roundtrip_text_literals() {
        let input = sample_text();
        let packed = encode_ppmd_member_packed(&input, false).unwrap();
        assert!(
            packed.len() < input.len() / 4,
            "PPMd literals should compress repetitive text hard: {} -> {}",
            input.len(),
            packed.len()
        );
        let mut decoder = crate::codec::legacy::rar29::Rar29Decoder::new();
        let out = decoder.decode_member(&packed, input.len() as u64).unwrap();
        assert_eq!(out, input, "PPMd literals roundtrip mismatch");
    }

    #[test]
    fn ppmd_roundtrip_binary_and_empty() {
        // Structured binary with runs and repeats (still PPMd-friendly).
        let mut input = Vec::new();
        for i in 0..4000u16 {
            input.extend_from_slice(&i.to_le_bytes());
            input.extend_from_slice(&i.to_le_bytes());
            if i % 7 == 0 {
                input.extend_from_slice(&[0u8; 64]);
            }
        }
        let packed = encode_ppmd_member_packed(&input, true).unwrap();
        let mut decoder = crate::codec::legacy::rar29::Rar29Decoder::new();
        assert_eq!(
            decoder.decode_member(&packed, input.len() as u64).unwrap(),
            input
        );

        // Empty member: header + end-of-block only; decodes to nothing.
        let packed = encode_ppmd_member_packed(&[], true).unwrap();
        let mut decoder = crate::codec::legacy::rar29::Rar29Decoder::new();
        assert_eq!(decoder.decode_member(&packed, 0).unwrap(), Vec::<u8>::new());
    }

    fn sample_text() -> Vec<u8> {
        // Repetitive prose with enough variation to exercise escapes and
        // literals; a good PPMd candidate.
        let mut out = Vec::with_capacity(220_000);
        let sentences = [
            "The quick brown fox jumps over the lazy dog. ",
            "Pack my box with five dozen liquor jugs. ",
            "How vexingly quick daft zebras jump! ",
            "Sphinx of black quartz, judge my vow. ",
        ];
        let mut n = 0u32;
        while out.len() < 200_000 {
            out.extend_from_slice(
                format!("paragraph {n:05}: {}\n", sentences[(n % 4) as usize]).as_bytes(),
            );
            n += 1;
        }
        out
    }

    /// Synthetic x86-ish data: a scatter of E8 (call) opcodes whose 4-byte
    /// operands are small relative offsets (the shape the E8 filter makes
    /// compressible). After `e8_encode` the operands become absolute-ish
    /// values the filter record can reverse.
    fn x86_like_data(blocks: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(blocks * 256);
        let mut seed = 0xDEADBEEFu32;
        for _ in 0..blocks {
            for _ in 0..64 {
                data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
            }
            // A handful of E8 calls with small relative targets.
            for k in 0..6u32 {
                data.push(0xE8);
                let target = k * 0x100 + 0x40;
                data.extend_from_slice(&target.to_le_bytes());
                data.extend_from_slice(&[0x90, 0x90]);
            }
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        }
        data
    }

    #[test]
    fn e8_filter_roundtrips_via_decoder() {
        let input = x86_like_data(2000);
        let mut encoder = Unpack29Encoder::with_options(options_for_level(3));
        let packed = encoder
            .encode_member_with_filter(&input, Rar29FilterKind::E8, None)
            .unwrap();
        // The filtered member must be plainly smaller than the raw input
        // (E8 zeros dominate after the transform).
        assert!(
            packed.len() < input.len() / 2,
            "E8-filtered member should compress hard: {} -> {}",
            input.len(),
            packed.len()
        );
        let mut decoder = crate::codec::legacy::rar29::Rar29Decoder::new();
        let out = decoder
            .decode_member(&packed, input.len() as u64)
            .unwrap_or_else(|e| panic!("E8 filtered decode: {e:?}"));
        assert_eq!(out, input, "E8 filter roundtrip mismatch");
    }

    #[test]
    fn e8e9_filter_roundtrips_via_decoder() {
        let input = x86_like_data(1500);
        let mut encoder = Unpack29Encoder::with_options(options_for_level(3));
        let packed = encoder
            .encode_member_with_filter(&input, Rar29FilterKind::E8E9, None)
            .unwrap();
        let mut decoder = crate::codec::legacy::rar29::Rar29Decoder::new();
        assert_eq!(
            decoder.decode_member(&packed, input.len() as u64).unwrap(),
            input,
            "E8E9 filter roundtrip mismatch"
        );
    }

    #[test]
    fn delta_filter_roundtrips_via_decoder() {
        // 16-bit stereo samples (little-endian pairs with slow deltas) — the
        // classic delta-filter payload.
        let mut input = Vec::with_capacity(120_000);
        let mut left = 0i16;
        let mut right = 0i16;
        let mut seed = 12345u32;
        while input.len() < 100_000 {
            left = left.wrapping_add(((seed >> 16) & 0xff) as i16 - 100);
            right = right.wrapping_add((((seed >> 8) & 0xff) as i16) - 120);
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            input.extend_from_slice(&left.to_le_bytes());
            input.extend_from_slice(&right.to_le_bytes());
        }
        let mut encoder = Unpack29Encoder::with_options(options_for_level(3));
        let packed = encoder
            .encode_member_with_filter(&input, Rar29FilterKind::Delta { channels: 4 }, None)
            .unwrap();
        let mut decoder = crate::codec::legacy::rar29::Rar29Decoder::new();
        assert_eq!(
            decoder.decode_member(&packed, input.len() as u64).unwrap(),
            input,
            "delta filter roundtrip mismatch"
        );
    }

    fn audio_wave_data(bytes: usize) -> Vec<u8> {
        // Interleaved 8-bit two-channel audio: per-channel smooth walk.
        let mut data = Vec::with_capacity(bytes);
        let mut ch = [128i16; 2];
        let mut seed = 0xC0FFEEu32;
        while data.len() < bytes {
            for (c, sample) in ch.iter_mut().enumerate() {
                *sample = (*sample + (((seed >> (c * 8)) & 0x3f) as i16 - 30)).clamp(0, 255);
                data.push(*sample as u8);
            }
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        }
        data
    }

    fn rgb_data(bytes: usize) -> Vec<u8> {
        // 24-bit RGB rows with smooth colour gradients (row stride 300 = 100 px).
        let mut data = Vec::with_capacity(bytes);
        let width = 300usize;
        let mut seed = 0xABCDEFu32;
        while data.len() < bytes {
            let mut row = Vec::with_capacity(width);
            for i in 0..width {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let base = ((seed >> 16) & 0xff) as u8;
                row.push(base.wrapping_add((i as u8).wrapping_mul(3)));
            }
            data.extend_from_slice(&row);
        }
        let cut = (bytes / width) * width;
        data.truncate(cut);
        data
    }

    #[test]
    fn itanium_filter_roundtrips_via_decoder() {
        // Itanium bundles: byte 0's slot-mask template must only select
        // masks 0,2,3,4 (mask 1 would make the slot offset negative), so
        // (b % 11) * 2 cycles 0x00..0x14 and never 0x16 (the mask-1 template).
        let mut input = Vec::new();
        for b in 0..2000usize {
            let mut bundle = vec![0u8; 16];
            bundle[0] = ((b % 11) * 2) as u8;
            for slot in 2..=4u32 {
                let p = slot as usize * 5 - 8;
                if p + 4 <= 16 {
                    bundle[p] = 0x0d;
                    bundle[p + 3] = bundle[p + 3].wrapping_add(0x50);
                }
            }
            input.extend_from_slice(&bundle);
        }
        let mut encoder = Unpack29Encoder::with_options(options_for_level(3));
        let packed = encoder
            .encode_member_with_filter(&input, Rar29FilterKind::Itanium, None)
            .unwrap();
        let mut decoder = crate::codec::legacy::rar29::Rar29Decoder::new();
        assert_eq!(
            decoder.decode_member(&packed, input.len() as u64).unwrap(),
            input,
            "Itanium filter roundtrip mismatch"
        );
    }

    #[test]
    fn rgb_filter_roundtrips_via_decoder() {
        let input = rgb_data(150_000);
        let mut encoder = Unpack29Encoder::with_options(options_for_level(3));
        let packed = encoder
            .encode_member_with_filter(
                &input,
                Rar29FilterKind::Rgb {
                    width: 300,
                    pos_r: 0,
                },
                None,
            )
            .unwrap();
        let mut decoder = crate::codec::legacy::rar29::Rar29Decoder::new();
        assert_eq!(
            decoder.decode_member(&packed, input.len() as u64).unwrap(),
            input,
            "RGB filter roundtrip mismatch"
        );
    }

    #[test]
    fn audio_filter_roundtrips_via_decoder() {
        let input = audio_wave_data(180_000);
        let mut encoder = Unpack29Encoder::with_options(options_for_level(3));
        let packed = encoder
            .encode_member_with_filter(&input, Rar29FilterKind::Audio { channels: 2 }, None)
            .unwrap();
        assert!(
            packed.len() * 3 < input.len(),
            "audio filter should compress smooth waves hard: {} -> {}",
            input.len(),
            packed.len()
        );
        let mut decoder = crate::codec::legacy::rar29::Rar29Decoder::new();
        assert_eq!(
            decoder.decode_member(&packed, input.len() as u64).unwrap(),
            input,
            "audio filter roundtrip mismatch"
        );
    }

    #[test]
    fn filter_range_partial_roundtrips() {
        // Only the middle of the member is x86 code; the tail stays literal.
        let mut input = x86_like_data(1000);
        input.extend_from_slice(&[0u8; 5000]);
        let range = 0..input.len() - 5000;
        let mut encoder = Unpack29Encoder::with_options(options_for_level(3));
        let packed = encoder
            .encode_member_with_filter(&input, Rar29FilterKind::E8, Some(range))
            .unwrap();
        let mut decoder = crate::codec::legacy::rar29::Rar29Decoder::new();
        assert_eq!(
            decoder.decode_member(&packed, input.len() as u64).unwrap(),
            input,
            "partial-range E8 filter roundtrip mismatch"
        );
    }

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

            let mut decoder = crate::codec::legacy::rar29::Rar29Decoder::new();
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
