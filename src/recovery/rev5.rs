//! RAR 5.0 recovery volumes (`.rev` files).
//!
//! A `.rev` file stores Reed-Solomon parity for a set of multi-volume
//! archives, using the same 16-bit GF(2^16) Cauchy codec as the inline
//! recovery record (WinRAR's `-rv` switch). Each `.rev` file protects the
//! whole volume set; up to `NR` missing/corrupt volumes can be rebuilt.

use super::rar5::encode_parity_shards;
use crate::error::{RarError, RarResult};

/// REV5 file signature, distinct from the RAR archive marker.
pub const REV5_SIGNATURE: &[u8] = b"Rar!\x1aRev";

/// Number of `.rev` files for `data_count` volumes at `rec_percent`
/// (0-100): `max(1, ceil(pct * ND / 100))`, capped at `ND`.
pub fn plan_recovery_volume_count(data_count: usize, rec_percent: u64) -> RarResult<usize> {
    if data_count == 0 {
        return Err(RarError::Format(
            "no data volumes for recovery volumes".into(),
        ));
    }
    let nd = data_count as u64;
    let pct = rec_percent.min(100);
    let nr = ((pct * nd + 99) / 100).max(1).min(nd);
    Ok(nr as usize)
}

/// Encode the parity payloads for the recovery volumes.
///
/// Returns `(rec_count, payloads)` where `payloads[k]` is the payload of
/// the k-th `.rev` file. Shards are padded with zeros to the largest
/// volume size (rounded up to an even byte count for the 16-bit codec).
pub fn encode_recovery_volumes(
    volume_data: &[&[u8]],
    rec_percent: u64,
) -> RarResult<(usize, Vec<Vec<u8>>)> {
    let rec_count = plan_recovery_volume_count(volume_data.len(), rec_percent)?;
    let maxlen = volume_data.iter().map(|d| d.len()).max().unwrap_or(0);
    let maxlen = if maxlen % 2 == 0 { maxlen } else { maxlen + 1 };
    let mut padded: Vec<Vec<u8>> = Vec::with_capacity(volume_data.len());
    for d in volume_data {
        let mut v = d.to_vec();
        v.resize(maxlen, 0);
        padded.push(v);
    }
    let refs: Vec<&[u8]> = padded.iter().map(|v| v.as_slice()).collect();
    let parity = encode_parity_shards(&refs, rec_count)
        .map_err(|e| RarError::Format(format!("recovery volumes encode: {e}")))?;
    Ok((rec_count, parity))
}

/// Serialize one `.rev` file: signature, header (with the per-volume
/// metadata table) and the parity payload.
pub fn build_recovery_volume_file(
    rec_index: usize,
    rec_count: usize,
    volume_sizes: &[u64],
    volume_crcs: &[u32],
    payload: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(1u8); // version
    body.extend((volume_sizes.len() as u16).to_le_bytes()); // data count
    body.extend((rec_count as u16).to_le_bytes());
    body.extend(((volume_sizes.len() + rec_index) as u16).to_le_bytes()); // rev number
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(payload);
    body.extend(hasher.finalize().to_le_bytes()); // payload CRC32
    for (&size, &crc) in volume_sizes.iter().zip(volume_crcs) {
        body.extend(size.to_le_bytes());
        body.extend(crc.to_le_bytes());
    }

    let header_size = body.len() as u32;
    let mut header_content = Vec::with_capacity(4 + body.len());
    header_content.extend(header_size.to_le_bytes());
    header_content.extend(&body);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&header_content);
    let header_crc = hasher.finalize();

    let mut out = Vec::with_capacity(8 + 4 + header_content.len() + payload.len());
    out.extend(REV5_SIGNATURE);
    out.extend(header_crc.to_le_bytes());
    out.extend(header_content);
    out.extend(payload);
    out
}
