//! RAR 5.0 recovery volumes (`.rev` files).
//!
//! A `.rev` file stores Reed-Solomon parity for a set of multi-volume
//! archives, using the same 16-bit GF(2^16) Cauchy codec as the inline
//! recovery record (WinRAR's `-rv` switch). Each `.rev` file protects the
//! whole volume set; up to `NR` missing/corrupt volumes can be rebuilt.

use super::rar50::encode_parity_shards;
use crate::error::{RarError, RarResult};
use crate::fs::atomic::read_up_to;
use std::fs;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};

/// REV5 file signature, distinct from the RAR archive marker.
pub const REV5_SIGNATURE: &[u8] = b"Rar!\x1aRev";

/// Refuse non-RAR5 volume sets: legacy RAR 1.5–4.x sets are dispatched to
/// [`crate::recovery::rev3`] before this check runs, so anything still
/// reaching it (e.g. a RAR 1.3/1.4 `RE~^` set) is unsupported here.
fn ensure_rar5_volume_set(first_volume: &Path) -> RarResult<()> {
    let mut head = [0u8; 8];
    let mut file = fs::File::open(first_volume)?;
    let read = file.read(&mut head)?;
    if read >= 7 && head[..7] == *crate::detect::RAR4_SIGNATURE {
        return Err(RarError::Format(
            "legacy recovery volumes are handled by the RAR 1.5-4.x codec".into(),
        ));
    }
    if read >= 4 && head[..4] == *crate::detect::RAR13_SIGNATURE {
        return Err(RarError::Unsupported(
            "recovery volumes are not supported for RAR 1.3/1.4 archives".into(),
        ));
    }
    Ok(())
}

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

/// Verify the header CRC32 of a `.rev` file over the exact writer coverage:
/// [`build_recovery_volume_file`] hashes `header_content`, i.e. the 4-byte
/// header-size field plus the header body (file bytes `12..16 + hsize`),
/// and stores the CRC right after the signature (bytes `8..12`).
fn verify_rev5_header(data: &[u8], path: &Path) -> RarResult<()> {
    if data.len() < 16 {
        return Err(RarError::Format(format!(
            "{}: truncated recovery volume header",
            path.display()
        )));
    }
    let stored = u32::from_le_bytes(data[8..12].try_into().unwrap());
    let hsize = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let Some(content) = 16usize.checked_add(hsize).and_then(|end| data.get(12..end)) else {
        return Err(RarError::Format(format!(
            "{}: truncated recovery volume header",
            path.display()
        )));
    };
    if crc32fast::hash(content) != stored {
        return Err(RarError::Format(format!(
            "{}: recovery volume header CRC mismatch",
            path.display()
        )));
    }
    Ok(())
}

