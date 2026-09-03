//! RAR5 decoder: LZSS+Huffman decompression.
//!
//! Clean-room implementation for software conservation and educational
//! purposes.
//!
//! License: BSD-2-Clause

use super::*;
use crate::codec::bitstream::BitReader;
use crate::codec::filters::apply_filter_decode;
use crate::codec::huffman::{DecodeTable, decode_symbol};
use crate::codec::window::SlidingWindow;
use crate::error::{RarError, RarResult};
use crate::version::ArchiveVersion;

// ── Decoder ────────────────────────────────────────────────────────────────

// ── Pending Filter ─────────────────────────────────────────────────────────

struct PendingFilter {
    filter_type: u8,
    block_start: u64,
    block_length: u64,
    channels: u8,
    applied: bool,
}

// ── Decoder State (for solid archives) ─────────────────────────────────────

/// Persistent decoder state for solid archive support.
///
/// In a solid archive, the sliding window, distance cache, and Huffman
/// tables carry over between files. The fields are codec-private; the
/// archive layer only creates and holds the state.
pub struct DecoderState {
    window: SlidingWindow,
    dist_cache: [u64; DIST_CACHE_SIZE],
    last_length: u32,
    prev_low_dist: u32,
    table_nc: Option<DecodeTable>,
    table_dc: Option<DecodeTable>,
    table_ldc: Option<DecodeTable>,
    table_rc: Option<DecodeTable>,
}

impl DecoderState {
    pub fn new(dict_size: usize) -> Self {
        DecoderState {
            window: SlidingWindow::new(dict_size),
            dist_cache: [0; DIST_CACHE_SIZE],
            last_length: 0,
            prev_low_dist: 0,
            table_nc: None,
            table_dc: None,
            table_ldc: None,
            table_rc: None,
        }
    }
}

// ── Public decode entry points ─────────────────────────────────────────────

