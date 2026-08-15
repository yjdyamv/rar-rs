/// RAR5 block decoder — decompresses the RAR5 compressed bitstream.
///
/// Bitstream format derived from analysis of libarchive's
/// archive_read_support_format_rar5.c by Grzegorz Antoniak (2018),
/// an independent BSD-2-Clause licensed implementation.
use super::bitstream::BitReader;
use super::filters::apply_filter_decode;
use super::huffman::{DecodeTable, decode_symbol};
use super::tables::*;
use super::window::SlidingWindow;

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
    dist_cache: [u32; DIST_CACHE_SIZE],
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
pub struct DecodeOptions<'a> {
    /// Dictionary size as log2(size/128KB), 0 = 128KB. Used when `state`
    /// is `None`.
    pub dict_size_log: u8,
    /// Shared decoder state for solid-chain continuity (`None` for
    /// standalone members).
    pub state: Option<&'a mut DecoderState>,
}

/// Decode RAR5 compressed data into a buffer.
///
/// - `data`: raw compressed bytes (the data area from the file block)
/// - `unpacked_size`: expected decompressed size in bytes
pub fn decode(data: &[u8], unpacked_size: u64, opts: DecodeOptions<'_>) -> Result<Vec<u8>, String> {
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
        ),
        None => decode_standalone(data, unpacked_size, opts.dict_size_log),
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
) -> Result<u64, String> {
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
            writer,
        ),
        None => decode_standalone_to_writer(data, unpacked_size, opts.dict_size_log, writer),
    }
}

/// Streaming variant of [`decode_standalone`].
pub fn decode_standalone_to_writer(
    data: &[u8],
    unpacked_size: u64,
    dict_size_log: u8,
    writer: &mut dyn std::io::Write,
) -> Result<u64, String> {
    let dict_size = checked_dict_size(dict_size_log)?;
    let mut reader = BitReader::new(data);
    let mut window = SlidingWindow::new(dict_size);
    let mut dist_cache = [0u32; DIST_CACHE_SIZE];
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
        writer,
    )
}

/// Compute and validate a decoder dictionary size from its log field.
///
/// Rejects values that would overflow the host `usize` or exceed the
/// RAR5 v5.0 maximum (1 GiB, `log` 13); WinRAR 5.x never emits larger
/// dictionaries.
fn checked_dict_size(dict_size_log: u8) -> Result<usize, String> {
    if dict_size_log > 13 {
        return Err(format!(
            "dictionary size log {dict_size_log} exceeds supported maximum 13"
        ));
    }
    (128usize * 1024)
        .checked_shl(dict_size_log as u32)
        .ok_or_else(|| format!("dictionary size log {dict_size_log} overflows usize"))
}