/// [`rebuild_missing_volumes`] with a cancellation flag and progress
/// reporting. `progress` receives `(rebuilt_bytes, total_bytes)`, strictly
/// non-decreasing up to `total` on success; `cancel` is polled per chunk.
pub fn rebuild_missing_volumes_with(
    first_volume: &Path,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
) -> RarResult<Vec<PathBuf>> {
    use crate::recovery::rar50::reconstruct_data_shards;

    // Legacy RAR 1.5–4.x sets use their own recovery-volume codec.
    if crate::recovery::rev3::is_legacy_rev_set(first_volume)? {
        return crate::recovery::rev3::rebuild_missing_volumes(first_volume, cancel, progress);
    }

    ensure_rar5_volume_set(first_volume)?;

    let check_cancel = |cancel: Option<&std::sync::atomic::AtomicBool>| -> RarResult<()> {
        if cancel.is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed)) {
            return Err(RarError::Cancelled);
        }
        Ok(())
    };

    let base = crate::archive::volume_base_of(first_volume);
    let parent = first_volume
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();

    // Parse the first `.rev` file for the set parameters. WinRAR (and
    // `build_recovery_volumes_for_set`) pad the part number to the digit
    // count of the volume count (part01.rev .. part15.rev), so probe the
    // padding width from 1 to 4 digits.
    let mut rev1: Option<PathBuf> = None;
    let mut width = 1usize;
    for w in 1..=4 {
        let probe = parent.join(format!("{base}.part{:0w$}.rev", 1, w = w));
        if probe.exists() {
            rev1 = Some(probe);
            width = w;
            break;
        }
    }
    let Some(rev1) = rev1 else {
        return Err(RarError::Format(format!(
            "{}: no recovery volumes found",
            first_volume.display()
        )));
    };
    let rev_data = std::fs::read(&rev1)?;
    if rev_data.len() < 8 + 4 + 4 + 1 + 2 + 2 + 2 + 4 || &rev_data[..8] != REV5_SIGNATURE {
        return Err(RarError::Format(format!(
            "{}: not a RAR5 recovery volume",
            rev1.display()
        )));
    }
    verify_rev5_header(&rev_data, &rev1)?;
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
    if data_count == 0 || data_count > 65535 || rec_count == 0 || rec_count > 65535 - data_count {
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
        let vol = parent.join(format!("{base}.part{:0width$}.rar", i + 1, width = width));
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
        let rev_path = parent.join(format!("{base}.part{:0width$}.rev", k + 1, width = width));
        let data = std::fs::read(&rev_path)?;
        if data.len() < 8 + 4 + 4 || &data[..8] != REV5_SIGNATURE {
            return Err(RarError::Format(format!(
                "{}: not a RAR5 recovery volume",
                rev_path.display()
            )));
        }
        verify_rev5_header(&data, &rev_path)?;
        let hsize = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
        // A truncated `.rev` (interrupted copy, partial download) must be an
        // error, not a slice past the buffer.
        let Some(payload) = 16usize
            .checked_add(hsize)
            .and_then(|start| data.get(start..))
        else {
            return Err(RarError::Format(format!(
                "{}: truncated recovery volume",
                rev_path.display()
            )));
        };
        rev_payloads.push(payload.to_vec());
    }

    let mut offset = 0u64;
    while offset < padded_max {
        check_cancel(cancel)?;
        if let Some(p) = progress.as_deref_mut() {
            p(offset, padded_max);
        }
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
            let Some(shard) = payload.get(start..start + want) else {
                return Err(RarError::Format(format!(
                    "recovery volume {} is shorter than the volume set it protects",
                    k + 1
                )));
            };
            recovery_shards.push((k, shard));
        }
        let all = reconstruct_data_shards(&data_refs, &recovery_shards)
            .map_err(|e| RarError::Format(format!("recovery volume reconstruction: {e}")))?;
        for (j, i) in missing.iter().enumerate() {
            rebuilt[j].extend_from_slice(&all[*i]);
        }
        offset += want as u64;
        if let Some(p) = progress.as_deref_mut() {
            p(offset.min(padded_max), padded_max);
        }
    }

    // Write the rebuilt volumes, truncated to their recorded size and
    // validated against their recorded CRC32.
    let mut rebuilt_paths = Vec::with_capacity(missing.len());
    for (j, i) in missing.iter().enumerate() {
        check_cancel(cancel)?;
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
        let vol_path = parent.join(format!("{base}.part{:0width$}.rar", i + 1, width = width));
        std::fs::write(&vol_path, &rebuilt[j])?;
        rebuilt_paths.push(vol_path);
    }
    Ok(rebuilt_paths)
}

