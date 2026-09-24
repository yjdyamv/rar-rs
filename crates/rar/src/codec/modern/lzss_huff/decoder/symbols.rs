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
        }
    }

    /// Absolute output position of the next symbol.
    #[inline]
    pub(super) fn pos(&self) -> u64 {
        self.pos
    }

    /// Yield the next symbol, or `None` when the member's declared output
    /// size has been produced or the last block ended. A filter region that
    /// outlives the member is malformed and rejected before `None`.
    pub(super) fn next(&mut self) -> RarResult<Option<Symbol>> {
        match self.next_symbol()? {
            Some(symbol) => Ok(Some(symbol)),
            None => {
                if self.max_filter_end > self.end {
                    return Err(RarError::format("unapplied RAR5 filter at end of stream"));
                }
                Ok(None)
            }
        }
    }

    fn next_symbol(&mut self) -> RarResult<Option<Symbol>> {
        loop {
            if self.pos >= self.end {
                return Ok(None);
            }
            if let Some(block) = self.block.as_ref() {
                let start_bits = block.start_bits;
                let bits = block.bits;
                let is_last = block.is_last;
                let cur_bits =
                    self.reader.byte_position() as u64 * 8 + self.reader.bit_position() as u64;
                if cur_bits - start_bits < bits {
                    if let Some(symbol) = self.decode_symbol()? {
                        return Ok(Some(symbol));
                    }
                    continue;
                }
                // Position the reader at the exact end of the block.
                let end_bits = start_bits + bits;
                self.reader
                    .set_position((end_bits / 8) as usize, (end_bits % 8) as u8);
                self.block = None;
                if is_last {
                    return Ok(None);
                }
                continue;
            }
            let info = self.read_block_header()?;
            return Ok(Some(Symbol::BlockStart(info)));
        }
    }

    /// Read a block header, verify its checksum, refresh the Huffman tables
    /// when the block carries them, and start the block.
    fn read_block_header(&mut self) -> RarResult<BlockInfo> {
        let block_flags_byte = self
            .reader
            .read_byte()
            .map_err(|e| RarError::Format(e.to_string()))?;
        let table_present = (block_flags_byte >> 7) & 1 != 0;
        let is_last = (block_flags_byte >> 6) & 1 != 0;
        let byte_count = ((block_flags_byte >> 3) & 3) + 1;
        let bit_size = block_flags_byte & 7;

        let checksum_byte = self
            .reader
            .read_byte()
            .map_err(|e| RarError::Format(e.to_string()))?;
        let size_bytes = self
            .reader
            .read_bytes(byte_count as usize)
            .map_err(|e| RarError::Format(e.to_string()))?;
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
    fn decode_symbol(&mut self) -> RarResult<Option<Symbol>> {
        let sym = {
            let table = self.state.table_nc.as_ref().ok_or_else(no_huffman_tables)?;
            decode_symbol(table, &mut self.reader).map_err(|e| RarError::Format(e.to_string()))?
        };

        if sym < 256 {
            self.pos += 1;
            return Ok(Some(Symbol::Literal(sym as u8)));
        }
        if sym == SYM_FILTER {
            let filter = parse_filter(&mut self.reader, self.pos)?;
            self.max_filter_end = self
                .max_filter_end
                .max(filter.block_start.saturating_add(filter.block_length));
            return Ok(Some(Symbol::Filter(filter)));
        }
        if sym == SYM_REPEAT {
            let dist = self.state.dist_cache[0];
            let len = self.state.last_length;
            if len > 0 && dist > 0 {
                self.pos += len as u64;
                return Ok(Some(Symbol::Match {
                    dist,
                    len,
                    kind: MatchKind::Repeat,
                }));
            }
            return Ok(None);
        }
        if (SYM_CACHE_BASE..=SYM_CACHE_BASE + 3).contains(&sym) {
            let dist = dist_cache_touch(&mut self.state.dist_cache, sym - SYM_CACHE_BASE);
            let len_slot = {
                let table = self.state.table_rc.as_ref().ok_or_else(no_huffman_tables)?;
                decode_symbol(table, &mut self.reader)
                    .map_err(|e| RarError::Format(e.to_string()))?
            };
            let length = decode_length(len_slot, &mut self.reader)?;
            self.state.last_length = length;
            self.pos += length as u64;
            return Ok(Some(Symbol::Match {
                dist,
                len: length,
                kind: MatchKind::Cache,
            }));
        }

        let len_slot = sym - SYM_MATCH_BASE;
        let mut length = decode_length(len_slot, &mut self.reader)?;
        let dist_slot = {
            let table = self.state.table_dc.as_ref().ok_or_else(no_huffman_tables)?;
            decode_symbol(table, &mut self.reader).map_err(|e| RarError::Format(e.to_string()))?
        };
        let dist = {
            let table = self
                .state
                .table_ldc
                .as_ref()
                .ok_or_else(no_huffman_tables)?;
            decode_distance(dist_slot, &mut self.reader, table)?
        };
        length = apply_length_bonus_u64(length, dist);
        self.state.last_length = length;
        dist_cache_push(&mut self.state.dist_cache, dist);
        self.pos += length as u64;
        Ok(Some(Symbol::Match {
            dist,
            len: length,
            kind: MatchKind::Match,
        }))
    }
}

fn no_huffman_tables() -> RarError {
    RarError::format("no Huffman tables defined")
}
