//! Record builder: parity computation and the `"RR"` chunk payload.

use std::io::{self, Read};

use super::gf16::Gf16;
use super::gf16::{encode_parity_shards_with_progress, make_encoder_matrix};
use super::plan::{
    InlineRecoveryPlan, crc64_rar_state, crc64_xz, plan_inline_recovery, shard_block_range,
    split_prefix_shard_ranges, split_prefix_shards,
};
use super::{Error, Result, shared_gf16};
use crate::write_progress::ProgressReporter;
use crate::write_progress::{WriteOperation, WriteProgressEvent};

pub(super) fn encode_inline_recovery_parity_with_progress(
    archive_prefix: &[u8],
    recovery_percent: u64,
    progress: Option<ProgressReporter<'_>>,
    pass: usize,
) -> Result<(InlineRecoveryPlan, Vec<Vec<u8>>)> {
    let plan = plan_inline_recovery(archive_prefix.len() as u64, recovery_percent)?;
    let shards = split_prefix_shards(archive_prefix, plan)?;
    let shard_refs: Vec<&[u8]> = shards.iter().map(Vec::as_slice).collect();
    let total_bytes = plan.payload_size()?;
    if let Some(progress) = progress {
        progress.report(WriteProgressEvent::OperationStarted {
            operation: WriteOperation::Recovery,
            total_bytes: Some(total_bytes),
            total_entries: None,
            pass,
        });
    }
    let parity = encode_parity_shards_with_progress(
        &shard_refs,
        usize::try_from(plan.recovery_shards).map_err(|_| Error::PlanOverflow)?,
        |completed| {
            if let Some(progress) = progress {
                progress.report(WriteProgressEvent::Advanced {
                    operation: WriteOperation::Recovery,
                    completed_bytes: completed,
                    total_bytes,
                    pass,
                });
            }
        },
    )?;
    if let Some(progress) = progress {
        progress.report(WriteProgressEvent::OperationFinished {
            operation: WriteOperation::Recovery,
            total_bytes: Some(total_bytes),
            total_entries: None,
            pass,
        });
    }
    Ok((plan, parity))
}

/// Build the data area of a RAR5 inline recovery record protecting
/// `archive_prefix` at `recovery_percent` percent, single-threaded.
pub fn build_structural_inline_recovery_data(
    archive_prefix: &[u8],
    recovery_percent: u64,
) -> Result<Vec<u8>> {
    build_structural_inline_recovery_data_with_progress(archive_prefix, recovery_percent, None, 1)
}

pub(crate) fn build_structural_inline_recovery_data_with_progress(
    archive_prefix: &[u8],
    recovery_percent: u64,
    progress: Option<ProgressReporter<'_>>,
    pass: usize,
) -> Result<Vec<u8>> {
    let (plan, parity) = encode_inline_recovery_parity_with_progress(
        archive_prefix,
        recovery_percent,
        progress,
        pass,
    )?;
    let shard_ranges = split_prefix_shard_ranges(archive_prefix.len(), plan)?;
    // One state table per 64 KiB block: each `{RB}` chunk carries the CRC64s of
    // the data-shard bytes that its own block covers, not of the whole shard.
    let block_states: Vec<Vec<u64>> = (0..plan.blocks_per_shard())
        .map(|block| {
            shard_ranges
                .iter()
                .map(|range| {
                    crc64_rar_state(&archive_prefix[shard_block_range(range.clone(), block)])
                })
                .collect()
        })
        .collect();
    // The record's last 8 header bytes. WinRAR fills this with a random nonce
    // (CryptGenRandom in `Rar7.exe`), so its `-rr` output is not reproducible
    // and cannot be matched byte for byte; repair ignores the field. We write a
    // deterministic CRC64 of the first recovery shard instead, keeping our
    // output reproducible.
    let final_state = parity
        .first()
        .map(|payload| crc64_rar_state(payload))
        .unwrap_or(0);
    assemble_recovery_data(
        plan,
        parity,
        block_states,
        final_state,
        archive_prefix.len() as u64,
    )
}