/// Build `.rev` recovery volumes for an existing multi-volume set,
/// streaming all volumes in lockstep chunks (memory stays bounded at
/// O(chunk × volume count)).
///
/// `rec_count` is the exact number of `.rev` files to produce; it is
/// clamped to `10 × volume count` (WinRAR's `rv[N]` cap) and to the
/// 65535 total-volume limit of the format. The `.rev` files are named
/// after the set with the same zero-padding as the volumes
/// (`<base>.partNN.rev`), matching WinRAR. Returns the written paths.
pub fn build_recovery_volumes_for_set(
    volume_paths: &[PathBuf],
    rec_count: usize,
) -> RarResult<Vec<PathBuf>> {
    let nd = volume_paths.len();
    if nd == 0 {
        return Err(RarError::Format("no volumes for recovery volumes".into()));
    }
    // Legacy RAR 1.5–4.x sets use their own recovery-volume codec.
    if crate::recovery::rev3::is_legacy_rev_set(&volume_paths[0])? {
        return crate::recovery::rev3::build_recovery_volumes_for_set(volume_paths, rec_count);
    }
    ensure_rar5_volume_set(&volume_paths[0])?;
    if nd > 65535 {
        return Err(RarError::Format(format!(
            "too many volumes ({nd}) for recovery volumes; maximum is 65535"
        )));
    }
    let rec_count = rec_count.min(nd * 10).max(1);
    if nd + rec_count > 65535 {
        return Err(RarError::Format(format!(
            "data ({nd}) + recovery ({rec_count}) volumes exceed the 65535 limit"
        )));
    }

    // Stream all volumes in lockstep chunks: per-chunk Reed-Solomon
    // parity keeps memory bounded at O(chunk x volumes) and CRCs are
    // computed in the same pass.
    const CHUNK: u64 = 1024 * 1024;
    let mut volume_sizes = Vec::with_capacity(nd);
    let mut readers = Vec::with_capacity(nd);
    let mut crcs = Vec::with_capacity(nd);
    for vol in volume_paths {
        let size = fs::metadata(vol)?.len();
        volume_sizes.push(size);
        readers.push(fs::File::open(vol)?);
        crcs.push(crc32fast::Hasher::new());
    }
    let max_len = *volume_sizes.iter().max().unwrap_or(&0);
    let padded_max = if max_len % 2 == 0 {
        max_len
    } else {
        max_len + 1
    };

    let mut payloads: Vec<Vec<u8>> = vec![Vec::new(); rec_count];
    let mut offset = 0u64;
    while offset < padded_max {
        let want = (padded_max - offset).min(CHUNK) as usize;
        let mut chunk_bufs: Vec<Vec<u8>> = Vec::with_capacity(nd);
        for (i, reader) in readers.iter_mut().enumerate() {
            let mut buf = vec![0u8; want];
            if offset < volume_sizes[i] {
                let to_read = (volume_sizes[i] - offset).min(want as u64) as usize;
                let n = read_up_to(reader, &mut buf[..to_read])?;
                if n != to_read {
                    return Err(RarError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "volume {} shrank while building recovery volumes",
                            volume_paths[i].display()
                        ),
                    )));
                }
                crcs[i].update(&buf[..to_read]);
                buf[to_read..].fill(0); // zero-pad to the chunk length
            }
            chunk_bufs.push(buf);
        }
        let refs: Vec<&[u8]> = chunk_bufs.iter().map(|b| b.as_slice()).collect();
        let parity = encode_parity_shards(&refs, rec_count)
            .map_err(|e| RarError::Format(format!("recovery volumes encode: {e}")))?;
        for (k, p) in parity.into_iter().enumerate() {
            payloads[k].extend(p);
        }
        offset += want as u64;
    }
    let volume_crcs: Vec<u32> = crcs.into_iter().map(|h| h.finalize()).collect();

    let base = crate::archive::volume_base_of(&volume_paths[0]);
    let parent = volume_paths[0].parent().unwrap_or(Path::new("."));
    // `.rev` names must carry the same padding as the volume set (which
    // comes from the file names, not the discovered count: a set with a
    // missing middle volume is discovered as a prefix but keeps its
    // original padding).
    let pad = crate::archive::volume_part_width(&volume_paths[0]).max(1);
    let mut written = Vec::with_capacity(rec_count);
    for (k, payload) in payloads.iter().enumerate() {
        let rev_path = parent.join(format!("{base}.part{:0pad$}.rev", k + 1, pad = pad));
        let file = build_recovery_volume_file(k, rec_count, &volume_sizes, &volume_crcs, payload);
        fs::write(&rev_path, &file)?;
        written.push(rev_path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_crc_is_verified_but_does_not_cover_payload() {
        let payload = vec![0x5au8; 512];
        let file = build_recovery_volume_file(0, 1, &[1024], &[0xdead_beef], &payload);
        let path = Path::new("set.part1.rev");

        // Positive control: the freshly built header verifies.
        verify_rev5_header(&file, path).unwrap();

        // A flipped header content byte (here the version field) is caught.
        let mut corrupted = file.clone();
        corrupted[16] ^= 0x40;
        let err = verify_rev5_header(&corrupted, path).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "got {err}");

        // A flipped stored CRC is caught too.
        let mut corrupted = file.clone();
        corrupted[8] ^= 0x01;
        assert!(matches!(
            verify_rev5_header(&corrupted, path).unwrap_err(),
            RarError::Format(_)
        ));

        // The parity payload lies outside the verified header.
        let mut payload_flip = file;
        let last = payload_flip.len() - 1;
        payload_flip[last] ^= 0xff;
        verify_rev5_header(&payload_flip, path).unwrap();
    }

    #[test]
    fn truncated_header_is_rejected() {
        let file = build_recovery_volume_file(0, 1, &[1024], &[0xdead_beef], &[0u8; 64]);
        let err = verify_rev5_header(&file[..20], Path::new("x.rev")).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "got {err}");
    }
}
