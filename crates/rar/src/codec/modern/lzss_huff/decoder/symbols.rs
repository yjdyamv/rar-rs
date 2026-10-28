//! Symbol-stream state machine: the single reader of a RAR5/RAR7 member
//! bitstream.
//!
//! [`SymbolReader`] owns block framing, checksum verification, Huffman table
//! refresh, the distance cache and the last-length state, and yields resolved
//! [`Symbol`]s. [`super::engine`] applies them to the window;
//! [`super::analysis`] only counts them.

use super::*;

use super::super::{
    BLOCK_CHECKSUM_SEED, DIST_CACHE_SIZE, SYM_CACHE_BASE, SYM_FILTER, SYM_MATCH_BASE, SYM_REPEAT,
};
use super::tables::{
    apply_length_bonus_u64, decode_distance, decode_length, dist_cache_push, dist_cache_touch,
    parse_filter, read_tables,
};
use crate::codec::common::bitstream::BitReader;
use crate::codec::common::huffman::{DecodeTable, decode_symbol};
use crate::error::{RarError, RarResult};
use crate::version::ArchiveVersion;

/// Static facts about a block, read when the block starts. The four table
/// symbol counts are zero when the block reuses the previous block's tables.
pub(super) struct BlockInfo {
    pub block_size: u32,
    pub table_present: bool,
    pub nc: usize,
    pub dc: usize,
    pub ldc: usize,
    pub rc: usize,
}

/// How a match was encoded.
pub(super) enum MatchKind {
    /// A full match symbol.
    Match,
    /// One of the four distance-cache references.
    Cache,
    /// A repeat of the previous match's length and distance.
    Repeat,
}

/// One resolved symbol.
pub(super) enum Symbol {
    Literal(u8),
    Match {
        dist: u64,
        len: u32,
        kind: MatchKind,
    },
    /// A filter record, parsed and awaiting its output region. The engine
    /// stages it; analysis only records the region.
    Filter(PendingFilter),
    /// The start of a block; consumers that need block statistics use it,
    /// the engine ignores it.
    BlockStart(BlockInfo),
}

/// Symbol-stream state carried across the members of a solid chain.
#[derive(Default)]
pub(super) struct SymbolState {
    dist_cache: [u64; DIST_CACHE_SIZE],
    last_length: u32,
    table_nc: Option<DecodeTable>,
    table_dc: Option<DecodeTable>,
    table_ldc: Option<DecodeTable>,
    table_rc: Option<DecodeTable>,
}

/// The block currently being decoded.
struct ActiveBlock {
    /// Bit offset of the block's first symbol.
    start_bits: u64,
    /// Number of bits the block's symbol area occupies.
    bits: u64,
    is_last: bool,
}

/// Reads one member's packed bytes as a sequence of [`Symbol`]s.
///
/// `start_pos` is the absolute output position of the member (the shared
/// solid-chain window's `total_written`, or zero for a standalone member);
/// `unpacked_size` bounds the produced bytes so a member can stop mid-block
/// exactly like the window loop does.
pub(super) struct SymbolReader<'d, 's> {
    reader: BitReader<'d>,
    variant: ArchiveVersion,
    state: &'s mut SymbolState,
    /// Absolute output position of the next symbol.
    pos: u64,
    /// Absolute output position at which this member stops.
    end: u64,
    /// End of the furthest filter region seen; must not pass [`Self::end`].
    max_filter_end: u64,
    block: Option<ActiveBlock>,
    /// The first malformed-stream error. Held here rather than in the return
    /// type: `RarError` is 88 bytes, so threading it through `Result` at every
    /// symbol pushed that whole struct through the stack ~6 million times per
    /// member and dominated decode time.
    error: Option<RarError>,
}

impl<'d, 's> SymbolReader<'d, 's> {
    pub(super) fn new(
        data: &'d [u8],
        variant: ArchiveVersion,
        start_pos: u64,
        unpacked_size: u64,
        state: &'s mut SymbolState,
    ) -> Self {
        Self {
            reader: BitReader::new(data),
            variant,
            state,
            pos: start_pos,
            end: start_pos.saturating_add(unpacked_size),
            max_filter_end: start_pos,
            block: None,
            error: None,
        }
    }

    /// Absolute output position of the next symbol.
    #[inline]
    pub(super) fn pos(&self) -> u64 {
        self.pos
    }