/// Assemble the on-disk recovery-record payload from a finished parity plan.
/// Shared by the buffered and streaming encoders so both emit byte-identical
/// output — the bytes must match WinRAR / UnRAR for the archive to be repairable.
///
/// Every recovery shard's parity is split into `plan.blocks_per_shard()` chunks
/// of at most 64 KiB, each carrying its own header. WinRAR lays the record out
/// this way, and it is what makes the record's advertised geometry
/// (`shard_size`) consistent with the bytes on disk: a single chunk per shard
/// describes a `total_size` field WinRAR cannot reconcile, and it then reports a
/// nonsense recovery-record size.
/// `block_states[block][shard]` is the CRC64 the `{RB}` chunk of `block`
/// records for data shard `shard`.
fn assemble_recovery_data(
    plan: InlineRecoveryPlan,
    parity: Vec<Vec<u8>>,
    block_states: Vec<Vec<u64>>,
    final_state: u64,
    archive_len: u64,
) -> Result<Vec<u8>> {
    let total_len = usize::try_from(plan.payload_size()?).map_err(|_| Error::PlanOverflow)?;
    let header_size = usize::try_from(plan.header_size).map_err(|_| Error::PlanOverflow)?;
    let data_shards = usize::try_from(plan.data_shards).map_err(|_| Error::PlanOverflow)?;
    let recovery_shards = usize::try_from(plan.recovery_shards).map_err(|_| Error::PlanOverflow)?;
    let header_size_u32 = u32::try_from(plan.header_size).map_err(|_| Error::PlanOverflow)?;
    let data_shards_u16 = u16::try_from(plan.data_shards).map_err(|_| Error::PlanOverflow)?;
    let recovery_shards_u16 =
        u16::try_from(plan.recovery_shards).map_err(|_| Error::PlanOverflow)?;

    // Real length of the last data shard: everything past it inside the padded
    // parity is zero fill, which the structural header excludes from the chunk's
    // `data_extent` field.
    let last_shard_len = archive_len
        .saturating_sub(
            plan.data_shards
                .saturating_sub(1)
                .saturating_mul(plan.group_count),
        )
        .min(plan.group_count);

    let mut out = Vec::with_capacity(total_len);
    for (shard_index, payload) in parity.iter().enumerate() {
        if payload.len() != usize::try_from(plan.group_count).map_err(|_| Error::PlanOverflow)? {
            return Err(Error::PlanOverflow);
        }
        for block in 0..plan.blocks_per_shard() {
            let block_offset =
                usize::try_from(plan.block_offset(block)).map_err(|_| Error::PlanOverflow)?;
            let block_len =
                usize::try_from(plan.block_payload_len(block)).map_err(|_| Error::PlanOverflow)?;
            let block_end = block_offset
                .checked_add(block_len)
                .ok_or(Error::PlanOverflow)?;
            let payload_slice = payload
                .get(block_offset..block_end)
                .ok_or(Error::PlanOverflow)?;
            let total_size = u32::try_from(
                plan.header_size
                    .checked_add(block_len as u64)
                    .ok_or(Error::PlanOverflow)?,
            )
            .map_err(|_| Error::PlanOverflow)?;
            // Parity words past the real data are zero fill; report only the
            // protected bytes that this block actually covers.
            let data_extent = last_shard_len
                .saturating_sub(plan.block_offset(block))
                .min(block_len as u64);
            let data_extent_u32 = u32::try_from(data_extent).map_err(|_| Error::PlanOverflow)?;

            let chunk_start = out.len();
            out.extend_from_slice(b"{RB}");
            out.extend_from_slice(&0u64.to_le_bytes());
            out.extend_from_slice(&total_size.to_le_bytes());
            out.extend_from_slice(&header_size_u32.to_le_bytes());
            out.push(1);
            out.push(1);
            out.extend_from_slice(&plan.block_offset(block).to_le_bytes());
            out.extend_from_slice(&data_extent_u32.to_le_bytes());
            out.extend_from_slice(&archive_len.to_le_bytes());
            out.extend_from_slice(&plan.group_count.to_le_bytes());
            out.extend_from_slice(&plan.shard_size.to_le_bytes());
            out.extend_from_slice(&data_shards_u16.to_le_bytes());
            out.extend_from_slice(&recovery_shards_u16.to_le_bytes());
            out.extend_from_slice(
                &u16::try_from(shard_index)
                    .map_err(|_| Error::PlanOverflow)?
                    .to_le_bytes(),
            );
            for &state in block_states
                .get(usize::try_from(block).map_err(|_| Error::PlanOverflow)?)
                .ok_or(Error::PlanOverflow)?
            {
                out.extend_from_slice(&state.to_le_bytes());
            }
            out.extend_from_slice(&final_state.to_le_bytes());
            if out.len() - chunk_start != header_size {
                return Err(Error::PlanOverflow);
            }
            out.extend_from_slice(payload_slice);
            let chunk_end = out.len();
            if chunk_end - chunk_start != total_size as usize {
                return Err(Error::PlanOverflow);
            }

            let crc = crc64_xz(
                out.get(chunk_start + 0x0c..chunk_end)
                    .ok_or(Error::PlanOverflow)?,
            );
            out.get_mut(chunk_start + 0x04..chunk_start + 0x0c)
                .ok_or(Error::PlanOverflow)?
                .copy_from_slice(&crc.to_le_bytes());
        }
    }
    if out.len() != total_len {
        return Err(Error::PlanOverflow);
    }
    debug_assert_eq!(parity.len(), recovery_shards);
    debug_assert_eq!(block_states.len() as u64, plan.blocks_per_shard());
    debug_assert!(
        block_states
            .iter()
            .all(|states| states.len() == data_shards)
    );
    Ok(out)
}

