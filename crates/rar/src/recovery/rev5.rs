//! RAR 5.0 recovery volumes (`.rev` files).
//!
//! A `.rev` file stores Reed-Solomon parity for a set of multi-volume
//! archives, using the same 16-bit GF(2^16) Cauchy codec as the inline
//! recovery record (WinRAR's `-rv` switch). Each `.rev` file protects the
//! whole volume set; up to `NR` missing/corrupt volumes can be rebuilt.

use super::rar5::encode_parity_shards;
use crate::error::{RarError, RarResult};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

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
    let nr = (pct * nd).div_ceil(100).max(1).min(nd);
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
    Ok((
        rec_count,
        encode_recovery_volumes_exact(volume_data, rec_count)?,
    ))
}

/// Encode exactly `rec_count` recovery-volume parity payloads (caller is
/// responsible for capping `rec_count` at the data volume count).
pub fn encode_recovery_volumes_exact(
    volume_data: &[&[u8]],
    rec_count: usize,
) -> RarResult<Vec<Vec<u8>>> {
    if volume_data.is_empty() {
        return Err(RarError::Format(
            "no data volumes for recovery volumes".into(),
        ));
    }
    if volume_data.len() > 65535 {
        return Err(RarError::Format(format!(
            "too many data volumes ({}) for recovery volumes; maximum is 65535",
            volume_data.len()
        )));
    }
    let rec_count = rec_count.min(volume_data.len()).max(1);
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
    Ok(parity)
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

/// Rebuild missing volumes of a multi-volume set from its `.rev` recovery
/// volumes (like `rar rc`).
///
/// `first_volume` is any path of the volume set (e.g. `archive.part1.rar`);
/// the surviving volumes and the `.rev` files are discovered next to it.
/// Returns the paths of the rebuilt volumes.
pub fn rebuild_missing_volumes(first_volume: &Path) -> RarResult<Vec<PathBuf>> {
    use crate::recovery::rar5::reconstruct_data_shards;

    let base = crate::archive::volume_base_of(first_volume);
    let parent = first_volume
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();

    // Parse the first `.rev` file for the set parameters.
    let rev1 = parent.join(format!("{base}.part1.rev"));
    if !rev1.exists() {
        return Err(RarError::Format(format!(
            "{}: no recovery volumes found",
            first_volume.display()
        )));
    }
    let rev_data = std::fs::read(&rev1)?;
    if rev_data.len() < 8 + 4 + 4 + 1 + 2 + 2 + 2 + 4 || &rev_data[..8] != REV5_SIGNATURE {
        return Err(RarError::Format(format!(
            "{}: not a RAR5 recovery volume",
            rev1.display()
        )));
    }
    let mut off = 8 + 4 + 4;
    if rev_data[off] != 1 {
        return Err(RarError::Format(
            "unsupported recovery volume version".into(),
        ));
    }
    off += 1;
    let data_count = u16::from_le_bytes(rev_data[off..off + 2].try_into().unwrap()) as usize;
    off += 2;
    let rec_count = u16::from_le_bytes(rev_data[off..off + 2].try_into().unwrap()) as usize;
    off += 2;
    off += 2; // rev number
    let payload_crc = u32::from_le_bytes(rev_data[off..off + 4].try_into().unwrap());
    off += 4;
    if data_count == 0 || data_count > 65535 || rec_count == 0 || rec_count > data_count {
        return Err(RarError::Format(
            "implausible recovery volume parameters".into(),
        ));
    }
    let mut volume_sizes = Vec::with_capacity(data_count);
    let mut volume_crcs = Vec::with_capacity(data_count);
    for _ in 0..data_count {
        if off + 12 > rev_data.len() {
            return Err(RarError::Format("truncated recovery volume header".into()));
        }
        volume_sizes.push(u64::from_le_bytes(
            rev_data[off..off + 8].try_into().unwrap(),
        ));
        volume_crcs.push(u32::from_le_bytes(
            rev_data[off + 8..off + 12].try_into().unwrap(),
        ));
        off += 12;
    }
    let header_end = off;
    let payload = &rev_data[header_end..];
    if crc32fast::hash(payload) != payload_crc {
        return Err(RarError::Format(
            "recovery volume payload CRC mismatch".into(),
        ));
    }
    let max_len = *volume_sizes.iter().max().unwrap_or(&0);
    let padded_max = if max_len % 2 == 0 {
        max_len
    } else {
        max_len + 1
    };
    if payload.len() as u64 != padded_max {
        return Err(RarError::Format(format!(
            "recovery volume payload size {} does not match the volume set ({padded_max})",
            payload.len()
        )));
    }

    // Identify the surviving and missing data volumes.
    let mut survivors: Vec<Option<PathBuf>> = Vec::with_capacity(data_count);
    let mut missing: Vec<usize> = Vec::new();
    for i in 0..data_count {
        let vol = parent.join(format!("{base}.part{}.rar", i + 1));
        if vol.exists() {
            survivors.push(Some(vol));
        } else {
            survivors.push(None);
            missing.push(i);
        }
    }
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    if missing.len() > rec_count {
        return Err(RarError::Format(format!(
            "{} volume(s) missing but only {rec_count} recovery volume(s) available",
            missing.len()
        )));
    }

    // Reconstruct chunk by chunk over the zero-padded volume streams.
    const CHUNK: u64 = 1024 * 1024;
    let mut rebuilt: Vec<Vec<u8>> = vec![Vec::new(); missing.len()];
    let mut rev_payloads: Vec<Vec<u8>> = Vec::with_capacity(rec_count);
    for k in 0..rec_count {
        let rev_path = parent.join(format!("{base}.part{}.rev", k + 1));
        let data = std::fs::read(&rev_path)?;
        if data.len() < 8 + 4 + 4 || &data[..8] != REV5_SIGNATURE {
            return Err(RarError::Format(format!(
                "{}: not a RAR5 recovery volume",
                rev_path.display()
            )));
        }
        let hsize = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
        rev_payloads.push(data[16 + hsize..].to_vec());
    }

    let mut offset = 0u64;
    while offset < padded_max {
        let want = (padded_max - offset).min(CHUNK) as usize;
        let mut data_shards: Vec<Option<Vec<u8>>> = Vec::with_capacity(data_count);
        for (i, vol) in survivors.iter().enumerate() {
            let Some(path) = vol else {
                data_shards.push(None);
                continue;
            };
            let mut buf = vec![0u8; want];
            let size = volume_sizes[i];
            if offset < size {
                let to_read = (size - offset).min(want as u64) as usize;
                let mut f = std::fs::File::open(path)?;
                f.seek(std::io::SeekFrom::Start(offset))?;
                let mut limited = f.take(to_read as u64);
                limited.read_exact(&mut buf[..to_read])?;
            }
            data_shards.push(Some(buf));
        }
        let data_refs: Vec<Option<&[u8]>> = data_shards.iter().map(|s| s.as_deref()).collect();
        let mut recovery_shards: Vec<(usize, &[u8])> = Vec::with_capacity(rec_count);
        for (k, payload) in rev_payloads.iter().enumerate() {
            let start = offset as usize;
            recovery_shards.push((k, &payload[start..start + want]));
        }
        let all = reconstruct_data_shards(&data_refs, &recovery_shards)
            .map_err(|e| RarError::Format(format!("recovery volume reconstruction: {e}")))?;
        for (j, i) in missing.iter().enumerate() {
            rebuilt[j].extend_from_slice(&all[*i]);
        }
        offset += want as u64;
    }

    // Write the rebuilt volumes, truncated to their recorded size and
    // validated against their recorded CRC32.
    let mut rebuilt_paths = Vec::with_capacity(missing.len());
    for (j, i) in missing.iter().enumerate() {
        let size = volume_sizes[*i] as usize;
        if rebuilt[j].len() < size {
            return Err(RarError::Format(format!(
                "reconstructed volume {} is shorter than expected",
                i + 1
            )));
        }
        rebuilt[j].truncate(size);
        let actual_crc = crc32fast::hash(&rebuilt[j]);
        if actual_crc != volume_crcs[*i] {
            return Err(RarError::Crc {
                expected: volume_crcs[*i],
                actual: actual_crc,
                context: format!("reconstructed volume {}", i + 1),
            });
        }
        let vol_path = parent.join(format!("{base}.part{}.rar", i + 1));
        std::fs::write(&vol_path, &rebuilt[j])?;
        rebuilt_paths.push(vol_path);
    }
    Ok(rebuilt_paths)
}