    /// Yield the next symbol, or `None` when the member's declared output
    /// size has been produced, the last block ended, or the stream is
    /// malformed.
    ///
    /// Malformed streams are reported through [`Self::take_error`] rather than
    /// a `Result`, which keeps the per-symbol boundary two words wide: a
    /// `Result<Option<Symbol>, RarError>` is 88 bytes wide because `RarError`
    /// is, and returning it per symbol dominated decode time.
    #[inline(always)]
    pub(super) fn next(&mut self) -> Option<Symbol> {
        match self.next_symbol() {
            Some(symbol) => Some(symbol),
            None => {
                if self.error.is_none() && self.max_filter_end > self.end {
                    return self.fail(RarError::format("unapplied RAR5 filter at end of stream"));
                }
                None
            }
        }
    }

    /// The malformed-stream error that stopped [`Self::next`], if any.
    pub(super) fn take_error(&mut self) -> Option<RarError> {
        self.error.take()
    }

    /// Record `err` and stop the stream. Kept out of line on purpose: it runs
    /// at most once per member, so it must not be inlined into the loop.
    #[cold]
    #[inline(never)]
    fn fail(&mut self, err: RarError) -> Option<Symbol> {
        self.error = Some(err);
        None
    }

    /// The per-symbol entry point. Small enough to inline into the engine's
    /// loop *because* the block-header path below stays out of line: inlining
    /// the header reader here once grew this function past LLVM's budget and
    /// every symbol paid an out-of-line call instead.
    #[inline(always)]
    fn next_symbol(&mut self) -> Option<Symbol> {
        loop {
            if self.pos >= self.end {
                return None;
            }
            if let Some(block) = self.block.as_ref() {
                let end_bits = block.start_bits + block.bits;
                let is_last = block.is_last;
                if self.reader.consumed_bits() < end_bits {
                    if let Some(symbol) = self.decode_symbol() {
                        return Some(symbol);
                    }
                    if self.error.is_some() {
                        return None;
                    }
                    // A repeat with nothing to repeat yields no output.
                    continue;
                }
                // Position the reader at the exact end of the block.
                self.reader
                    .set_position((end_bits / 8) as usize, (end_bits % 8) as u8);
                self.block = None;
                if is_last {
                    return None;
                }
                continue;
            }
            // A block header is read a handful of times per member, so the
            // rich `RarResult` there costs nothing.
            return match self.read_block_header() {
                Ok(info) => Some(Symbol::BlockStart(info)),
                Err(err) => self.fail(err),
            };
        }
    }

    /// Read a block header, verify its checksum, refresh the Huffman tables
    /// when the block carries them, and start the block.
    ///
    /// Runs a handful of times per member, so it must stay out of
    /// [`Self::next_symbol`]: its table building is hundreds of instructions
    /// that would otherwise be copied into the hottest function in the crate.
    #[inline(never)]
    fn read_block_header(&mut self) -> RarResult<BlockInfo> {
        let block_flags_byte = self.reader.read_byte().map_err(format_err)?;
        let table_present = (block_flags_byte >> 7) & 1 != 0;
        let is_last = (block_flags_byte >> 6) & 1 != 0;
        let byte_count = ((block_flags_byte >> 3) & 3) + 1;
        let bit_size = block_flags_byte & 7;

        let checksum_byte = self.reader.read_byte().map_err(format_err)?;
        let size_bytes = self
            .reader
            .read_bytes(byte_count as usize)
            .map_err(format_err)?;
        let mut block_size: u32 = 0;
        for (i, &b) in size_bytes.iter().enumerate() {
            block_size |= (b as u32) << (i * 8);
        }

        let mut expected_ck = BLOCK_CHECKSUM_SEED ^ block_flags_byte;
        for &b in size_bytes {
            expected_ck ^= b;
        }
        if checksum_byte != expected_ck {
            return Err(RarError::format(format!(
                "block checksum mismatch: got {checksum_byte:#x}, expected {expected_ck:#x}"
            )));
        }
        if block_size == 0 {
            return Err(RarError::format("zero-length block"));
        }

        let bits = ((block_size as u64) - 1) * 8 + (1 + bit_size as u64);
        let start_bits = self.reader.byte_position() as u64 * 8 + self.reader.bit_position() as u64;

        let (mut nc, mut dc, mut ldc, mut rc) = (0usize, 0usize, 0usize, 0usize);
        if table_present {
            let (t_nc, t_dc, t_ldc, t_rc) = read_tables(&mut self.reader, self.variant)?;
            nc = t_nc.num_symbols;
            dc = t_dc.num_symbols;
            ldc = t_ldc.num_symbols;
            rc = t_rc.num_symbols;
            self.state.table_nc = Some(t_nc);
            self.state.table_dc = Some(t_dc);
            self.state.table_ldc = Some(t_ldc);
            self.state.table_rc = Some(t_rc);
        }

        self.block = Some(ActiveBlock {
            start_bits,
            bits,
            is_last,
        });
        Ok(BlockInfo {
            block_size,
            table_present,
            nc,
            dc,
            ldc,
            rc,
        })
    }