/// `col * x` for every 16-bit symbol `x`, split into one table per byte: since
/// multiplication is linear over XOR, `col * x == low[x & 0xff] ^ high[x >> 8]`.
fn mul_tables(gf: &Gf16, col: u16) -> ([u16; 256], [u16; 256]) {
    let mut low = [0u16; 256];
    let mut high = [0u16; 256];
    for x in 0..256usize {
        low[x] = gf.mul(col, x as u16);
        high[x] = gf.mul(col, (x as u16) << 8);
    }
    (low, high)
}

/// The scalar form of one parity row's shard XOR: two 256-entry tables per
/// coefficient, one per byte of the 16-bit symbol.
fn xor_row_scalar(buf: &[u8], parity: &mut [u8], low: &[u16; 256], high: &[u16; 256]) {
    let (pairs, _) = buf.as_chunks::<2>();
    let (outs, _) = parity.as_chunks_mut::<2>();
    for (pair, out) in pairs.iter().zip(outs.iter_mut()) {
        let symbol = low[pair[0] as usize] ^ high[pair[1] as usize];
        out[0] ^= symbol as u8;
        out[1] ^= (symbol >> 8) as u8;
    }
}

#[cfg(target_arch = "x86_64")]
mod ssse3 {
    use super::Gf16;
    use std::arch::x86_64::*;

    /// One coefficient's multiply as eight 16-entry nibble tables: `mul` splits a
    /// 16-bit symbol into four nibbles and multiplication is linear over XOR, so
    /// the product is the XOR of the four tables' entries. Each table is stored
    /// as exactly sixteen bytes because `pshufb` reads the register that way.
    pub(super) struct RowTables {
        low: [[u8; 16]; 4],
        high: [[u8; 16]; 4],
    }

    impl RowTables {
        pub(super) fn new(gf: &Gf16, col: u16) -> Self {
            let mut low = [[0u8; 16]; 4];
            let mut high = [[0u8; 16]; 4];
            for (k, (low, high)) in low.iter_mut().zip(high.iter_mut()).enumerate() {
                for v in 0..16usize {
                    let product = gf.mul(col, (v as u16) << (4 * k as u16));
                    low[v] = product as u8;
                    high[v] = (product >> 8) as u8;
                }
            }
            Self { low, high }
        }
    }

    pub(super) fn available() -> bool {
        std::arch::is_x86_feature_detected!("ssse3")
    }

