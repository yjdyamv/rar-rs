//! Packing plan and prefix geometry for the inline recovery record.

use super::{CRC64_XZ_INIT, CRC64_XZ_POLY};
use super::{Error, KIB, MAX_WINRAR602_DATA_SHARDS, RAR5_RECOVERY_CHUNK_FIXED_HEADER_SIZE, Result};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct InlineRecoveryPlan {
    pub data_shards: u64,
    pub recovery_shards: u64,
    pub group_count: u64,
    pub header_size: u64,
    pub shard_size: u64,
}

impl InlineRecoveryPlan {
    pub fn payload_size(self) -> Result<u64> {
        self.recovery_shards
            .checked_mul(self.shard_size)
            .ok_or(Error::PlanOverflow)
    }
}

pub fn plan_inline_recovery(
    archive_size: u64,
    recovery_percent: u64,
) -> Result<InlineRecoveryPlan> {
    let pct = recovery_percent.min(100);
    let data_shards = if archive_size >= 200 * KIB {
        MAX_WINRAR602_DATA_SHARDS
    } else {
        archive_size.div_ceil(KIB).max(1)
    };
    let mut recovery_shards = (2 * pct * data_shards) / 200;
    recovery_shards = recovery_shards.min(data_shards);
    if recovery_shards == 0 && archive_size < 200 * KIB {
        recovery_shards = 1;
    }
    let mut group_count = archive_size.div_ceil(data_shards);
    group_count += group_count & 1;
    let header_size = data_shards
        .checked_mul(8)
        .and_then(|value| value.checked_add(RAR5_RECOVERY_CHUNK_FIXED_HEADER_SIZE))
        .ok_or(Error::PlanOverflow)?;
    let shard_size = header_size
        .checked_add(group_count)
        .ok_or(Error::PlanOverflow)?;

    Ok(InlineRecoveryPlan {
        data_shards,
        recovery_shards,
        group_count,
        header_size,
        shard_size,
    })
}

pub fn crc64_xz(data: &[u8]) -> u64 {
    crc64_update(data, CRC64_XZ_INIT) ^ CRC64_XZ_INIT
}

fn crc64_update(data: &[u8], initial: u64) -> u64 {
    let mut crc = initial;
    for &byte in data {
        crc ^= byte as u64;
        for _ in 0..8 {
            let mask = 0u64.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (CRC64_XZ_POLY & mask);
        }
    }
    crc
}

pub fn crc64_rar_state(data: &[u8]) -> u64 {
    crc64_update(data, 0)
}

pub fn split_prefix_shard_ranges(
    prefix_len: usize,
    plan: InlineRecoveryPlan,
) -> Result<Vec<std::ops::Range<usize>>> {
    let data_shards = usize::try_from(plan.data_shards).map_err(|_| Error::PlanOverflow)?;
    let group_count = usize::try_from(plan.group_count).map_err(|_| Error::PlanOverflow)?;
    let capacity = data_shards
        .checked_mul(group_count)
        .ok_or(Error::PlanOverflow)?;
    if prefix_len > capacity {
        return Err(Error::PrefixExceedsPlan);
    }

    let mut ranges = Vec::with_capacity(data_shards);
    for shard_index in 0..data_shards {
        let start = shard_index
            .checked_mul(group_count)
            .ok_or(Error::PlanOverflow)?;
        let end = start.saturating_add(group_count).min(prefix_len);
        ranges.push(start..end);
    }
    Ok(ranges)
}

pub fn split_prefix_shards(prefix: &[u8], plan: InlineRecoveryPlan) -> Result<Vec<Vec<u8>>> {
    let group_count = usize::try_from(plan.group_count).map_err(|_| Error::PlanOverflow)?;
    let ranges = split_prefix_shard_ranges(prefix.len(), plan)?;
    let mut shards = Vec::with_capacity(ranges.len());
    for range in ranges {
        let mut shard = vec![0u8; group_count];
        if range.start < range.end {
            shard[..range.end - range.start].copy_from_slice(&prefix[range]);
        }
        shards.push(shard);
    }
    Ok(shards)
}
