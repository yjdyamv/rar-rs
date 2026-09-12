//! In-memory repair: locate `"RR"` chunks, reconstruct damaged shards and
//! patch the archive prefix.

use super::gf16::{apply_inverse_matrix, invert_linear_system_matrix, read_u32, read_u64};
use super::plan::{InlineRecoveryPlan, split_prefix_shard_ranges, split_prefix_shards};
use super::{
    Error, RAR5_RECOVERY_CHUNK_FIXED_HEADER_SIZE, Result, check_cancel, crc64_rar_state, crc64_xz,
    make_encoder_matrix, shared_gf16,
};
#[derive(Debug, Clone)]
pub(super) struct InlineRecoveryChunk {
    pub(super) plan: InlineRecoveryPlan,
    pub(super) protected_size: u64,
    pub(super) shard_index: usize,
    pub(super) data_shard_states: Vec<u64>,
    pub(super) parity: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(super) struct FoundInlineRecoveryChunk {
    pub(super) offset: usize,
    pub(super) chunk: InlineRecoveryChunk,
}

pub fn repair_inline_recovery_prefix(
    archive_prefix: &[u8],
    recovery_data: &[u8],
) -> Result<Vec<u8>> {
    let chunks = parse_available_inline_recovery_chunks(recovery_data)?;
    let first = chunks.first().ok_or(Error::BadRecoveryChunk)?;
    let plan = first.plan;
    if first.protected_size != archive_prefix.len() as u64 {
        return Err(Error::BadRecoveryChunk);
    }
    if chunks.iter().any(|chunk| {
        chunk.plan != plan
            || chunk.protected_size != first.protected_size
            || chunk.data_shard_states != first.data_shard_states
    }) {
        return Err(Error::BadRecoveryChunk);
    }

    let mut data_shards = split_prefix_shards(archive_prefix, plan)?;
    let shard_ranges = split_prefix_shard_ranges(archive_prefix.len(), plan)?;
    let damaged: Vec<usize> = shard_ranges
        .iter()
        .enumerate()
        .filter_map(|(index, range)| {
            (crc64_rar_state(&archive_prefix[range.clone()]) != first.data_shard_states[index])
                .then_some(index)
        })
        .collect();
    if damaged.is_empty() {
        return Ok(archive_prefix.to_vec());
    }

    // Relocated phase: a damaged shard can also be repaired by copying an
    // identical byte sequence that survives elsewhere in the archive, e.g.
    // the data block of another file that was packed with identical content
    // (WinRAR stores such blocks byte-identically in one run). The candidate
    // copy is validated against the shard's expected CRC64, so a wrong or
    // itself-damaged source can never corrupt the archive.
    let file_blocks = parse_file_data_blocks(archive_prefix)?;
    let mut remaining: Vec<usize> = Vec::with_capacity(damaged.len());
    for &index in &damaged {
        let range = &shard_ranges[index];
        let Some((b1_start, _b1_end)) = file_blocks
            .iter()
            .filter(|&&(start, end)| range.start < end && start < range.end)
            .max_by_key(|&&(start, end)| range.end.min(end).saturating_sub(range.start.max(start)))
            .copied()
        else {
            remaining.push(index);
            continue;
        };
        let mut relocated = false;
        for &(b2_start, b2_end) in &file_blocks {
            if b2_start == b1_start && b2_end == _b1_end {
                continue;
            }
            let off = b2_start as isize - b1_start as isize;
            let cand_start = range.start as isize + off;
            let cand_end = range.end as isize + off;
            if cand_start < 0 || cand_end > archive_prefix.len() as isize {
                continue;
            }
            let candidate = &archive_prefix[cand_start as usize..cand_end as usize];
            if crc64_rar_state(candidate) == first.data_shard_states[index] {
                data_shards[index] = candidate.to_vec();
                relocated = true;
                break;
            }
        }
        if !relocated {
            remaining.push(index);
        }
    }

    if !remaining.is_empty() && remaining.len() > chunks.len() {
        return Err(Error::TooManyDamagedShards);
    }

    if !remaining.is_empty() {
        let recovery_rows: Vec<_> = chunks[..remaining.len()]
            .iter()
            .map(|chunk| (chunk.shard_index, chunk.parity.as_slice()))
            .collect();
        recover_damaged_shards(&mut data_shards, &remaining, &recovery_rows)?;
    }

    let mut repaired = Vec::with_capacity(archive_prefix.len());
    for (shard, range) in data_shards.iter().zip(shard_ranges) {
        repaired.extend_from_slice(&shard[..range.len()]);
    }
    debug_assert_eq!(repaired.len(), archive_prefix.len());
    Ok(repaired)
}

/// Walk the RAR5 blocks inside a protected prefix and collect the data
/// ranges of file blocks (header type 2). Ranges are absolute offsets
/// within `prefix`. Block layout follows the RAR5 spec: 4-byte header
/// CRC32, vint header size, then a vint stream of type/flags/extra/data
/// sizes followed by the header body; the data area trails the header.
fn parse_file_data_blocks(prefix: &[u8]) -> Result<Vec<(usize, usize)>> {
    let mut blocks = Vec::new();
    if prefix.len() < 8 || &prefix[..7] != b"Rar!\x1a\x07\x01" {
        return Ok(blocks);
    }
    let mut pos = 8usize;
    while pos + 4 < prefix.len() {
        let (header_size, size_bytes) =
            crate::format::rar5::vint::decode_from_slice(prefix, pos + 4)
                .map_err(|_| Error::BadRecoveryChunk)?;
        if header_size == 0 || header_size > 2 * 1024 * 1024 {
            break;
        }
        let body = pos + 4 + size_bytes;
        let block_end = body
            .checked_add(header_size as usize)
            .ok_or(Error::PlanOverflow)?;
        if block_end > prefix.len() {
            break;
        }
        let (block_type, t) = crate::format::rar5::vint::decode_from_slice(prefix, body)
            .map_err(|_| Error::BadRecoveryChunk)?;
        let (flags, f) = crate::format::rar5::vint::decode_from_slice(prefix, body + t)
            .map_err(|_| Error::BadRecoveryChunk)?;
        let mut p = body + t + f;
        if flags & 0x1 != 0 {
            let (_, n) = crate::format::rar5::vint::decode_from_slice(prefix, p)
                .map_err(|_| Error::BadRecoveryChunk)?;
            p += n;
        }
        let mut data_size = 0u64;
        if flags & 0x2 != 0 {
            let (v, _n) = crate::format::rar5::vint::decode_from_slice(prefix, p)
                .map_err(|_| Error::BadRecoveryChunk)?;
            data_size = v;
        }
        if block_type == 2 {
            let data_start = block_end;
            let data_end = data_start
                .checked_add(data_size as usize)
                .ok_or(Error::PlanOverflow)?;
            if data_end <= prefix.len() {
                blocks.push((data_start, data_end));
            }
        }
        let next = block_end
            .checked_add(data_size as usize)
            .ok_or(Error::PlanOverflow)?;
        if next <= pos {
            break;
        }
        pos = next;
        if block_type == 5 {
            break;
        }
    }
    Ok(blocks)
}

/// Repair damaged RAR5 inline-recovery data shards without materializing the
/// whole protected prefix.
///
/// `read_range` receives byte ranges relative to the protected prefix and must
/// return the current bytes for each requested range. The returned pairs contain
/// only the damaged prefix ranges that need to be written back.
pub fn repair_inline_recovery_prefix_shards<F>(
    protected_size: usize,
    recovery_data: &[u8],
    mut read_range: F,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Vec<(std::ops::Range<usize>, Vec<u8>)>>
where
    F: FnMut(std::ops::Range<usize>) -> Result<Vec<u8>>,
{
    let chunks = parse_available_inline_recovery_chunks(recovery_data)?;
    let first = chunks.first().ok_or(Error::BadRecoveryChunk)?;
    if first.protected_size != protected_size as u64 {
        return Err(Error::BadRecoveryChunk);
    }
    if chunks.iter().any(|chunk| {
        chunk.plan != first.plan
            || chunk.protected_size != first.protected_size
            || chunk.data_shard_states != first.data_shard_states
    }) {
        return Err(Error::BadRecoveryChunk);
    }

    let plan = first.plan;
    let shard_len = usize::try_from(plan.group_count).map_err(|_| Error::PlanOverflow)?;
    if !shard_len.is_multiple_of(2) {
        return Err(Error::OddShardSize);
    }
    let shard_ranges = split_prefix_shard_ranges(protected_size, plan)?;
    let mut damaged = Vec::new();
    for (index, range) in shard_ranges.iter().enumerate() {
        check_cancel(cancel)?;
        let shard = read_range(range.clone())?;
        if crc64_rar_state(&shard) != first.data_shard_states[index] {
            damaged.push(index);
        }
    }
    if damaged.is_empty() {
        return Ok(Vec::new());
    }
    if damaged.len() > chunks.len() {
        return Err(Error::TooManyDamagedShards);
    }

    let recovery_rows: Vec<_> = chunks[..damaged.len()]
        .iter()
        .map(|chunk| (chunk.shard_index, chunk.parity.as_slice()))
        .collect();
    if recovery_rows
        .iter()
        .any(|(_, shard)| shard.len() != shard_len)
    {
        return Err(Error::ShardSizeMismatch);
    }
    let matrix = make_encoder_matrix(shard_ranges.len(), plan.recovery_shards as usize)?;
    let equations: Vec<Vec<u16>> = recovery_rows
        .iter()
        .map(|&(row_index, _)| {
            damaged
                .iter()
                .map(|&data_index| matrix[row_index][data_index])
                .collect()
        })
        .collect();
    let gf = shared_gf16();
    let inverse = invert_linear_system_matrix(gf, &equations)?;
    let word_count = shard_len / 2;
    let mut rhs_by_row = recovery_rows
        .iter()
        .map(|(_, parity)| {
            parity
                .as_chunks::<2>()
                .0
                .iter()
                .map(|word| u16::from_le_bytes(*word))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let damaged_lookup = damaged_lookup(shard_ranges.len(), &damaged)?;

    for (data_index, range) in shard_ranges.iter().enumerate() {
        check_cancel(cancel)?;
        if damaged_lookup[data_index] {
            continue;
        }
        let shard = read_padded_prefix_shard(range.clone(), shard_len, &mut read_range)?;
        for (row_index, rhs) in rhs_by_row.iter_mut().enumerate() {
            let coeff = matrix[recovery_rows[row_index].0][data_index];
            if coeff == 0 {
                continue;
            }
            for (word_index, word) in shard.as_chunks::<2>().0.iter().enumerate() {
                let data_symbol = u16::from_le_bytes(*word);
                rhs[word_index] ^= gf.mul(coeff, data_symbol);
            }
        }
    }

    let mut repaired = damaged
        .iter()
        .map(|&index| vec![0; shard_ranges[index].len()])
        .collect::<Vec<_>>();
    for word_index in 0..word_count {
        if (word_index & 0x3FFF) == 0 {
            check_cancel(cancel)?;
        }
        let rhs = rhs_by_row
            .iter()
            .map(|row| row[word_index])
            .collect::<Vec<_>>();
        let solved = apply_inverse_matrix(gf, &inverse, &rhs)?;
        for (output, &symbol) in repaired.iter_mut().zip(&solved) {
            let byte_offset = word_index * 2;
            if byte_offset < output.len() {
                let bytes = symbol.to_le_bytes();
                let take = (output.len() - byte_offset).min(2);
                output[byte_offset..byte_offset + take].copy_from_slice(&bytes[..take]);
            }
        }
    }

    // Verify every solved shard against the state recorded in the record
    // before returning it: the RS solve can mix parity rows from a different
    // archive generation when the file carries two RR chunks whose plan and
    // protected size happen to match.
    let mut verified = Vec::with_capacity(repaired.len());
    for (index, data) in damaged.into_iter().zip(repaired) {
        if crc64_rar_state(&data) != first.data_shard_states[index] {
            return Err(Error::BadRecoveryChunk);
        }
        verified.push((shard_ranges[index].clone(), data));
    }
    Ok(verified)
}

fn damaged_lookup(data_count: usize, damaged: &[usize]) -> Result<Vec<bool>> {
    let mut lookup = vec![false; data_count];
    for &index in damaged {
        if index >= data_count {
            return Err(Error::TooManyDamagedShards);
        }
        lookup[index] = true;
    }
    Ok(lookup)
}

fn read_padded_prefix_shard<F>(
    range: std::ops::Range<usize>,
    shard_len: usize,
    read_range: &mut F,
) -> Result<Vec<u8>>
where
    F: FnMut(std::ops::Range<usize>) -> Result<Vec<u8>>,
{
    let mut shard = vec![0; shard_len];
    let bytes = read_range(range)?;
    if bytes.len() > shard_len {
        return Err(Error::ShardSizeMismatch);
    }
    shard[..bytes.len()].copy_from_slice(&bytes);
    Ok(shard)
}

pub fn repair_inline_recovery_archive(input: &[u8]) -> Result<Vec<u8>> {
    let chunks = find_inline_recovery_chunks(input)?;
    let first = chunks.first().ok_or(Error::BadRecoveryChunk)?;
    let protected_size =
        usize::try_from(first.chunk.protected_size).map_err(|_| Error::PlanOverflow)?;
    if protected_size > input.len() {
        return Err(Error::BadRecoveryChunk);
    }
    let mut recovery_data = Vec::with_capacity(
        chunks
            .iter()
            .map(|found| found.chunk.plan.shard_size as usize)
            .sum(),
    );
    for found in &chunks {
        append_inline_recovery_chunk(input, found, &mut recovery_data)?;
    }
    let repaired_prefix = repair_inline_recovery_prefix(&input[..protected_size], &recovery_data)?;
    if repaired_prefix == input[..protected_size] {
        return Ok(input.to_vec());
    }
    let mut repaired = input.to_vec();
    repaired[..protected_size].copy_from_slice(&repaired_prefix);
    Ok(repaired)
}

fn find_inline_recovery_chunks(input: &[u8]) -> Result<Vec<FoundInlineRecoveryChunk>> {
    let mut chunks = Vec::new();
    let mut offset = 0usize;
    while let Some(relative) = find_recovery_marker(&input[offset..]) {
        let start = offset + relative;
        if let Ok(chunk) = parse_inline_recovery_chunk(&input[start..]) {
            let shard_size =
                usize::try_from(chunk.plan.shard_size).map_err(|_| Error::PlanOverflow)?;
            if input.len().saturating_sub(start) >= shard_size {
                chunks.push(FoundInlineRecoveryChunk {
                    offset: start,
                    chunk,
                });
                offset = start + shard_size;
                continue;
            }
        }
        offset = start + 1;
    }
    if chunks.is_empty() {
        return Err(Error::BadRecoveryChunk);
    }
    Ok(chunks)
}

fn find_recovery_marker(input: &[u8]) -> Option<usize> {
    let mut offset = 0usize;
    while offset + 4 <= input.len() {
        let relative = input[offset..].iter().position(|&byte| byte == b'{')?;
        offset += relative;
        if input.get(offset..offset + 4) == Some(b"{RB}") {
            return Some(offset);
        }
        offset += 1;
    }
    None
}

fn append_inline_recovery_chunk(
    input: &[u8],
    found: &FoundInlineRecoveryChunk,
    out: &mut Vec<u8>,
) -> Result<()> {
    let shard_size =
        usize::try_from(found.chunk.plan.shard_size).map_err(|_| Error::PlanOverflow)?;
    let start = found.offset;
    let end = start.checked_add(shard_size).ok_or(Error::PlanOverflow)?;
    out.extend_from_slice(input.get(start..end).ok_or(Error::BadRecoveryChunk)?);
    Ok(())
}

pub fn reconstruct_data_shards(
    data_shards: &[Option<&[u8]>],
    recovery_shards: &[(usize, &[u8])],
) -> Result<Vec<Vec<u8>>> {
    if data_shards.is_empty() {
        return Err(Error::TooManyShards);
    }
    let shard_len = recovery_shards
        .first()
        .map(|(_, shard)| shard.len())
        .or_else(|| data_shards.iter().flatten().map(|shard| shard.len()).max())
        .ok_or(Error::TooManyDamagedShards)?;
    if !shard_len.is_multiple_of(2) {
        return Err(Error::OddShardSize);
    }
    if recovery_shards
        .iter()
        .any(|(_, shard)| shard.len() != shard_len)
    {
        return Err(Error::ShardSizeMismatch);
    }

    let mut out = Vec::with_capacity(data_shards.len());
    let mut missing = Vec::new();
    for (index, shard) in data_shards.iter().enumerate() {
        let mut padded = vec![0; shard_len];
        if let Some(shard) = shard {
            if shard.len() > shard_len {
                return Err(Error::ShardSizeMismatch);
            }
            padded[..shard.len()].copy_from_slice(shard);
        } else {
            missing.push(index);
        }
        out.push(padded);
    }
    if missing.is_empty() {
        return Ok(out);
    }
    if missing.len() > recovery_shards.len() {
        return Err(Error::TooManyDamagedShards);
    }
    recover_damaged_shards(&mut out, &missing, &recovery_shards[..missing.len()])?;
    Ok(out)
}

fn parse_available_inline_recovery_chunks(
    recovery_data: &[u8],
) -> Result<Vec<InlineRecoveryChunk>> {
    Ok(find_inline_recovery_chunks(recovery_data)?
        .into_iter()
        .map(|found| found.chunk)
        .collect())
}

pub(super) fn parse_inline_recovery_chunk(input: &[u8]) -> Result<InlineRecoveryChunk> {
    if input.len() < 0x48 || &input[..4] != b"{RB}" {
        return Err(Error::BadRecoveryChunk);
    }
    let total_size = read_u32(input, 0x0c)? as u64;
    let header_size = read_u32(input, 0x10)? as u64;
    if header_size < RAR5_RECOVERY_CHUNK_FIXED_HEADER_SIZE || header_size > total_size {
        return Err(Error::BadRecoveryChunk);
    }
    let total_size_usize = usize::try_from(total_size).map_err(|_| Error::PlanOverflow)?;
    let header_size_usize = usize::try_from(header_size).map_err(|_| Error::PlanOverflow)?;
    if input.len() < total_size_usize {
        return Err(Error::BadRecoveryChunk);
    }
    let expected_crc = read_u64(input, 0x04)?;
    let actual_crc = crc64_xz(&input[0x0c..total_size_usize]);
    if actual_crc != expected_crc {
        return Err(Error::BadRecoveryChunk);
    }
    if input[0x14] != 1 || input[0x15] != 1 {
        return Err(Error::BadRecoveryChunk);
    }

    let protected_size = read_u64(input, 0x22)?;
    let group_count = read_u64(input, 0x2a)?;
    let shard_size = read_u64(input, 0x32)?;
    let data_shards = u16::from_le_bytes(input[0x3a..0x3c].try_into().unwrap()) as u64;
    let recovery_shards = u16::from_le_bytes(input[0x3c..0x3e].try_into().unwrap()) as u64;
    let shard_index = u16::from_le_bytes(input[0x3e..0x40].try_into().unwrap()) as usize;
    let plan = InlineRecoveryPlan {
        data_shards,
        recovery_shards,
        group_count,
        header_size,
        shard_size,
    };
    if plan.payload_size()?
        != recovery_shards
            .checked_mul(shard_size)
            .ok_or(Error::PlanOverflow)?
        || shard_size != total_size
        || shard_index >= recovery_shards as usize
        || header_size_usize != 0x48 + data_shards as usize * 8
        || total_size_usize < header_size_usize
    {
        return Err(Error::BadRecoveryChunk);
    }

    let mut data_shard_states = Vec::with_capacity(data_shards as usize);
    let mut pos = 0x40;
    for _ in 0..data_shards {
        data_shard_states.push(read_u64(input, pos)?);
        pos += 8;
    }
    let _final_state = read_u64(input, pos)?;
    let parity = input[header_size_usize..total_size_usize].to_vec();
    if parity.len() as u64 != group_count {
        return Err(Error::BadRecoveryChunk);
    }
    Ok(InlineRecoveryChunk {
        plan,
        protected_size,
        shard_index,
        data_shard_states,
        parity,
    })
}

fn recover_damaged_shards(
    data_shards: &mut [Vec<u8>],
    damaged: &[usize],
    recovery_shards: &[(usize, &[u8])],
) -> Result<()> {
    let data_count = data_shards.len();
    let mut damaged_lookup = vec![false; data_count];
    for &data_index in damaged {
        if data_index >= data_count {
            return Err(Error::TooManyDamagedShards);
        }
        damaged_lookup[data_index] = true;
    }

    let recovery_count = recovery_shards
        .iter()
        .map(|(row, _)| row + 1)
        .max()
        .ok_or(Error::TooManyDamagedShards)?;
    let matrix = make_encoder_matrix(data_count, recovery_count)?;
    let gf = shared_gf16();
    let equations: Vec<Vec<u16>> = recovery_shards
        .iter()
        .map(|&(row_index, _)| {
            damaged
                .iter()
                .map(|&data_index| matrix[row_index][data_index])
                .collect()
        })
        .collect();
    let inverse = invert_linear_system_matrix(gf, &equations)?;

    let shard_len = data_shards.first().ok_or(Error::TooManyShards)?.len();
    for word_offset in (0..shard_len).step_by(2) {
        let mut rhs = Vec::with_capacity(recovery_shards.len());
        for &(row_index, parity) in recovery_shards {
            let mut value = u16::from_le_bytes([parity[word_offset], parity[word_offset + 1]]);
            for (data_index, shard) in data_shards.iter().enumerate() {
                if damaged_lookup[data_index] {
                    continue;
                }
                let data_symbol = u16::from_le_bytes([shard[word_offset], shard[word_offset + 1]]);
                value ^= gf.mul(matrix[row_index][data_index], data_symbol);
            }
            rhs.push(value);
        }
        let solved = apply_inverse_matrix(gf, &inverse, &rhs)?;
        for (&data_index, &symbol) in damaged.iter().zip(&solved) {
            data_shards[data_index][word_offset..word_offset + 2]
                .copy_from_slice(&symbol.to_le_bytes());
        }
    }
    Ok(())
}
