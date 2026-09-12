//! Public decode entry points and the streaming state machine.
//!
//! `decode_raw` dispatches to [`decode_standalone`] or [`decode_inner`];
//! [`decode_to_writer`] streams through `OutputSink`, which applies pending
//! filters before bytes leave and bounds held-back output by
//! [`MAX_STREAMING_FILTER_BUFFER`]. The solid-chain entry point
//! ([`decode_raw`] with a state) reuses the window/tables owned by
//! [`super::DecoderState`]. Bit-level reading lives in [`super::tables`].

use super::*;

use super::super::{BLOCK_CHECKSUM_SEED, SYM_CACHE_BASE, SYM_FILTER, SYM_MATCH_BASE, SYM_REPEAT};
use super::tables::{
    apply_length_bonus_u64, decode_distance, decode_length, dist_cache_push, dist_cache_touch,
    parse_filter, read_tables,
};
use crate::codec::common::bitstream::BitReader;
use crate::codec::common::filters::apply_filter_decode;
use crate::codec::common::huffman::decode_symbol;
use crate::error::{RarError, RarResult};
/// Decode RAR5 compressed data into a buffer.
///
/// - `data`: raw compressed bytes (the data area from the file block)
/// - `unpacked_size`: expected decompressed size in bytes
pub fn decode_raw(data: &[u8], unpacked_size: u64, opts: DecodeOptions<'_>) -> RarResult<Vec<u8>> {
    let mut reader = BitReader::new(data);

    match opts.state {
        Some(st) => decode_inner(
            &mut reader,
            unpacked_size,
            &mut st.window,
            &mut st.dist_cache,
            &mut st.last_length,
            &mut st.prev_low_dist,
            &mut st.table_nc,
            &mut st.table_dc,
            &mut st.table_ldc,
            &mut st.table_rc,
            opts.variant,
        ),
        None => decode_standalone(
            data,
            unpacked_size,
            opts.dict_size_log,
            opts.dict_size_bytes,
            opts.variant,
        ),
    }
}

/// Maximum bytes of decompressed output held back for RAR5 filters during
/// streaming decode. Filtered regions beyond this are rejected instead of
/// buffering the whole member.
pub const MAX_STREAMING_FILTER_BUFFER: u64 = 64 * 1024 * 1024;

/// Decode RAR5 compressed data, streaming output to `writer` instead of
/// allocating the whole member. Returns the number of bytes written.
///
/// Filters are applied before bytes leave the stream; the total held-back
/// region is bounded by [`MAX_STREAMING_FILTER_BUFFER`].
pub fn decode_to_writer(
    data: &[u8],
    unpacked_size: u64,
    opts: DecodeOptions<'_>,
    writer: &mut dyn std::io::Write,
) -> RarResult<u64> {
    if unpacked_size == 0 {
        return Ok(0);
    }
    match opts.state {
        Some(st) => decode_inner_streaming(
            &mut BitReader::new(data),
            unpacked_size,
            &mut st.window,
            &mut st.dist_cache,
            &mut st.last_length,
            &mut st.prev_low_dist,
            &mut st.table_nc,
            &mut st.table_dc,
            &mut st.table_ldc,
            &mut st.table_rc,
            opts.variant,
            writer,
        ),
        None => decode_standalone_to_writer(
            data,
            unpacked_size,
            opts.dict_size_log,
            opts.dict_size_bytes,
            opts.variant,
            writer,
        ),
    }
}

/// Streaming variant of [`decode_standalone`].
pub fn decode_standalone_to_writer(
    data: &[u8],
    unpacked_size: u64,
    dict_size_log: u8,
    dict_size_bytes: Option<u64>,
    variant: ArchiveVersion,
    writer: &mut dyn std::io::Write,
) -> RarResult<u64> {
    let dict_size = checked_dict_size(dict_size_log, dict_size_bytes)?;
    let mut reader = BitReader::new(data);
    let mut window = SlidingWindow::new(dict_size);
    let mut dist_cache = [0u64; DIST_CACHE_SIZE];
    let mut last_length = 0u32;
    let mut prev_low_dist = 0u32;
    let mut table_nc: Option<DecodeTable> = None;
    let mut table_dc: Option<DecodeTable> = None;
    let mut table_ldc: Option<DecodeTable> = None;
    let mut table_rc: Option<DecodeTable> = None;

    decode_inner_streaming(
        &mut reader,
        unpacked_size,
        &mut window,
        &mut dist_cache,
        &mut last_length,
        &mut prev_low_dist,
        &mut table_nc,
        &mut table_dc,
        &mut table_ldc,
        &mut table_rc,
        variant,
        writer,
    )
}