    /// XOR `col * symbol` for eight symbols (sixteen bytes) per iteration.
    ///
    /// The symbols' low bytes are gathered into lanes 0-7, then their two nibbles
    /// feed the first two tables; the high bytes likewise feed the last two. The
    /// low and high result bytes accumulate separately and are interleaved back
    /// into symbol order at the end.
    ///
    /// # Safety
    /// Requires ssse3; `parity` must be at least as long as `buf`.
    #[target_feature(enable = "ssse3")]
    pub(super) unsafe fn apply(buf: &[u8], parity: &mut [u8], tables: &RowTables) {
        // SAFETY: ssse3 is enabled for this function and the caller checks it; the
        // loads and stores stay inside `buf`/`parity` because both come from the
        // same `chunks_exact(16)` ranges.
        unsafe {
            let even = _mm_setr_epi8(0, 2, 4, 6, 8, 10, 12, 14, -1, -1, -1, -1, -1, -1, -1, -1);
            let odd = _mm_setr_epi8(1, 3, 5, 7, 9, 11, 13, 15, -1, -1, -1, -1, -1, -1, -1, -1);
            let nibble = _mm_set1_epi8(0x0f);
            let low: [__m128i; 4] =
                std::array::from_fn(|k| _mm_loadu_si128(tables.low[k].as_ptr().cast()));
            let high: [__m128i; 4] =
                std::array::from_fn(|k| _mm_loadu_si128(tables.high[k].as_ptr().cast()));
            let (blocks, _) = buf.as_chunks::<16>();
            let (outs, _) = parity.as_chunks_mut::<16>();
            for (src, dst) in blocks.iter().zip(outs.iter_mut()) {
                let symbols = _mm_loadu_si128(src.as_ptr().cast());
                let lo_bytes = _mm_shuffle_epi8(symbols, even);
                let hi_bytes = _mm_shuffle_epi8(symbols, odd);
                let n0 = _mm_and_si128(lo_bytes, nibble);
                let n1 = _mm_and_si128(_mm_srli_epi16(lo_bytes, 4), nibble);
                let n2 = _mm_and_si128(hi_bytes, nibble);
                let n3 = _mm_and_si128(_mm_srli_epi16(hi_bytes, 4), nibble);
                let lo = _mm_xor_si128(
                    _mm_xor_si128(_mm_shuffle_epi8(low[0], n0), _mm_shuffle_epi8(low[1], n1)),
                    _mm_xor_si128(_mm_shuffle_epi8(low[2], n2), _mm_shuffle_epi8(low[3], n3)),
                );
                let hi = _mm_xor_si128(
                    _mm_xor_si128(_mm_shuffle_epi8(high[0], n0), _mm_shuffle_epi8(high[1], n1)),
                    _mm_xor_si128(_mm_shuffle_epi8(high[2], n2), _mm_shuffle_epi8(high[3], n3)),
                );
                let product = _mm_unpacklo_epi8(lo, hi);
                let prior = _mm_loadu_si128(dst.as_ptr().cast());
                _mm_storeu_si128(dst.as_mut_ptr().cast(), _mm_xor_si128(prior, product));
            }
        }
    }
}

/// Read up to `buf.len()` bytes from `reader`, returning the count actually read
/// (0 on a clean EOF). Like `read_exact` but tolerant of a short/empty tail so
/// callers can pad the remainder with zeros instead of erroring.
fn read_prefix_chunk<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e.to_string())),
        }
    }
    Ok(filled)
}

