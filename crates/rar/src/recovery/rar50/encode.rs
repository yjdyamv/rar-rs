//! Record builder: parity computation and the `"RR"` chunk payload.

use std::io::{self, Read};

use super::gf16::{encode_parity_shards_with_progress, make_encoder_matrix};
use super::plan::{
    InlineRecoveryPlan, crc64_rar_state, crc64_xz, plan_inline_recovery, split_prefix_shard_ranges,
    split_prefix_shards,
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
    let data_shard_states: Vec<u64> = shard_ranges
        .iter()
        .map(|range| crc64_rar_state(&archive_prefix[range.clone()]))
        .collect();
    let final_state = parity
        .first()
        .map(|payload| crc64_rar_state(payload))
        .unwrap_or(0);
    let chunk_data_extent = shard_ranges.last().map_or(0usize, std::ops::Range::len);
    assemble_recovery_data(
        plan,
        parity,
        data_shard_states,
        final_state,
        archive_prefix.len(),
        chunk_data_extent,
    )
}

/// Assemble the on-disk recovery-record payload from a finished parity plan.
/// Shared by the buffered and streaming encoders so both emit byte-identical
/// output — the bytes must match WinRAR / UnRAR for the archive to be repairable.
fn assemble_recovery_data(
    plan: InlineRecoveryPlan,
    parity: Vec<Vec<u8>>,
    data_shard_states: Vec<u64>,
    final_state: u64,
    archive_len: usize,
    chunk_data_extent: usize,
) -> Result<Vec<u8>> {
    let total_len = usize::try_from(plan.payload_size()?).map_err(|_| Error::PlanOverflow)?;
    let header_size = usize::try_from(plan.header_size).map_err(|_| Error::PlanOverflow)?;
    let shard_size = usize::try_from(plan.shard_size).map_err(|_| Error::PlanOverflow)?;
    let data_shards = usize::try_from(plan.data_shards).map_err(|_| Error::PlanOverflow)?;
    let recovery_shards = usize::try_from(plan.recovery_shards).map_err(|_| Error::PlanOverflow)?;
    let total_size = u32::try_from(plan.shard_size).map_err(|_| Error::PlanOverflow)?;
    let header_size_u32 = u32::try_from(plan.header_size).map_err(|_| Error::PlanOverflow)?;
    let data_shards_u16 = u16::try_from(plan.data_shards).map_err(|_| Error::PlanOverflow)?;
    let recovery_shards_u16 =
        u16::try_from(plan.recovery_shards).map_err(|_| Error::PlanOverflow)?;
    let chunk_data_extent_u32 =
        u32::try_from(chunk_data_extent).map_err(|_| Error::PlanOverflow)?;

    let mut out = Vec::with_capacity(total_len);
    for (shard_index, payload) in parity.iter().enumerate() {
        if payload.len() + header_size != shard_size {
            return Err(Error::PlanOverflow);
        }

        let chunk_start = out.len();
        out.extend_from_slice(b"{RB}");
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&total_size.to_le_bytes());
        out.extend_from_slice(&header_size_u32.to_le_bytes());
        out.push(1);
        out.push(1);
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&chunk_data_extent_u32.to_le_bytes());
        out.extend_from_slice(&(archive_len as u64).to_le_bytes());
        out.extend_from_slice(&plan.group_count.to_le_bytes());
        out.extend_from_slice(&plan.shard_size.to_le_bytes());
        out.extend_from_slice(&data_shards_u16.to_le_bytes());
        out.extend_from_slice(&recovery_shards_u16.to_le_bytes());
        out.extend_from_slice(
            &u16::try_from(shard_index)
                .map_err(|_| Error::PlanOverflow)?
                .to_le_bytes(),
        );
        for &state in &data_shard_states {
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.extend_from_slice(&final_state.to_le_bytes());
        if out.len() - chunk_start != header_size {
            return Err(Error::PlanOverflow);
        }
        out.extend_from_slice(payload);
        if out.len() - chunk_start != shard_size {
            return Err(Error::PlanOverflow);
        }

        let chunk_end = chunk_start
            .checked_add(shard_size)
            .ok_or(Error::PlanOverflow)?;
        let crc_start = chunk_start.checked_add(0x0c).ok_or(Error::PlanOverflow)?;
        let crc = crc64_xz(out.get(crc_start..chunk_end).ok_or(Error::PlanOverflow)?);
        let crc_field_start = chunk_start.checked_add(0x04).ok_or(Error::PlanOverflow)?;
        let crc_field_end = chunk_start.checked_add(0x0c).ok_or(Error::PlanOverflow)?;
        out.get_mut(crc_field_start..crc_field_end)
            .ok_or(Error::PlanOverflow)?
            .copy_from_slice(&crc.to_le_bytes());
    }
    if out.len() != total_len {
        return Err(Error::PlanOverflow);
    }
    debug_assert_eq!(parity.len(), recovery_shards);
    debug_assert_eq!(data_shard_states.len(), data_shards);
    Ok(out)
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
    let mut data_shard_states = vec![0u64; data_shards];

    for s in 0..data_shards {
        // Fresh, zeroed buffer each shard: a partial trailing shard must pad with
        // zeros (matching `split_prefix_shards`), not the previous shard's bytes.
        let mut buf = vec![0u8; group_count];
        let got = read_prefix_chunk(&mut reader, &mut buf)?;
        // CRC over the *real* prefix bytes only — `split_prefix_shards` treats
        // overflow shards as empty slices, so an EOF (got == 0) yields the crc64
        // of nothing, matching the buffered path.
        data_shard_states[s] = crc64_rar_state(&buf[..got]);
        for (j, row) in matrix.iter().enumerate() {
            let col = row[s];
            for word_offset in (0..group_count).step_by(2) {
                let data_symbol = u16::from_le_bytes([buf[word_offset], buf[word_offset + 1]]);
                let symbol = gf.mul(col, data_symbol);
                let bytes = symbol.to_le_bytes();
                parity[j][word_offset] ^= bytes[0];
                parity[j][word_offset + 1] ^= bytes[1];
            }
        }
    }

    let final_state = parity
        .first()
        .map(|payload| crc64_rar_state(payload))
        .unwrap_or(0);

    // Last shard's real extent, carried in the structural header.
    let chunk_data_extent = (archive_size as usize)
        .saturating_sub((data_shards - 1) * group_count)
        .min(group_count);

    let out = assemble_recovery_data(
        plan,
        parity,
        data_shard_states,
        final_state,
        archive_size as usize,
        chunk_data_extent,
    )?;
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