/// Options for decoding one member.
///
/// `dict_size_log` sizes the window for standalone members only: when
/// `state` is carried (solid chains), the state owns its window and the
/// log is ignored by construction.
#[derive(Default)]
pub struct DecodeOptions<'a> {
    /// Dictionary size as log2(size/128KB), 0 = 128KB. Used when `state`
    /// is `None`.
    pub dict_size_log: u8,
    /// Actual dictionary size in bytes (RAR7, `comp_version` 1): may be
    /// non-power-of-two; the window rounds up to a power of two.
    pub dict_size_bytes: Option<u64>,
    /// RAR7 algorithm variant (extended distance codes, `v70`).
    pub variant: ArchiveVersion,
    /// Shared decoder state for solid-chain continuity (`None` for
    /// standalone members).
    pub state: Option<&'a mut DecoderState>,
}

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
/// 64 GiB, possibly non-power-of-two — the window rounds up to a power of
/// two so the circular buffer keeps its fast mask arithmetic).
fn checked_dict_size(dict_size_log: u8, dict_size_bytes: Option<u64>) -> RarResult<usize> {
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
    let written = (window.total_written() - output_start).min(unpacked_size);
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

// ── Huffman Table Reading ──────────────────────────────────────────────────

fn read_tables(
    reader: &mut BitReader,
    variant: ArchiveVersion,
) -> RarResult<(DecodeTable, DecodeTable, DecodeTable, DecodeTable)> {
    // RAR7 (v70) extends the distance code table from 64 to 80 codes.
    let dc_count = if variant.uses_extra_dist() {
        HUFF_DCX
    } else {
        HUFF_DC
    };
    // Read BC table: 20 code lengths as nibbles with escape mechanism
    let mut bc_lengths = Vec::with_capacity(HUFF_BC);
    while bc_lengths.len() < HUFF_BC {
        let val = reader
            .read_bits(4)
            .map_err(|e| RarError::Format(e.to_string()))? as u8;
        if val == NIBBLE_ESCAPE {
            let next_val = reader
                .read_bits(4)
                .map_err(|e| RarError::Format(e.to_string()))? as u8;
            if next_val == 0 {
                bc_lengths.push(15);
            } else {
                for _ in 0..(next_val as usize + 2) {
                    if bc_lengths.len() < HUFF_BC {
                        bc_lengths.push(0);
                    }
                }
            }
        } else {
            bc_lengths.push(val);
        }
    }

    let table_bc = DecodeTable::new(&bc_lengths);

    let total = HUFF_NC + dc_count + HUFF_LDC + HUFF_RC;
    let all_lengths = read_code_lengths(reader, &table_bc, total)?;

    let nc_len = &all_lengths[..HUFF_NC];
    let dc_len = &all_lengths[HUFF_NC..HUFF_NC + dc_count];
    let ldc_len = &all_lengths[HUFF_NC + dc_count..HUFF_NC + dc_count + HUFF_LDC];
    let rc_len = &all_lengths[HUFF_NC + dc_count + HUFF_LDC..];

    Ok((
        DecodeTable::new(nc_len),
        DecodeTable::new(dc_len),
        DecodeTable::new(ldc_len),
        DecodeTable::new(rc_len),
    ))
}

fn read_code_lengths(
    reader: &mut BitReader,
    bc_table: &DecodeTable,
    count: usize,
) -> RarResult<Vec<u8>> {
    let mut lengths = vec![0u8; count];
    let mut i = 0;
    while i < count {
        let sym = decode_symbol(bc_table, reader).map_err(|e| RarError::Format(e.to_string()))?;
        if sym < 16 {
            lengths[i] = sym as u8;
            i += 1;
        } else if sym < 18 {
            if i == 0 {
                return Err(RarError::Format(
                    "run-length repeat with no previous length".into(),
                ));
            }
            let repeat = if sym == 16 {
                3 + reader
                    .read_bits(3)
                    .map_err(|e| RarError::Format(e.to_string()))? as usize
            } else {
                11 + reader
                    .read_bits(7)
                    .map_err(|e| RarError::Format(e.to_string()))? as usize
            };
            let prev = lengths[i - 1];
            for _ in 0..repeat {
                if i >= count {
                    break;
                }
                lengths[i] = prev;
                i += 1;
            }
        } else {
            let repeat = if sym == 18 {
                3 + reader
                    .read_bits(3)
                    .map_err(|e| RarError::Format(e.to_string()))? as usize
            } else {
                11 + reader
                    .read_bits(7)
                    .map_err(|e| RarError::Format(e.to_string()))? as usize
            };
            for _ in 0..repeat {
                if i >= count {
                    break;
                }
                lengths[i] = 0;
                i += 1;
            }
        }
    }
    Ok(lengths)
}

// ── Length/Distance Decoding ───────────────────────────────────────────────

fn decode_length(slot: usize, reader: &mut BitReader) -> RarResult<u32> {
    if slot < 8 {
        Ok(2 + slot as u32)
    } else {
        let lbits = (slot / 4 - 1) as u8;
        let base = 2 + ((4 | (slot & 3)) << lbits) as u32;
        if lbits > 0 {
            let extra = reader
                .read_bits(lbits)
                .map_err(|e| RarError::Format(e.to_string()))?;
            Ok(base + extra)
        } else {
            Ok(base)
        }
    }
}

fn decode_distance(
    dist_slot: usize,
    reader: &mut BitReader,
    table_ldc: &DecodeTable,
) -> RarResult<u64> {
    // RAR7's extended table (80 codes) reaches dist slots up to 79, i.e.
    // DBits up to 38 and distances beyond 4 GB — hence u64 arithmetic.
    if dist_slot < 4 {
        Ok(1 + dist_slot as u64)
    } else {
        let dbits = (dist_slot / 2 - 1) as u8;
        let mut dist = 1u64 + (((2 | (dist_slot & 1)) as u64) << dbits);

        if dbits > 0 {
            if dbits >= 4 {
                if dbits > 4 {
                    let upper = reader
                        .read_bits(dbits - 4)
                        .map_err(|e| RarError::Format(e.to_string()))?;
                    dist = dist.wrapping_add((upper as u64) << 4);
                }
                let low_dist = decode_symbol(table_ldc, reader)
                    .map_err(|e| RarError::Format(e.to_string()))?;
                dist = dist.wrapping_add(low_dist as u64);
            } else {
                let extra = reader
                    .read_bits(dbits)
                    .map_err(|e| RarError::Format(e.to_string()))?;
                dist = dist.wrapping_add(extra as u64);
            }
        }
        Ok(dist)
    }
}

// ── Distance Cache ─────────────────────────────────────────────────────────

fn dist_cache_push(cache: &mut [u64; DIST_CACHE_SIZE], value: u64) {
    cache[3] = cache[2];
    cache[2] = cache[1];
    cache[1] = cache[0];
    cache[0] = value;
}

fn dist_cache_touch(cache: &mut [u64; DIST_CACHE_SIZE], idx: usize) -> u64 {
    let value = cache[idx];
    for i in (1..=idx).rev() {
        cache[i] = cache[i - 1];
    }
    cache[0] = value;
    value
}

// ── Length Bonus ────────────────────────────────────────────────────────────

fn apply_length_bonus_u64(length: u32, dist: u64) -> u32 {
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

// ── Filter Parsing ─────────────────────────────────────────────────────────

fn parse_filter(reader: &mut BitReader, write_pos: u64) -> RarResult<PendingFilter> {
    let block_start = write_pos + parse_filter_data(reader)? as u64;
    let block_length = parse_filter_data(reader)? as u64;
    let filter_type = reader
        .read_bits(3)
        .map_err(|e| RarError::Format(e.to_string()))? as u8;

    let channels = if filter_type == FILTER_DELTA {
        reader
            .read_bits(5)
            .map_err(|e| RarError::Format(e.to_string()))? as u8
            + 1
    } else {
        0
    };

    Ok(PendingFilter {
        filter_type,
        block_start,
        block_length,
        channels,
        applied: false,
    })
}

fn parse_filter_data(reader: &mut BitReader) -> RarResult<u32> {
    let byte_count = reader
        .read_bits(2)
        .map_err(|e| RarError::Format(e.to_string()))?
        + 1;
    let mut value: u32 = 0;
    for i in 0..byte_count {
        let b = reader
            .read_bits(8)
            .map_err(|e| RarError::Format(e.to_string()))?;
        value |= b << (i * 8);
    }
    Ok(value)
}

#[cfg(test)]
mod decode_tests {
    use super::*;

    /// The RAR5 format ceiling is 4 GiB (log 15); RAR7 sizes come from
    /// the byte count. Larger values are rejected.
    #[test]
    fn checked_dict_size_accepts_range_and_rejects_larger() {
        assert_eq!(checked_dict_size(0, None).unwrap(), 128 * 1024);
        assert_eq!(checked_dict_size(13, None).unwrap(), 1024 * 1024 * 1024);
        assert_eq!(checked_dict_size(15, None).unwrap(), 4 * 1024 * 1024 * 1024);
        let err = checked_dict_size(16, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("maximum 15"), "{msg}");
        // RAR7: byte count, non-power-of-two rounds the window up.
        assert_eq!(
            checked_dict_size(0, Some(6 * 1024 * 1024 * 1024)).unwrap(),
            8 * 1024 * 1024 * 1024
        );
    }

    /// Fuzz regression: block flags with the reserved bit 5 set previously
    /// produced a byte_count of 8, overflowing the u32 block-size shift in
    /// debug builds. The block size field is 2 bits (1-4 bytes); reserved
    /// bits are ignored.
    #[test]
    fn block_flags_with_reserved_bit_do_not_overflow() {
        let stream = [0xe4u8, 0x00, 0xe0, 0x00, 0xe0, 0x00, 0x00];
        let result = std::panic::catch_unwind(|| {
            let _ = decode_standalone(&stream, 78090, 0, None, ArchiveVersion::Rar50);
        });
        assert!(
            result.is_ok(),
            "decode must not panic on reserved flag bits"
        );
    }

    /// Fuzz regression: a valid archive with a 3-byte block size field
    /// (byte_count 3) still decodes.
    #[test]
    fn three_byte_block_size_field_decodes() {
        let data = b"rar5 three-byte block size regression test data ".repeat(64);
        let packed = crate::codec::encode_raw(&data, 3, 3, ArchiveVersion::Rar50);
        let back =
            decode_standalone(&packed, data.len() as u64, 3, None, ArchiveVersion::Rar50).unwrap();
        assert_eq!(back, data);
    }

    /// Regression: streaming decode (used by `extract_all`) applied split
    /// filter records at the wrong staging offset once part of the staging
    /// buffer had already been written out. Members whose filter region
    /// exceeds MAX_FILTER_BLOCK_LENGTH are split into multiple records, so
    /// the streaming path must produce byte-identical output to the
    /// buffered path for every filter type.
    #[test]
    fn streaming_decode_matches_buffered_for_split_filter_records() {
        use super::{FILTER_ARM, FILTER_DELTA, FILTER_E8, FILTER_E8E9};
        use super::{FilterSpec, MAX_FILTER_BLOCK_LENGTH, encode_with_filters};

        fn pattern(filter_type: u8, size: usize) -> Vec<u8> {
            match filter_type {
                FILTER_E8 | FILTER_E8E9 => {
                    let mut data = vec![0x90u8; size];
                    let mut pos = 0usize;
                    while pos + 5 <= size {
                        data[pos] = if filter_type == FILTER_E8 || pos.is_multiple_of(170) {
                            0xE8
                        } else {
                            0xE9
                        };
                        let addr = ((pos as u32).wrapping_mul(7)) & 0x00FF_FFFF;
                        data[pos + 1..pos + 5].copy_from_slice(&addr.to_le_bytes());
                        pos += 85;
                    }
                    data
                }
                FILTER_ARM => {
                    let mut data = vec![0x00u8; size];
                    let mut pos = 0usize;
                    while pos + 4 <= size {
                        data[pos + 3] = 0xEB;
                        let off = ((pos as u32).wrapping_mul(3)) & 0x00FF_FFFF;
                        data[pos..pos + 3].copy_from_slice(&off.to_le_bytes()[..3]);
                        pos += 64;
                    }
                    data
                }
                _ => (0..size).map(|i| (i % 251) as u8).collect(),
            }
        }

        let cases: [(u8, u8); 5] = [
            (FILTER_E8, 0),
            (FILTER_E8E9, 0),
            (FILTER_ARM, 0),
            (FILTER_DELTA, 1),
            (FILTER_DELTA, 4),
        ];
        for &(filter_type, channels) in &cases {
            for &size in &[MAX_FILTER_BLOCK_LENGTH as usize + 1, 300_000usize] {
                let data = pattern(filter_type, size);
                let spec = FilterSpec::new(filter_type, channels, 0, size as u32);
                let packed =
                    encode_with_filters(&data, 3, 0, &[spec], ArchiveVersion::Rar50).unwrap();
                let buffered =
                    decode_standalone(&packed, size as u64, 0, None, ArchiveVersion::Rar50)
                        .unwrap();
                let mut streamed = Vec::new();
                let written = decode_standalone_to_writer(
                    &packed,
                    size as u64,
                    0,
                    None,
                    ArchiveVersion::Rar50,
                    &mut streamed,
                )
                .unwrap();
                assert_eq!(written, size as u64);
                assert_eq!(
                    streamed, buffered,
                    "streaming != buffered for filter {filter_type:#x}, channels {channels}, size {size}"
                );
                assert_eq!(
                    streamed, data,
                    "streaming != original for filter {filter_type:#x}, channels {channels}, size {size}"
                );
            }
        }
    }

    /// The size-based channel selection must pick the frame size (bytes ×
    /// channels) for interleaved little-endian samples: each byte position of
    /// the frame packs best as its own delta lane, so 16-bit stereo picks 4,
    /// 32-bit stereo 8, 24-bit 3-channel 9, 32-bit 4-channel 16.
    #[test]
    fn delta_selection_prefers_frame_size() {
        fn correlated_samples(bytes: usize, channels: usize, n: usize) -> Vec<u8> {
            let mut out = Vec::with_capacity(bytes * channels * n);
            let mut val = vec![0i64; channels];
            let mut state = 0x1234_5678u64;
            for _ in 0..n {
                for v in &mut val {
                    state ^= state >> 12;
                    state ^= state << 25;
                    state ^= state >> 27;
                    let r = (state >> 33) as u32;
                    *v += (r % 8) as i64 - 4;
                    let value = *v;
                    for b in 0..bytes {
                        out.push((value >> (8 * b)) as u8);
                    }
                }
            }
            out
        }
        for (bytes, channels, expect) in [
            (1usize, 1usize, 1u8),
            (2, 1, 2),
            (2, 2, 4),
            (3, 1, 3),
            (3, 3, 9),
            (1, 2, 2),
            (2, 4, 8),
            (4, 2, 8),
            (4, 4, 16),
        ] {
            let data = correlated_samples(bytes, channels, 100_000);
            let got = pick_delta_channel(&data, 3, 0, ArchiveVersion::Rar50).unwrap();
            assert_eq!(
                got,
                Some(expect),
                "bytes={bytes} channels={channels}: expected frame size {expect}, got {got:?}"
            );
        }
    }

    /// The automatic delta (multimedia) filter must round-trip correlated
    /// multi-channel data and pack it smaller than STORE, while refusing
    /// random/text data.
    #[test]
    fn auto_delta_filter_roundtrips_and_packs_smaller() {
        use super::encode_with_auto_delta_filter;

        // Correlated N-bit interleaved samples (small per-sample deltas),
        // matching the kind of 8/16/24/32-bit multi-channel data WinRAR
        // deltas: `bytes` little-endian bytes per sample × `channels` lanes.
        fn correlated(bytes: usize, channels: usize, n: usize) -> Vec<u8> {
            let mut out = Vec::with_capacity(bytes * channels * n);
            let mut val = vec![0i64; channels];
            let mut state = 0xABCDEF01u64;
            for _ in 0..n {
                for v in &mut val {
                    state ^= state >> 12;
                    state ^= state << 25;
                    state ^= state >> 27;
                    *v += ((state >> 33) as u32 % 8) as i64 - 4;
                    for b in 0..bytes {
                        out.push((*v >> (8 * b)) as u8);
                    }
                }
            }
            out
        }

        for (bytes, channels) in [
            (1usize, 1usize),
            (2, 1),
            (2, 2),
            (3, 1),
            (3, 3),
            (4, 2),
            (4, 4),
        ] {
            let data = correlated(bytes, channels, 200_000);
            let packed = encode_with_auto_delta_filter(&data, 3, 0, ArchiveVersion::Rar50, 1)
                .unwrap()
                .expect("delta scan must find a beneficial channel count");
            assert!(
                packed.len() < data.len(),
                "bytes={bytes} channels={channels}: delta-filtered encoding should compress: {} vs {}",
                packed.len(),
                data.len()
            );
            let back =
                decode_standalone(&packed, data.len() as u64, 0, None, ArchiveVersion::Rar50)
                    .unwrap();
            assert_eq!(back, data, "bytes={bytes} channels={channels}");
        }

        // Text must NOT be delta-filtered: delta cannot beat plain LZSS on it.
        let text = b"the quick brown fox jumps over the lazy dog. ".repeat(6_000);
        assert!(
            encode_with_auto_delta_filter(&text, 3, 0, ArchiveVersion::Rar50, 1)
                .unwrap()
                .is_none(),
            "text must fall back to plain LZSS"
        );

        // Random data must NOT be delta-filtered.
        let mut state = 0x9E37_9B97_7F4A_7C15u64;
        let mut random = vec![0u8; 200_000];
        for b in random.iter_mut() {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            *b = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u8;
        }
        assert!(
            encode_with_auto_delta_filter(&random, 3, 0, ArchiveVersion::Rar50, 1)
                .unwrap()
                .is_none()
        );
    }

    /// Synthetic x86 code: E8/E9 opcodes with plausible relative targets,
    /// dense enough that the automatic scan finds a region. The auto-filter
    /// path must pick it up, pack it, and decode back to the original.
    #[test]
    fn auto_x86_filter_roundtrips_and_packs_smaller() {
        use super::encode_with_auto_x86_filter;

        fn x86ish(size: usize) -> Vec<u8> {
            let mut data = vec![0x90u8; size]; // NOP padding
            let mut pos = 0usize;
            while pos + 5 <= size {
                data[pos] = if pos.is_multiple_of(170) { 0xE8 } else { 0xE9 };
                let addr = ((pos as u32).wrapping_mul(7)) & 0x00FF_FFFF;
                data[pos + 1..pos + 5].copy_from_slice(&addr.to_le_bytes());
                pos += 85;
            }
            data
        }

        let data = x86ish(400_000);
        let packed = encode_with_auto_x86_filter(&data, 3, 0, ArchiveVersion::Rar50, 1)
            .unwrap()
            .expect("x86 scan must find regions");
        assert!(
            packed.len() < data.len(),
            "filtered encoding should compress: {} vs {}",
            packed.len(),
            data.len()
        );
        let back =
            decode_standalone(&packed, data.len() as u64, 0, None, ArchiveVersion::Rar50).unwrap();
        assert_eq!(back, data);

        // Non-code data with isolated opcodes must NOT be filtered.
        let mut sparse = vec![0x41u8; 20_000];
        sparse[100] = 0xE8;
        sparse[10_000] = 0xE8;
        assert!(
            encode_with_auto_x86_filter(&sparse, 3, 0, ArchiveVersion::Rar50, 1)
                .unwrap()
                .is_none()
        );
    }

    /// Regression: filter positions and E8 transform offsets are
    /// member-relative even when the member sits at a non-zero offset of a
    /// solid chain (WinRAR's `WrittenFileSize` is per-file while the filter
    /// record positions are stream-absolute). Decoding a filtered member
    /// with shared decoder state must reproduce the original bytes.
    #[test]
    fn filtered_member_at_solid_offset_decodes_member_relative() {
        use super::encode_with_auto_x86_filter;

        fn x86ish(size: usize) -> Vec<u8> {
            let mut data = vec![0x90u8; size];
            let mut pos = 0usize;
            while pos + 5 <= size {
                data[pos] = if pos.is_multiple_of(170) { 0xE8 } else { 0xE9 };
                let addr = ((pos as u32).wrapping_mul(7)) & 0x00FF_FFFF;
                data[pos + 1..pos + 5].copy_from_slice(&addr.to_le_bytes());
                pos += 85;
            }
            data
        }

        // A first plain member fills the shared window; the filtered member
        // then decodes at a non-zero stream offset.
        let first = b"solid chain prefix data padding padding padding".repeat(64);
        let member = x86ish(120_000);

        let packed_first = crate::codec::encode_raw(&first, 3, 3, ArchiveVersion::Rar50);
        let packed_member = encode_with_auto_x86_filter(&member, 3, 0, ArchiveVersion::Rar50, 1)
            .unwrap()
            .expect("x86 scan must find regions");

        let mut state = DecoderState::new(128 * 1024);
        let decoded_first = decode_raw(
            &packed_first,
            first.len() as u64,
            DecodeOptions {
                dict_size_log: 3,
                dict_size_bytes: None,
                variant: ArchiveVersion::Rar50,
                state: Some(&mut state),
            },
        )
        .unwrap();
        assert_eq!(decoded_first, first);

        let decoded_member = decode_raw(
            &packed_member,
            member.len() as u64,
            DecodeOptions {
                dict_size_log: 0,
                dict_size_bytes: None,
                variant: ArchiveVersion::Rar50,
                state: Some(&mut state),
            },
        )
        .unwrap();
        assert_eq!(
            decoded_member, member,
            "filtered member must decode member-relative at a solid offset"
        );
    }
}