/// Streaming decode core: writes decoded (and filtered) output to `writer`.
#[allow(clippy::too_many_arguments)]
fn decode_inner_streaming(
    reader: &mut BitReader,
    unpacked_size: u64,
    window: &mut SlidingWindow,
    dist_cache: &mut [u32; DIST_CACHE_SIZE],
    last_length: &mut u32,
    prev_low_dist: &mut u32,
    table_nc: &mut Option<DecodeTable>,
    table_dc: &mut Option<DecodeTable>,
    table_ldc: &mut Option<DecodeTable>,
    table_rc: &mut Option<DecodeTable>,
    writer: &mut dyn std::io::Write,
) -> Result<u64, String> {
    const COPY_THRESHOLD: u64 = 64 * 1024;

    let mut pending_filters: Vec<PendingFilter> = Vec::new();
    let output_start = window.total_written();
    let mut sink = OutputSink::new(writer, output_start);
    let mut copied_abs = output_start;

    while (window.total_written() - output_start) < unpacked_size {
        // ── Read block header ──────────────────────────────────────────
        let block_flags_byte = reader.read_byte().map_err(|e| e.to_string())?;

        let table_present = (block_flags_byte >> 7) & 1 != 0;
        let is_last_block = (block_flags_byte >> 6) & 1 != 0;
        let byte_count = ((block_flags_byte >> 3) & 3) + 1;
        let bit_size = block_flags_byte & 7;

        let checksum_byte = reader.read_byte().map_err(|e| e.to_string())?;

        let block_size_bytes = reader
            .read_bytes(byte_count as usize)
            .map_err(|e| e.to_string())?;
        let mut block_size: u32 = 0;
        for (i, &b) in block_size_bytes.iter().enumerate() {
            block_size |= (b as u32) << (i * 8);
        }

        let mut expected_ck = BLOCK_CHECKSUM_SEED ^ block_flags_byte;
        for &b in block_size_bytes {
            expected_ck ^= b;
        }
        if checksum_byte != expected_ck {
            return Err(format!(
                "block checksum mismatch: got {checksum_byte:#x}, expected {expected_ck:#x}"
            ));
        }

        if block_size == 0 {
            return Err("zero-length block".into());
        }
        let block_bits = ((block_size as u64) - 1) * 8 + (1 + bit_size as u64);
        let block_start_bits = reader.byte_position() as u64 * 8 + reader.bit_position() as u64;

        if table_present {
            let (nc, dc, ldc, rc) = read_tables(reader)?;
            *table_nc = Some(nc);
            *table_dc = Some(dc);
            *table_ldc = Some(ldc);
            *table_rc = Some(rc);
        }

        let t_nc = table_nc.as_ref().ok_or("no Huffman tables defined")?;
        let t_dc = table_dc.as_ref().ok_or("no Huffman tables defined")?;
        let t_ldc = table_ldc.as_ref().ok_or("no Huffman tables defined")?;
        let t_rc = table_rc.as_ref().ok_or("no Huffman tables defined")?;

        // ── Decode symbols ─────────────────────────────────────────────
        while (window.total_written() - output_start) < unpacked_size {
            let cur_bits = reader.byte_position() as u64 * 8 + reader.bit_position() as u64;
            if cur_bits - block_start_bits >= block_bits {
                break;
            }

            let sym = decode_symbol(t_nc, reader).map_err(|e| e.to_string())?;

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
                let len_slot = decode_symbol(t_rc, reader).map_err(|e| e.to_string())?;
                let length = decode_length(len_slot, reader)?;
                *last_length = length;
                *prev_low_dist = dist & 0xF;
                window.copy_match(dist as usize, length as usize);
            } else if sym >= SYM_MATCH_BASE {
                let len_slot = sym - SYM_MATCH_BASE;
                let mut length = decode_length(len_slot, reader)?;
                let dist_slot = decode_symbol(t_dc, reader).map_err(|e| e.to_string())?;
                let dist = decode_distance(dist_slot, reader, t_ldc)?;
                length = apply_length_bonus(length, dist);
                *last_length = length;
                *prev_low_dist = dist & 0xF;
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
        return Err("unapplied RAR5 filter at end of stream".into());
    }
    if sink.staging_len() != 0 {
        return Err("internal streaming decode staging error".into());
    }
    let produced = written - output_start;
    if produced != unpacked_size {
        return Err(format!(
            "decompressed size mismatch: expected {unpacked_size}, got {produced}"
        ));
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
    consumed: usize,
}

impl<'a> OutputSink<'a> {
    fn new(writer: &'a mut dyn std::io::Write, start: u64) -> Self {
        Self {
            writer,
            staging: Vec::new(),
            staging_start: start,
            consumed: 0,
        }
    }

    fn staging_len(&self) -> usize {
        self.staging.len() - self.consumed
    }

    fn append_window(&mut self, window: &SlidingWindow, from: u64, to: u64) -> Result<(), String> {
        if to <= from {
            return Ok(());
        }
        let bytes = window.get_output(from, (to - from) as usize);
        if self.staging_len() + bytes.len() > MAX_STREAMING_FILTER_BUFFER as usize {
            return Err(format!(
                "filtered output region exceeds streaming buffer limit {}",
                MAX_STREAMING_FILTER_BUFFER
            ));
        }
        self.staging.extend_from_slice(&bytes);
        Ok(())
    }

    fn apply_complete_filters(&mut self, pending: &mut [PendingFilter]) -> Result<(), String> {
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
                return Err("filter region out of staging bounds".into());
            }
            let region = &mut self.staging[base + start_off..base + end_off];
            let filtered =
                apply_filter_decode(filt.filter_type, region, filt.channels, filt.block_start)?;
            if filtered.len() != region.len() {
                return Err("RAR5 filter changed output length".into());
            }
            region.copy_from_slice(&filtered);
            filt.applied = true;
        }
        Ok(())
    }

    fn drain_up_to(&mut self, written: u64, pending: &[PendingFilter]) -> Result<(), String> {
        let earliest_filter = pending
            .iter()
            .filter(|f| !f.applied)
            .map(|f| f.block_start)
            .min()
            .unwrap_or(written);
        let drain_to = earliest_filter.min(written);
        let n = (drain_to - self.staging_start) as usize;
        if n > self.staging_len() {
            return Err("internal drain beyond staging".into());
        }
        if n > 0 {
            self.writer
                .write_all(&self.staging[self.consumed..self.consumed + n])
                .map_err(|e| e.to_string())?;
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

/// Decode RAR5 compressed data (standalone, no solid state).
pub fn decode_standalone(
    data: &[u8],
    unpacked_size: u64,
    dict_size_log: u8,
) -> Result<Vec<u8>, String> {
    let mut dict_size = checked_dict_size(dict_size_log)?;
    // The decoder reconstructs the whole file in the sliding window before
    // extracting it (see `get_output`), so the window must be at least as
    // large as the unpacked output. The encoder caps its dictionary at 1 MiB
    // for compression performance, so grow the decode buffer here instead of
    // reverting that cap.
    let unpacked = usize::try_from(unpacked_size)
        .map_err(|_| "unpacked size overflows host address space".to_string())?;
    if unpacked > dict_size {
        dict_size = unpacked
            .checked_next_power_of_two()
            .ok_or_else(|| "unpacked size too large for host address space".to_string())?;
    }

    let mut reader = BitReader::new(data);
    let mut window = SlidingWindow::new(dict_size);
    let mut dist_cache = [0u32; DIST_CACHE_SIZE];
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
    )
}

#[allow(clippy::too_many_arguments)]
fn decode_inner(
    reader: &mut BitReader,
    unpacked_size: u64,
    window: &mut SlidingWindow,
    dist_cache: &mut [u32; DIST_CACHE_SIZE],
    last_length: &mut u32,
    prev_low_dist: &mut u32,
    table_nc: &mut Option<DecodeTable>,
    table_dc: &mut Option<DecodeTable>,
    table_ldc: &mut Option<DecodeTable>,
    table_rc: &mut Option<DecodeTable>,
) -> Result<Vec<u8>, String> {
    let mut pending_filters: Vec<PendingFilter> = Vec::new();
    let output_start = window.total_written();

    while (window.total_written() - output_start) < unpacked_size {
        // ── Read block header ──────────────────────────────────────────
        let block_flags_byte = reader.read_byte().map_err(|e| e.to_string())?;

        let table_present = (block_flags_byte >> 7) & 1 != 0;
        let is_last_block = (block_flags_byte >> 6) & 1 != 0;
        let byte_count = ((block_flags_byte >> 3) & 3) + 1;
        let bit_size = block_flags_byte & 7;

        let checksum_byte = reader.read_byte().map_err(|e| e.to_string())?;

        let block_size_bytes = reader
            .read_bytes(byte_count as usize)
            .map_err(|e| e.to_string())?;
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
            return Err(format!(
                "block checksum mismatch: got {checksum_byte:#x}, expected {expected_ck:#x}"
            ));
        }

        if block_size == 0 {
            return Err("zero-length block".into());
        }
        let block_bits = ((block_size as u64) - 1) * 8 + (1 + bit_size as u64);
        let block_start_bits = reader.byte_position() as u64 * 8 + reader.bit_position() as u64;

        // ── Read Huffman tables if present ──────────────────────────────
        if table_present {
            let (nc, dc, ldc, rc) = read_tables(reader)?;
            *table_nc = Some(nc);
            *table_dc = Some(dc);
            *table_ldc = Some(ldc);
            *table_rc = Some(rc);
        }

        let t_nc = table_nc.as_ref().ok_or("no Huffman tables defined")?;
        let t_dc = table_dc.as_ref().ok_or("no Huffman tables defined")?;
        let t_ldc = table_ldc.as_ref().ok_or("no Huffman tables defined")?;
        let t_rc = table_rc.as_ref().ok_or("no Huffman tables defined")?;

        // ── Decode symbols ─────────────────────────────────────────────
        while (window.total_written() - output_start) < unpacked_size {
            let cur_bits = reader.byte_position() as u64 * 8 + reader.bit_position() as u64;
            if cur_bits - block_start_bits >= block_bits {
                break;
            }

            let sym = decode_symbol(t_nc, reader).map_err(|e| e.to_string())?;

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
                let len_slot = decode_symbol(t_rc, reader).map_err(|e| e.to_string())?;
                let length = decode_length(len_slot, reader)?;
                *last_length = length;
                *prev_low_dist = dist & 0xF;
                window.copy_match(dist as usize, length as usize);
            } else if sym >= SYM_MATCH_BASE {
                let len_slot = sym - SYM_MATCH_BASE;
                let mut length = decode_length(len_slot, reader)?;
                let dist_slot = decode_symbol(t_dc, reader).map_err(|e| e.to_string())?;
                let dist = decode_distance(dist_slot, reader, t_ldc)?;
                length = apply_length_bonus(length, dist);
                *last_length = length;
                *prev_low_dist = dist & 0xF;
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

    // Apply pending filters
    for filt in &pending_filters {
        let start = (filt.block_start - output_start) as usize;
        let end = (start + filt.block_length as usize).min(output.len());
        if start >= output.len() {
            continue;
        }
        let region = &mut output[start..end];
        let filtered =
            apply_filter_decode(filt.filter_type, region, filt.channels, filt.block_start)?;
        output[start..start + filtered.len()].copy_from_slice(&filtered);
    }

    output.truncate(unpacked_size as usize);
    Ok(output)
}

// ── Huffman Table Reading ──────────────────────────────────────────────────

fn read_tables(
    reader: &mut BitReader,
) -> Result<(DecodeTable, DecodeTable, DecodeTable, DecodeTable), String> {
    // Read BC table: 20 code lengths as nibbles with escape mechanism
    let mut bc_lengths = Vec::with_capacity(HUFF_BC);
    while bc_lengths.len() < HUFF_BC {
        let val = reader.read_bits(4).map_err(|e| e.to_string())? as u8;
        if val == NIBBLE_ESCAPE {
            let next_val = reader.read_bits(4).map_err(|e| e.to_string())? as u8;
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

    let total = HUFF_NC + HUFF_DC + HUFF_LDC + HUFF_RC;
    let all_lengths = read_code_lengths(reader, &table_bc, total)?;

    let nc_len = &all_lengths[..HUFF_NC];
    let dc_len = &all_lengths[HUFF_NC..HUFF_NC + HUFF_DC];
    let ldc_len = &all_lengths[HUFF_NC + HUFF_DC..HUFF_NC + HUFF_DC + HUFF_LDC];
    let rc_len = &all_lengths[HUFF_NC + HUFF_DC + HUFF_LDC..];

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
) -> Result<Vec<u8>, String> {
    let mut lengths = vec![0u8; count];
    let mut i = 0;
    while i < count {
        let sym = decode_symbol(bc_table, reader).map_err(|e| e.to_string())?;
        if sym < 16 {
            lengths[i] = sym as u8;
            i += 1;
        } else if sym < 18 {
            if i == 0 {
                return Err("run-length repeat with no previous length".into());
            }
            let repeat = if sym == 16 {
                3 + reader.read_bits(3).map_err(|e| e.to_string())? as usize
            } else {
                11 + reader.read_bits(7).map_err(|e| e.to_string())? as usize
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
                3 + reader.read_bits(3).map_err(|e| e.to_string())? as usize
            } else {
                11 + reader.read_bits(7).map_err(|e| e.to_string())? as usize
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

fn decode_length(slot: usize, reader: &mut BitReader) -> Result<u32, String> {
    if slot < 8 {
        Ok(2 + slot as u32)
    } else {
        let lbits = (slot / 4 - 1) as u8;
        let base = 2 + ((4 | (slot & 3)) << lbits) as u32;
        if lbits > 0 {
            let extra = reader.read_bits(lbits).map_err(|e| e.to_string())?;
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
) -> Result<u32, String> {
    if dist_slot < 4 {
        Ok(1 + dist_slot as u32)
    } else {
        let dbits = (dist_slot / 2 - 1) as u8;
        let mut dist = 1 + ((2 | (dist_slot & 1)) << dbits) as u32;

        if dbits > 0 {
            if dbits >= 4 {
                if dbits > 4 {
                    let upper = reader.read_bits(dbits - 4).map_err(|e| e.to_string())?;
                    dist = dist.wrapping_add(upper << 4);
                }
                let low_dist = decode_symbol(table_ldc, reader).map_err(|e| e.to_string())?;
                dist = dist.wrapping_add(low_dist as u32);
            } else {
                let extra = reader.read_bits(dbits).map_err(|e| e.to_string())?;
                dist = dist.wrapping_add(extra);
            }
        }
        Ok(dist)
    }
}

// ── Distance Cache ─────────────────────────────────────────────────────────

fn dist_cache_push(cache: &mut [u32; DIST_CACHE_SIZE], value: u32) {
    cache[3] = cache[2];
    cache[2] = cache[1];
    cache[1] = cache[0];
    cache[0] = value;
}

fn dist_cache_touch(cache: &mut [u32; DIST_CACHE_SIZE], idx: usize) -> u32 {
    let value = cache[idx];
    for i in (1..=idx).rev() {
        cache[i] = cache[i - 1];
    }
    cache[0] = value;
    value
}

// ── Length Bonus ────────────────────────────────────────────────────────────

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

// ── Filter Parsing ─────────────────────────────────────────────────────────

fn parse_filter(reader: &mut BitReader, write_pos: u64) -> Result<PendingFilter, String> {
    let block_start = write_pos + parse_filter_data(reader)? as u64;
    let block_length = parse_filter_data(reader)? as u64;
    let filter_type = reader.read_bits(3).map_err(|e| e.to_string())? as u8;

    let channels = if filter_type == FILTER_DELTA {
        reader.read_bits(5).map_err(|e| e.to_string())? as u8 + 1
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

fn parse_filter_data(reader: &mut BitReader) -> Result<u32, String> {
    let byte_count = reader.read_bits(2).map_err(|e| e.to_string())? + 1;
    let mut value: u32 = 0;
    for i in 0..byte_count {
        let b = reader.read_bits(8).map_err(|e| e.to_string())?;
        value |= b << (i * 8);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fuzz regression: block flags with the reserved bit 5 set previously
    /// produced a byte_count of 8, overflowing the u32 block-size shift in
    /// debug builds. The block size field is 2 bits (1-4 bytes); reserved
    /// bits are ignored.
    #[test]
    fn block_flags_with_reserved_bit_do_not_overflow() {
        let stream = [0xe4u8, 0x00, 0xe0, 0x00, 0xe0, 0x00, 0x00];
        let result = std::panic::catch_unwind(|| {
            let _ = decode_standalone(&stream, 78090, 0);
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
        let packed = crate::codec::encode(&data, 3, 3);
        let back = decode_standalone(&packed, data.len() as u64, 3).unwrap();
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
        use crate::codec::encoder::{FilterSpec, MAX_FILTER_BLOCK_LENGTH, encode_with_filters};
        use crate::codec::tables::{FILTER_ARM, FILTER_DELTA, FILTER_E8, FILTER_E8E9};

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
                let packed = encode_with_filters(&data, 3, 0, &[spec]).unwrap();
                let buffered = decode_standalone(&packed, size as u64, 0).unwrap();
                let mut streamed = Vec::new();
                let written =
                    decode_standalone_to_writer(&packed, size as u64, 0, &mut streamed).unwrap();
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
}
