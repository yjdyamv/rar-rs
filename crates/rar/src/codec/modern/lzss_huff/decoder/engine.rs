//! Public decode entry points over the single decoding engine.
//!
//! Every entry point runs the same window loop over
//! [`SymbolReader`](super::symbols::SymbolReader): `decode_raw` collects the
//! output into a buffer, while [`decode_to_writer`] streams through
//! `OutputSink`, which applies pending filters before bytes leave and bounds
//! held-back output by [`MAX_STREAMING_FILTER_BUFFER`]. The solid-chain entry
//! points reuse the window and symbol state owned by
//! [`super::DecoderState`].

use super::*;

use super::symbols::{Symbol, SymbolReader, SymbolState};
use crate::codec::common::filters::apply_filter_decode;
use crate::error::{RarError, RarResult};

/// Decode RAR5 compressed data into a buffer.
///
/// - `data`: raw compressed bytes (the data area from the file block)
/// - `unpacked_size`: expected decompressed size in bytes
pub fn decode_raw(data: &[u8], unpacked_size: u64, opts: DecodeOptions<'_>) -> RarResult<Vec<u8>> {
    let mut output = Vec::new();
    decode_to_writer(data, unpacked_size, opts, &mut output)?;
    Ok(output)
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
        Some(st) => run_engine(
            data,
            unpacked_size,
            &mut st.window,
            &mut st.symbols,
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

/// Decode a standalone member (no solid state) to a writer.
pub fn decode_standalone_to_writer(
    data: &[u8],
    unpacked_size: u64,
    dict_size_log: u8,
    dict_size_bytes: Option<u64>,
    variant: ArchiveVersion,
    writer: &mut dyn std::io::Write,
) -> RarResult<u64> {
    let dict_size = checked_dict_size(dict_size_log, dict_size_bytes)?;
    let mut window = SlidingWindow::new(dict_size);
    let mut symbols = SymbolState::default();
    run_engine(
        data,
        unpacked_size,
        &mut window,
        &mut symbols,
        variant,
        writer,
    )
}

/// Decode RAR5/RAR7 compressed data (standalone, no solid state).
pub fn decode_standalone(
    data: &[u8],
    unpacked_size: u64,
    dict_size_log: u8,
    dict_size_bytes: Option<u64>,
    variant: ArchiveVersion,
) -> RarResult<Vec<u8>> {
    let mut output = Vec::new();
    decode_standalone_to_writer(
        data,
        unpacked_size,
        dict_size_log,
        dict_size_bytes,
        variant,
        &mut output,
    )?;
    Ok(output)
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

/// The single decode loop: apply every symbol to the window and stream the
/// produced bytes through [`OutputSink`].
///
/// `window` and `symbols` are the two halves of the decoder state; a
/// standalone member gets fresh ones, a solid-chain member the shared ones.
fn run_engine(
    data: &[u8],
    unpacked_size: u64,
    window: &mut SlidingWindow,
    symbols: &mut SymbolState,
    variant: ArchiveVersion,
    writer: &mut dyn std::io::Write,
) -> RarResult<u64> {
    const COPY_THRESHOLD: u64 = 64 * 1024;

    let output_start = window.total_written();
    let mut symbols = SymbolReader::new(data, variant, output_start, unpacked_size, symbols);
    let mut pending_filters: Vec<PendingFilter> = Vec::new();
    let mut sink = OutputSink::new(writer, output_start);
    let mut copied_abs = output_start;

    while let Some(symbol) = symbols.next()? {
        match symbol {
            Symbol::Literal(byte) => window.put_byte(byte),
            Symbol::Match { dist, len, .. } => window.copy_match(dist as usize, len as usize),
            Symbol::Filter(filter) => pending_filters.push(filter),
            Symbol::BlockStart(_) => {}
        }

        // Copy newly produced window bytes into staging before the ring can
        // overwrite them, then drain as far as filters allow.
        let written = window.total_written();
        debug_assert_eq!(
            written,
            symbols.pos(),
            "symbol reader and window positions diverged"
        );
        if written - copied_abs >= COPY_THRESHOLD {
            sink.append_window(window, copied_abs, written)?;
            copied_abs = written;
            sink.apply_complete_filters(&mut pending_filters)?;
            sink.drain_up_to(window.total_written(), &pending_filters)?;
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
