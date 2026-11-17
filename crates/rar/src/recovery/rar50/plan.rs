//! Packing plan and prefix geometry for the inline recovery record.

use super::{CRC64_XZ_INIT, CRC64_XZ_POLY};
use super::{
    Error, KIB, MAX_WINRAR602_DATA_SHARDS, RAR5_RECOVERY_BLOCK_PAYLOAD_SIZE,
    RAR5_RECOVERY_CHUNK_FIXED_HEADER_SIZE, Result,
};
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

    /// Number of `{RB}` chunks one recovery shard is split into.
    ///
    /// WinRAR caps the parity carried by a single chunk at
    /// [`RAR5_RECOVERY_BLOCK_PAYLOAD_SIZE`] bytes and repeats the
    /// [`Self::header_size`]-byte header before each block, so
    /// `shard_size == group_count + blocks_per_shard() * header_size`.
    pub fn blocks_per_shard(self) -> u64 {
        self.group_count
            .div_ceil(RAR5_RECOVERY_BLOCK_PAYLOAD_SIZE)
            .max(1)
    }

    /// Payload length carried by block `block` of a recovery shard.
    pub fn block_payload_len(self, block: u64) -> u64 {
        self.group_count
            .saturating_sub(block.saturating_mul(RAR5_RECOVERY_BLOCK_PAYLOAD_SIZE))
            .min(RAR5_RECOVERY_BLOCK_PAYLOAD_SIZE)
    }

    /// Byte offset of block `block` within its recovery shard's parity.
    pub fn block_offset(self, block: u64) -> u64 {
        block
            .saturating_mul(RAR5_RECOVERY_BLOCK_PAYLOAD_SIZE)
            .min(self.group_count)
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
    let shard_size = {
        // One header per 64 KiB parity block: WinRAR's chunked layout.
        let blocks = group_count
            .div_ceil(RAR5_RECOVERY_BLOCK_PAYLOAD_SIZE)
            .max(1);
        blocks
            .checked_mul(header_size)
            .and_then(|headers| headers.checked_add(group_count))
            .ok_or(Error::PlanOverflow)?
    };

    Ok(InlineRecoveryPlan {
        data_shards,
        recovery_shards,
        group_count,
        header_size,
        shard_size,
    })
}

/// Length of the parity payload the record protecting a `prefix_len`-byte
/// archive at `recovery_percent` will carry.
///
/// Geometry only — no parity is computed — so the volume writers can reserve
/// room for the record before filling a volume.
pub(crate) fn inline_recovery_payload_len(prefix_len: u64, recovery_percent: u64) -> Result<u64> {
    plan_inline_recovery(prefix_len, recovery_percent)?.payload_size()
}

/// CRC-64/XZ over `data`, the checksum the RAR5 recovery record header stores.
pub fn crc64_xz(data: &[u8]) -> u64 {
    crc64_update(data, CRC64_XZ_INIT) ^ CRC64_XZ_INIT
}

/// Byte-at-a-time tables for the reflected CRC-64 this module uses: entry `i` of
/// `[0]` is the CRC of the single byte `i`, and the rest are derived so a whole
/// 64-bit word can be consumed per step (slicing by eight).
static CRC64_TABLES: [[u64; 256]; 8] = build_crc64_tables();

const fn build_crc64_tables() -> [[u64; 256]; 8] {
    let mut tables = [[0u64; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u64;
        let mut bit = 0;
        while bit < 8 {
            let mask = 0u64.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (CRC64_XZ_POLY & mask);
            bit += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut slice = 1;
    while slice < 8 {
        let mut i = 0;
        while i < 256 {
            let previous = tables[slice - 1][i];
            tables[slice][i] = (previous >> 8) ^ tables[0][(previous & 0xff) as usize];
            i += 1;
        }
        slice += 1;
    }
    tables
}

fn crc64_update(data: &[u8], initial: u64) -> u64 {
    // A word at a time through the eight tables: this was a bitwise loop (eight
    // shift-xor rounds per byte) and ran at 217 MB/s, which made the CRC pass over
    // a 13.9 MB prefix cost 64 ms — more than the parity computation it feeds.
    let mut crc = initial;
    let (words, tail) = data.as_chunks::<8>();
    for word in words {
        let low = crc ^ u64::from_le_bytes(*word);
        crc = CRC64_TABLES[7][(low & 0xff) as usize]
            ^ CRC64_TABLES[6][((low >> 8) & 0xff) as usize]
            ^ CRC64_TABLES[5][((low >> 16) & 0xff) as usize]
            ^ CRC64_TABLES[4][((low >> 24) & 0xff) as usize]
            ^ CRC64_TABLES[3][((low >> 32) & 0xff) as usize]
            ^ CRC64_TABLES[2][((low >> 40) & 0xff) as usize]
            ^ CRC64_TABLES[1][((low >> 48) & 0xff) as usize]
            ^ CRC64_TABLES[0][((low >> 56) & 0xff) as usize];
    }
    for &byte in tail {
        crc = (crc >> 8) ^ CRC64_TABLES[0][((crc ^ byte as u64) & 0xff) as usize];
    }
    crc
}

/// CRC-64 with the initial value RAR's own recovery code uses (no final XOR),
/// for the fields RAR computes that way rather than as CRC-64/XZ.
pub fn crc64_rar_state(data: &[u8]) -> u64 {
    crc64_update(data, 0)
}

/// Byte range of block `block` inside a data shard occupying `shard_range` of
/// the protected prefix.
///
/// WinRAR records one CRC64 per data shard *per 64 KiB block*: every `{RB}`
/// chunk of a recovery shard carries the state table of its own block only, so
/// a shard's expected state is a vector indexed by block, not a single value.
/// Block slicing starts at the shard's start, and a shard shorter than
/// `block * 64 KiB` yields an empty range for that block (the encoder records
/// the CRC64 of nothing, i.e. zero, for it).
pub fn shard_block_range(
    shard_range: std::ops::Range<usize>,
    block: u64,
) -> std::ops::Range<usize> {
    let block_len = usize::try_from(RAR5_RECOVERY_BLOCK_PAYLOAD_SIZE).unwrap_or(usize::MAX);
    let start = usize::try_from(block.saturating_mul(RAR5_RECOVERY_BLOCK_PAYLOAD_SIZE))
        .unwrap_or(usize::MAX)
        .min(shard_range.len());
    let end = start.saturating_add(block_len).min(shard_range.len());
    (shard_range.start + start)..(shard_range.start + end)
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
        // A shard that starts past the protected prefix is entirely absent;
        // clamping the start keeps every range non-reversed (`start <= end`)
        // so callers can slice the prefix without an out-of-order panic.
        let start = shard_index
            .checked_mul(group_count)
            .ok_or(Error::PlanOverflow)?
            .min(prefix_len);
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