    /// Decode one symbol. `Ok(None)` means the symbol produced no output
    /// (an invalid repeat) and the caller should keep reading.
    ///
    /// The literal path is the hot one: a symbol below 256 costs one quick
    /// table lookup and a counter bump. Everything else — filters, repeats,
    /// cache moves, distance codes — stays out of line so the literal case
    /// keeps its branch predictor warm and its code in cache.
    #[inline(always)]
    fn decode_symbol(&mut self) -> Option<Symbol> {
        let Some(table) = self.state.table_nc.as_ref() else {
            return self.fail(no_huffman_tables());
        };
        let sym = match decode_symbol(table, &mut self.reader) {
            Ok(sym) => sym,
            Err(msg) => return self.fail(format_err(msg)),
        };
        if sym < 256 {
            self.pos += 1;
            return Some(Symbol::Literal(sym as u8));
        }
        self.decode_non_literal(sym)
    }

    /// The rare half of [`Self::decode_symbol`].
    #[inline(never)]
    fn decode_non_literal(&mut self, sym: usize) -> Option<Symbol> {
        if sym == SYM_FILTER {
            let filter = match parse_filter(&mut self.reader, self.pos) {
                Ok(filter) => filter,
                Err(err) => return self.fail(err),
            };
            self.max_filter_end = self
                .max_filter_end
                .max(filter.block_start.saturating_add(filter.block_length));
            return Some(Symbol::Filter(filter));
        }
        if sym == SYM_REPEAT {
            let dist = self.state.dist_cache[0];
            let len = self.state.last_length;
            if len > 0 && dist > 0 {
                self.pos += len as u64;
                return Some(Symbol::Match {
                    dist,
                    len,
                    kind: MatchKind::Repeat,
                });
            }
            return None;
        }
        if (SYM_CACHE_BASE..=SYM_CACHE_BASE + 3).contains(&sym) {
            let dist = dist_cache_touch(&mut self.state.dist_cache, sym - SYM_CACHE_BASE);
            let Some(table) = self.state.table_rc.as_ref() else {
                return self.fail(no_huffman_tables());
            };
            let len_slot = match decode_symbol(table, &mut self.reader) {
                Ok(slot) => slot,
                Err(msg) => return self.fail(format_err(msg)),
            };
            let length = match decode_length(len_slot, &mut self.reader) {
                Ok(length) => length,
                Err(msg) => return self.fail(format_err(msg)),
            };
            self.state.last_length = length;
            self.pos += length as u64;
            return Some(Symbol::Match {
                dist,
                len: length,
                kind: MatchKind::Cache,
            });
        }

        let len_slot = sym - SYM_MATCH_BASE;
        let mut length = match decode_length(len_slot, &mut self.reader) {
            Ok(length) => length,
            Err(msg) => return self.fail(format_err(msg)),
        };
        let Some(table) = self.state.table_dc.as_ref() else {
            return self.fail(no_huffman_tables());
        };
        let dist_slot = match decode_symbol(table, &mut self.reader) {
            Ok(slot) => slot,
            Err(msg) => return self.fail(format_err(msg)),
        };
        let Some(table) = self.state.table_ldc.as_ref() else {
            return self.fail(no_huffman_tables());
        };
        let dist = match decode_distance(dist_slot, &mut self.reader, table) {
            Ok(dist) => dist,
            Err(msg) => return self.fail(format_err(msg)),
        };
        length = apply_length_bonus_u64(length, dist);
        self.state.last_length = length;
        dist_cache_push(&mut self.state.dist_cache, dist);
        self.pos += length as u64;
        Some(Symbol::Match {
            dist,
            len: length,
            kind: MatchKind::Match,
        })
    }
}

fn no_huffman_tables() -> RarError {
    RarError::format("no Huffman tables defined")
}

/// Bitstream failures carry a static message; only the error path allocates.
#[cold]
fn format_err(e: &'static str) -> RarError {
    RarError::Format(e.to_string())
}