/// Streaming variant of [`build_structural_inline_recovery_data`]: the archive
/// prefix is read incrementally from `reader` instead of being buffered whole.
///
/// The RAR5 recovery parity is computed shard-by-shard. The prefix is laid out
/// shard-major (shard 0 occupies the first `group_count` bytes, shard 1 the
/// next, …), so a sequential read visits the shards in order and each shard
/// contributes to the `recovery_shards` parity buffers via the fixed
/// Reed–Solomon encoder matrix. Memory therefore tracks the *parity* size
/// (≈ `recovery_percent`% of the archive) rather than the archive itself, so
/// arbitrarily large archives can carry a recovery record without a
/// multi-gigabyte in-RAM buffer.
///
/// `archive_size` must equal the exact prefix length (the current write
/// position); it plans the sharding only and is never buffered.
pub(crate) fn build_structural_inline_recovery_data_streaming<R: Read>(
    mut reader: R,
    archive_size: u64,
    recovery_percent: u64,
    progress: Option<ProgressReporter<'_>>,
    pass: usize,
) -> Result<Vec<u8>> {
    let plan = plan_inline_recovery(archive_size, recovery_percent)?;
    let total_bytes = plan.payload_size()?;
    if let Some(p) = &progress {
        p.report(WriteProgressEvent::OperationStarted {
            operation: WriteOperation::Recovery,
            total_bytes: Some(total_bytes),
            total_entries: None,
            pass,
        });
    }

    let data_shards = usize::try_from(plan.data_shards).map_err(|_| Error::PlanOverflow)?;
    let recovery_shards = usize::try_from(plan.recovery_shards).map_err(|_| Error::PlanOverflow)?;
    let group_count = usize::try_from(plan.group_count).map_err(|_| Error::PlanOverflow)?;
    let matrix = make_encoder_matrix(data_shards, recovery_shards)?;
    let gf = shared_gf16();

    let mut parity: Vec<Vec<u8>> = vec![vec![0u8; group_count]; recovery_shards];
    let blocks = plan.blocks_per_shard();
    let mut block_states: Vec<Vec<u64>> =
        vec![vec![0u64; data_shards]; usize::try_from(blocks).map_err(|_| Error::PlanOverflow)?];
    let block_len = usize::try_from(super::RAR5_RECOVERY_BLOCK_PAYLOAD_SIZE)
        .map_err(|_| Error::PlanOverflow)?;

    for s in 0..data_shards {
        // Fresh, zeroed buffer each shard: a partial trailing shard must pad with
        // zeros (matching `split_prefix_shards`), not the previous shard's bytes.
        let mut buf = vec![0u8; group_count];
        let got = read_prefix_chunk(&mut reader, &mut buf)?;
        // CRC over the *real* prefix bytes only, one entry per 64 KiB block —
        // `split_prefix_shards` treats overflow shards as empty slices, so an
        // EOF (got == 0) yields the crc64 of nothing, matching the buffered
        // path. A block that starts past the real bytes covers nothing.
        for (block, states) in block_states.iter_mut().enumerate() {
            let start = block.saturating_mul(block_len).min(got);
            let end = start.saturating_add(block_len).min(got);
            states[s] = crc64_rar_state(&buf[start..end]);
        }
        // The coefficient is fixed for the whole row, and multiplication is linear
        // over XOR, so `col * symbol` splits into one table per byte of the
        // symbol: `low[lo] ^ high[hi]`. The log/antilog pair that used to do this
        // is 256 KiB and 512 KiB, so every symbol paid a cache miss; these two
        // tables are 1 KiB and stay in L1. Rows are independent, which is what
        // makes the shard's work splittable across the pool — the shard buffer is
        // read-only, and each row is written by exactly one worker, so the output
        // does not depend on the schedule.
        let apply = |j: usize, parity: &mut [u8]| {
            let col = matrix[j][s];
            #[cfg(target_arch = "x86_64")]
            {
                // Eight symbols per `pshufb` iteration; the shard's tail, and any
                // CPU without ssse3, go through the scalar form.
                if ssse3::available() {
                    let tables = ssse3::RowTables::new(gf, col);
                    let whole = buf.len() & !15;
                    // SAFETY: ssse3 is present (just checked) and `whole` keeps
                    // both slices on their 16-byte grid, so `apply`'s loads and
                    // stores stay in bounds.
                    unsafe { ssse3::apply(&buf[..whole], &mut parity[..whole], &tables) };
                    let (low, high) = mul_tables(gf, col);
                    xor_row_scalar(&buf[whole..], &mut parity[whole..], &low, &high);
                    return;
                }
            }
            let (low, high) = mul_tables(gf, col);
            xor_row_scalar(&buf, parity, &low, &high);
        };
        #[cfg(feature = "parallel")]
        {
            use rayon::prelude::*;
            parity
                .par_iter_mut()
                .enumerate()
                .for_each(|(j, parity)| apply(j, parity));
        }
        #[cfg(not(feature = "parallel"))]
        for (j, parity) in parity.iter_mut().enumerate() {
            apply(j, parity);
        }
    }

    // Deterministic stand-in for WinRAR's random nonce (see the buffered path).
    let final_state = parity
        .first()
        .map(|payload| crc64_rar_state(payload))
        .unwrap_or(0);

    let out = assemble_recovery_data(plan, parity, block_states, final_state, archive_size)?;
    if let Some(p) = &progress {
        p.report(WriteProgressEvent::OperationFinished {
            operation: WriteOperation::Recovery,
            total_bytes: Some(total_bytes),
            total_entries: None,
            pass,
        });
    }
    Ok(out)
}