/// Compute and validate a decoder dictionary size.
///
/// For RAR5 (`dict_size_bytes == None`) the size comes from the 4-bit log
/// field (up to 4 GiB); for RAR7 the actual byte count is given (up to
/// 126 GiB, possibly non-power-of-two — the window rounds up to a power of
/// two so the circular buffer keeps its fast mask arithmetic).
pub(super) fn checked_dict_size(
    dict_size_log: u8,
    dict_size_bytes: Option<u64>,
) -> RarResult<usize> {
    let bytes = match dict_size_bytes {
        Some(bytes) => bytes,
        None => {
            if dict_size_log > 15 {
                return Err(RarError::Format(format!(
                    "dictionary size log {dict_size_log} exceeds supported maximum 15"
                )));
            }
            (128u64 * 1024) << dict_size_log
        }
    };
    let bytes_usize = usize::try_from(bytes).map_err(|_| {
        RarError::Format(format!(
            "dictionary size {bytes} overflows host address space"
        ))
    })?;
    bytes_usize.checked_next_power_of_two().ok_or_else(|| {
        RarError::Format(format!(
            "dictionary size {bytes} overflows host address space"
        ))
    })
}

/// Streaming decode core: writes decoded (and filtered) output to `writer`.
#[allow(clippy::too_many_arguments)]
fn decode_inner_streaming(
    reader: &mut BitReader,
    unpacked_size: u64,
    window: &mut SlidingWindow,
    dist_cache: &mut [u64; DIST_CACHE_SIZE],
    last_length: &mut u32,
    prev_low_dist: &mut u32,
    table_nc: &mut Option<DecodeTable>,
    table_dc: &mut Option<DecodeTable>,
    table_ldc: &mut Option<DecodeTable>,
    table_rc: &mut Option<DecodeTable>,
    variant: ArchiveVersion,
    writer: &mut dyn std::io::Write,
) -> RarResult<u64> {
    const COPY_THRESHOLD: u64 = 64 * 1024;

    let mut pending_filters: Vec<PendingFilter> = Vec::new();
    let output_start = window.total_written();
    let mut sink = OutputSink::new(writer, output_start);
    let mut copied_abs = output_start;

    while (window.total_written() - output_start) < unpacked_size {
        // ── Read block header ──────────────────────────────────────────
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

        let block_size_bytes = reader
            .read_bytes(byte_count as usize)
            .map_err(|e| RarError::Format(e.to_string()))?;
        let mut block_size: u32 = 0;
        for (i, &b) in block_size_bytes.iter().enumerate() {
            block_size |= (b as u32) << (i * 8);
        }

        let mut expected_ck = BLOCK_CHECKSUM_SEED ^ block_flags_byte;
        for &b in block_size_bytes {
            expected_ck ^= b;
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

        if table_present {
            let (nc, dc, ldc, rc) = read_tables(reader, variant)?;
            *table_nc = Some(nc);
            *table_dc = Some(dc);
            *table_ldc = Some(ldc);
            *table_rc = Some(rc);
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

        // ── Decode symbols ─────────────────────────────────────────────
        while (window.total_written() - output_start) < unpacked_size {
            let cur_bits = reader.byte_position() as u64 * 8 + reader.bit_position() as u64;
            if cur_bits - block_start_bits >= block_bits {
                break;
            }

            let sym = decode_symbol(t_nc, reader).map_err(|e| RarError::Format(e.to_string()))?;

            if sym < 256 {
                window.put_byte(sym as u8);
            } else if sym == SYM_FILTER {
                let filt = parse_filter(reader, window.total_written())?;
                pending_filters.push(filt);
            } else if sym == SYM_REPEAT {
                if *last_length > 0 && dist_cache[0] > 0 {
                    window.copy_match(dist_cache[0] as usize, *last_length as usize);
                }
            } else if (SYM_CACHE_BASE..=SYM_CACHE_BASE + 3).contains(&sym) {
                let cache_idx = sym - SYM_CACHE_BASE;
                let dist = dist_cache_touch(dist_cache, cache_idx);
                let len_slot =
                    decode_symbol(t_rc, reader).map_err(|e| RarError::Format(e.to_string()))?;
                let length = decode_length(len_slot, reader)?;
                *last_length = length;
                *prev_low_dist = (dist & 0xF) as u32;
                window.copy_match(dist as usize, length as usize);
            } else if sym >= SYM_MATCH_BASE {
                let len_slot = sym - SYM_MATCH_BASE;
                let mut length = decode_length(len_slot, reader)?;
                let dist_slot =
                    decode_symbol(t_dc, reader).map_err(|e| RarError::Format(e.to_string()))?;
                let dist = decode_distance(dist_slot, reader, t_ldc)?;
                length = apply_length_bonus_u64(length, dist);
                *last_length = length;
                *prev_low_dist = (dist & 0xF) as u32;
                dist_cache_push(dist_cache, dist);
                window.copy_match(dist as usize, length as usize);
            }

            // Copy newly produced window bytes into staging before the
            // ring can overwrite them, then drain as far as filters allow.
            let written = window.total_written();
            if written - copied_abs >= COPY_THRESHOLD {
                sink.append_window(window, copied_abs, written)?;
                copied_abs = written;
                sink.apply_complete_filters(&mut pending_filters)?;
                sink.drain_up_to(window.total_written(), &pending_filters)?;
            }
        }

        // Position reader at exact end of block
        let block_end_bits = block_start_bits + block_bits;
        reader.set_position((block_end_bits / 8) as usize, (block_end_bits % 8) as u8);

        if is_last_block {
            break;
        }
    }

    let written = window.total_written();
    if written > copied_abs {
        sink.append_window(window, copied_abs, written)?;
    }
    sink.apply_complete_filters(&mut pending_filters)?;
    sink.drain_up_to(written, &pending_filters)?;

    // Any filter whose region was never produced is malformed.
    if pending_filters.iter().any(|f| !f.applied) {
        return Err(RarError::Format(
            "unapplied RAR5 filter at end of stream".into(),
        ));
    }
    if sink.staging_len() != 0 {
        return Err(RarError::Format(
            "internal streaming decode staging error".into(),
        ));
    }
    let produced = written - output_start;
    if produced != unpacked_size {
        return Err(RarError::Format(format!(
            "decompressed size mismatch: expected {unpacked_size}, got {produced}"
        )));
    }
    Ok(produced)
}

/// Buffered output staging for streaming decode.
///
/// Holds decoded bytes that cannot yet be written (RAR5 filters transform
/// regions before they leave) and flushes the rest to the underlying writer.
struct OutputSink<'a> {
    writer: &'a mut dyn std::io::Write,
    staging: Vec<u8>,
    staging_start: u64,
    /// Absolute stream position where this member's output starts; the
    /// base for member-relative filter transform offsets.
    member_start: u64,
    consumed: usize,
}

impl<'a> OutputSink<'a> {
    fn new(writer: &'a mut dyn std::io::Write, start: u64) -> Self {
        Self {
            writer,
            staging: Vec::new(),
            staging_start: start,
            member_start: start,
            consumed: 0,
        }
    }

    fn staging_len(&self) -> usize {
        self.staging.len() - self.consumed
    }

    fn append_window(&mut self, window: &SlidingWindow, from: u64, to: u64) -> RarResult<()> {
        if to <= from {
            return Ok(());
        }
        let bytes = window.get_output(from, (to - from) as usize);
        if self.staging_len() + bytes.len() > MAX_STREAMING_FILTER_BUFFER as usize {
            return Err(RarError::Format(format!(
                "filtered output region exceeds streaming buffer limit {}",
                MAX_STREAMING_FILTER_BUFFER
            )));
        }
        self.staging.extend_from_slice(&bytes);
        Ok(())
    }

    fn apply_complete_filters(&mut self, pending: &mut [PendingFilter]) -> RarResult<()> {
        for filt in pending.iter_mut().filter(|f| !f.applied) {
            let staging_end = self.staging_start + (self.staging_len() as u64);
            if staging_end < filt.block_start + filt.block_length {
                continue; // region not fully produced yet
            }
            let start_off = (filt.block_start - self.staging_start) as usize;
            let end_off = start_off + filt.block_length as usize;
            // `consumed` bytes were already written out but not yet
            // compacted, so the staging slot for `staging_start` is at
            // index `consumed`, not 0.
            let base = self.consumed;
            if base + end_off > self.staging.len() {
                return Err(RarError::Format(
                    "filter region out of staging bounds".into(),
                ));
            }
            let region = &mut self.staging[base + start_off..base + end_off];
            // The E8/ARM inverse transforms read a file-relative position
            // (WinRAR's `WrittenFileSize` is per-file), while `block_start`
            // is stream-absolute for solid chains — subtract the member
            // start to get the member-relative offset. `staging_start`
            // advances as data drains, so it is not a valid base here.
            let filtered = apply_filter_decode(
                filt.filter_type,
                region,
                filt.channels,
                filt.block_start - self.member_start,
            )
            .map_err(RarError::Format)?;
            if filtered.len() != region.len() {
                return Err(RarError::Format("RAR5 filter changed output length".into()));
            }
            region.copy_from_slice(&filtered);
            filt.applied = true;
        }
        Ok(())
    }

    fn drain_up_to(&mut self, written: u64, pending: &[PendingFilter]) -> RarResult<()> {
        let earliest_filter = pending
            .iter()
            .filter(|f| !f.applied)
            .map(|f| f.block_start)
            .min()
            .unwrap_or(written);
        let drain_to = earliest_filter.min(written);
        let n = (drain_to - self.staging_start) as usize;
        if n > self.staging_len() {
            return Err(RarError::Format("internal drain beyond staging".into()));
        }
        if n > 0 {
            self.writer
                .write_all(&self.staging[self.consumed..self.consumed + n])
                .map_err(|e| RarError::Format(e.to_string()))?;
            self.consumed += n;
            self.staging_start += n as u64;
            if self.consumed > 1024 * 1024 || self.consumed == self.staging.len() {
                self.staging.drain(..self.consumed);
                self.consumed = 0;
            }
        }
        Ok(())
    }
}

/// Decode RAR5/RAR7 compressed data (standalone, no solid state).
pub fn decode_standalone(
    data: &[u8],
    unpacked_size: u64,
    dict_size_log: u8,
    dict_size_bytes: Option<u64>,
    variant: ArchiveVersion,
) -> RarResult<Vec<u8>> {
    let mut dict_size = checked_dict_size(dict_size_log, dict_size_bytes)?;
    // The decoder reconstructs the whole file in the sliding window before
    // extracting it (see `get_output`), so the window must be at least as
    // large as the unpacked output. The encoder sizes its dictionary to the
    // input (WinRAR-style, capped at 2x the file size), so grow the decode
    // buffer here instead of reverting that cap.
    let unpacked = usize::try_from(unpacked_size)
        .map_err(|_| RarError::Format("unpacked size overflows host address space".into()))?;
    if unpacked > dict_size {
        dict_size = unpacked.checked_next_power_of_two().ok_or_else(|| {
            RarError::Format("unpacked size too large for host address space".into())
        })?;
    }

    let mut reader = BitReader::new(data);
    let mut window = SlidingWindow::new(dict_size);
    let mut dist_cache = [0u64; DIST_CACHE_SIZE];
    let mut last_length = 0u32;
    let mut prev_low_dist = 0u32;
    let mut table_nc: Option<DecodeTable> = None;
    let mut table_dc: Option<DecodeTable> = None;
    let mut table_ldc: Option<DecodeTable> = None;
    let mut table_rc: Option<DecodeTable> = None;

    decode_inner(
        &mut reader,
        unpacked_size,
        &mut window,
        &mut dist_cache,
        &mut last_length,
        &mut prev_low_dist,
        &mut table_nc,
        &mut table_dc,
        &mut table_ldc,
        &mut table_rc,
        variant,
    )
}

#[allow(clippy::too_many_arguments)]
fn decode_inner(
    reader: &mut BitReader,
    unpacked_size: u64,
    window: &mut SlidingWindow,
    dist_cache: &mut [u64; DIST_CACHE_SIZE],
    last_length: &mut u32,
    prev_low_dist: &mut u32,
    table_nc: &mut Option<DecodeTable>,
    table_dc: &mut Option<DecodeTable>,
    table_ldc: &mut Option<DecodeTable>,
    table_rc: &mut Option<DecodeTable>,
    variant: ArchiveVersion,
) -> RarResult<Vec<u8>> {
    let mut pending_filters: Vec<PendingFilter> = Vec::new();
    let output_start = window.total_written();

    while (window.total_written() - output_start) < unpacked_size {
        // ── Read block header ──────────────────────────────────────────
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

        let block_size_bytes = reader
            .read_bytes(byte_count as usize)
            .map_err(|e| RarError::Format(e.to_string()))?;
        let mut block_size: u32 = 0;
        for (i, &b) in block_size_bytes.iter().enumerate() {
            block_size |= (b as u32) << (i * 8);
        }

        // Verify checksum
        let mut expected_ck = BLOCK_CHECKSUM_SEED ^ block_flags_byte;
        for &b in block_size_bytes {
            expected_ck ^= b;
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

        // ── Read Huffman tables if present ──────────────────────────────
        if table_present {
            let (nc, dc, ldc, rc) = read_tables(reader, variant)?;
            *table_nc = Some(nc);
            *table_dc = Some(dc);
            *table_ldc = Some(ldc);
            *table_rc = Some(rc);
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

        // ── Decode symbols ─────────────────────────────────────────────
        while (window.total_written() - output_start) < unpacked_size {
            let cur_bits = reader.byte_position() as u64 * 8 + reader.bit_position() as u64;
            if cur_bits - block_start_bits >= block_bits {
                break;
            }

            let sym = decode_symbol(t_nc, reader).map_err(|e| RarError::Format(e.to_string()))?;

            if sym < 256 {
                window.put_byte(sym as u8);
            } else if sym == SYM_FILTER {
                let filt = parse_filter(reader, window.total_written())?;
                pending_filters.push(filt);
            } else if sym == SYM_REPEAT {
                if *last_length > 0 && dist_cache[0] > 0 {
                    window.copy_match(dist_cache[0] as usize, *last_length as usize);
                }
            } else if (SYM_CACHE_BASE..=SYM_CACHE_BASE + 3).contains(&sym) {
                let cache_idx = sym - SYM_CACHE_BASE;
                let dist = dist_cache_touch(dist_cache, cache_idx);
                let len_slot =
                    decode_symbol(t_rc, reader).map_err(|e| RarError::Format(e.to_string()))?;
                let length = decode_length(len_slot, reader)?;
                *last_length = length;
                *prev_low_dist = (dist & 0xF) as u32;
                window.copy_match(dist as usize, length as usize);
            } else if sym >= SYM_MATCH_BASE {
                let len_slot = sym - SYM_MATCH_BASE;
                let mut length = decode_length(len_slot, reader)?;
                let dist_slot =
                    decode_symbol(t_dc, reader).map_err(|e| RarError::Format(e.to_string()))?;
                let dist = decode_distance(dist_slot, reader, t_ldc)?;
                length = apply_length_bonus_u64(length, dist);
                *last_length = length;
                *prev_low_dist = (dist & 0xF) as u32;
                dist_cache_push(dist_cache, dist);
                window.copy_match(dist as usize, length as usize);
            }
        }

        // Position reader at exact end of block
        let block_end_bits = block_start_bits + block_bits;
        reader.set_position((block_end_bits / 8) as usize, (block_end_bits % 8) as u8);

        if is_last_block {
            break;
        }
    }

    // Extract output
    let produced = window.total_written() - output_start;
    if produced != unpacked_size {
        // The streaming path rejects this too (see `decode_inner_streaming`):
        // a packed stream that ends early must not surface as a silently
        // truncated member.
        return Err(RarError::Format(format!(
            "decompressed size mismatch: expected {unpacked_size}, got {produced}"
        )));
    }
    let written = produced.min(unpacked_size);
    let mut output = window.get_output(output_start, written as usize);

    // Apply pending filters. RAR5 filter positions are stream-absolute
    // (relative to the solid chain), but the E8/ARM transforms read a
    // position relative to the current file's output (WinRAR's
    // `WrittenFileSize`, reset per file), so the offset passed to the
    // inverse filter is member-relative: `block_start - output_start`.
    for filt in &pending_filters {
        let start = (filt.block_start - output_start) as usize;
        let end = (start + filt.block_length as usize).min(output.len());
        if start >= output.len() {
            continue;
        }
        let region = &mut output[start..end];
        let filtered = apply_filter_decode(
            filt.filter_type,
            region,
            filt.channels,
            filt.block_start - output_start,
        )
        .map_err(RarError::Format)?;
        output[start..start + filtered.len()].copy_from_slice(&filtered);
    }

    output.truncate(unpacked_size as usize);
    Ok(output)
}
