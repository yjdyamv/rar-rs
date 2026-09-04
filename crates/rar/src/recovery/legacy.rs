//! Legacy (RAR 1.5–4.x) recovery-record repair — the RAR 3-style
//! `PROTECT_HEAD` (0x78) inline recovery record.
//!
//! The recovery record covers the archive prefix in 512-byte sectors. Each
//! sector's CRC16 (the low 16 bits of the ones-complemented CRC32) is stored
//! in a tag table, and `rec_sectors` XOR parity sectors follow it. Repair =
//! compare stored tags against recomputed ones, then XOR the surviving
//! sectors of each parity group back into the parity to rebuild the damaged
//! sector. Ported from the rars recovery path (`repair_protect_head_bytes`),
//! which is validated against genuine RAR 2.5/3.x archives.
//!
//! Only archives whose creator added `-rr` are repairable — without the
//! redundant parity there is nothing to rebuild from. The RAR 4.x-era
//! NEWSUB (0x7A) recovery subblock and legacy `.rev` recovery volumes are
//! separate follow-ups.

use crate::crc32;
use crate::detect::RAR4_SIGNATURE;
use crate::error::{RarError, RarResult};

/// `PROTECT_HEAD` parsed from a legacy archive (file-absolute positions).
#[derive(Debug, Clone)]
pub(crate) struct Rar4Protect {
    /// Number of 512-byte parity sectors.
    pub rec_sectors: u16,
    /// Number of 512-byte sectors the record declares as protected.
    pub total_blocks: u32,
    /// Must be the 8 bytes `Protect!`.
    pub mark: [u8; 8],
    /// File-absolute offset of the recovery block's data area (tag table +
    /// parity sectors).
    pub data_start: usize,
    /// File-absolute end of the recovery block's data area.
    pub data_end: usize,
    /// File-absolute offset where the recovery block itself starts.
    pub block_offset: usize,
}

/// Whether a byte stream carries a legacy RAR4 `PROTECT_HEAD` recovery
/// record, and where the archive (signature) starts.
pub(crate) struct Rar4ProtectScan {
    pub sfx_offset: usize,
    pub protect: Option<Rar4Protect>,
}

/// Find the RAR4 signature inside `bytes` (SFX stubs allowed, bounded like
/// the reader's own scan) and walk the blocks looking for a PROTECT_HEAD.
pub(crate) fn scan_protect(bytes: &[u8]) -> RarResult<Rar4ProtectScan> {
    const MARK_HEAD: u8 = 0x72;
    const PROTECT_HEAD: u8 = 0x78;
    const ENDARC_HEAD: u8 = 0x7b;

    let sig = find_bytes(bytes, RAR4_SIGNATURE, 8 * 1024 * 1024)
        .ok_or_else(|| RarError::Format("not a RAR4 archive (signature not found)".into()))?;
    let mut pos = sig + RAR4_SIGNATURE.len();
    let mut protect = None;
    while pos + 7 <= bytes.len() {
        let start = pos;
        let head_type = bytes[pos + 2];
        let flags = u16::from_le_bytes([bytes[pos + 3], bytes[pos + 4]]);
        let head_size = u16::from_le_bytes([bytes[pos + 5], bytes[pos + 6]]) as usize;
        if head_size < 7 {
            return Err(RarError::Format("RAR4: block head_size too small".into()));
        }
        let add_size = if flags & 0x8000 != 0 {
            if pos + 11 > bytes.len() {
                return Err(RarError::Format("RAR4: truncated block".into()));
            }
            u32::from_le_bytes(bytes[pos + 7..pos + 11].try_into().unwrap()) as usize
        } else {
            0
        };
        let total = head_size + add_size;
        if pos + total > bytes.len() {
            return Err(RarError::Format("RAR4: truncated block".into()));
        }

        if head_type == PROTECT_HEAD
            && head_size == 26
            && flags & 0x8000 != 0
            && bytes.get(start + 18..start + 26) == Some(b"Protect!")
        {
            let rec_sectors = u16::from_le_bytes(bytes[start + 12..start + 14].try_into().unwrap());
            let total_blocks =
                u32::from_le_bytes(bytes[start + 14..start + 18].try_into().unwrap());
            let mark: [u8; 8] = bytes[start + 18..start + 26].try_into().unwrap();
            let data_start = start + head_size;
            let data_end = start + total;
            if u64::from(total_blocks) * 2 + u64::from(rec_sectors) * 512
                != (data_end - data_start) as u64
            {
                return Err(RarError::Format(
                    "RAR4: recovery data size does not match header".into(),
                ));
            }
            protect = Some(Rar4Protect {
                rec_sectors,
                total_blocks,
                mark,
                data_start,
                data_end,
                block_offset: start,
            });
            break;
        }
        if head_type == ENDARC_HEAD {
            break;
        }
        let _ = head_type;
        let _ = MARK_HEAD;
        pos = start + total;
    }
    Ok(Rar4ProtectScan {
        sfx_offset: sig,
        protect,
    })
}

/// Repair a legacy archive that carries a PROTECT_HEAD recovery record.
/// `sfx_offset` is where the archive signature starts (0 for plain
/// archives); the 512-byte sector grid is anchored there. Returns
/// `Ok(None)` when every protected sector already matches its tag (nothing
/// to do), `Ok(Some(repaired))` after rebuilding, or an error when the
/// damage exceeds the parity or the record is malformed.
pub(crate) fn repair_protect_head(
    bytes: &[u8],
    sfx_offset: usize,
    protect: &Rar4Protect,
) -> RarResult<Option<Vec<u8>>> {
    if protect.rec_sectors == 0 {
        return Err(RarError::Format(
            "RAR4: recovery record has no parity sectors".into(),
        ));
    }
    if &protect.mark != b"Protect!" {
        return Err(RarError::Format("RAR4: recovery mark is invalid".into()));
    }
    let protected_len = (protect.total_blocks as usize)
        .checked_mul(512)
        .ok_or_else(|| RarError::Format("RAR4: protected range overflows".into()))?;
    let protected_end = sfx_offset
        .checked_add(protected_len)
        .ok_or_else(|| RarError::Format("RAR4: protected range overflows".into()))?;
    if protected_end > bytes.len() || protect.data_end > bytes.len() {
        return Err(RarError::Format("RAR4: protected range is invalid".into()));
    }
    let recovery = &bytes[protect.data_start..protect.data_end];
    let tag_len = (protect.total_blocks as usize)
        .checked_mul(2)
        .ok_or_else(|| RarError::Format("RAR4: recovery tag size overflows".into()))?;
    let parity_len = (protect.rec_sectors as usize)
        .checked_mul(512)
        .ok_or_else(|| RarError::Format("RAR4: recovery parity size overflows".into()))?;
    if recovery.len() != tag_len + parity_len {
        return Err(RarError::Format(
            "RAR4: recovery data size is invalid".into(),
        ));
    }
    let tags = &recovery[..tag_len];
    let parity = &recovery[tag_len..];
    // RAR 2.50 records may declare a final sector that starts before the
    // PROTECT_HEAD but overlaps the recovery block; only complete sectors
    // before it are safely repairable.
    let repairable_blocks =
        (protect.total_blocks as usize).min((protect.block_offset - sfx_offset) / 512);

    let mut damaged = Vec::new();
    for index in 0..repairable_blocks {
        let start = sfx_offset + index * 512;
        let sector = &bytes[start..start + 512];
        let actual = (!crc32::crc32(sector) & 0xffff) as u16;
        let expected = u16::from_le_bytes(tags[index * 2..index * 2 + 2].try_into().unwrap());
        if actual != expected {
            damaged.push(index);
        }
    }
    if damaged.is_empty() {
        return Ok(None);
    }
    if damaged.len() > protect.rec_sectors as usize {
        return Err(RarError::Format(format!(
            "RAR4: recovery damage ({} sectors) exceeds parity sector count {}",
            damaged.len(),
            protect.rec_sectors
        )));
    }
    let mut used_slots = vec![false; protect.rec_sectors as usize];
    for &index in &damaged {
        let slot = index % protect.rec_sectors as usize;
        if used_slots[slot] {
            return Err(RarError::Format(
                "RAR4: recovery cannot repair multiple sectors in the same parity group".into(),
            ));
        }
        used_slots[slot] = true;
    }

    let mut repaired = bytes.to_vec();
    for &missing_index in &damaged {
        let slot = missing_index % protect.rec_sectors as usize;
        let mut sector = parity[slot * 512..slot * 512 + 512].to_vec();
        for index in (slot..repairable_blocks).step_by(protect.rec_sectors as usize) {
            if index == missing_index {
                continue;
            }
            let start = sfx_offset + index * 512;
            for (out, byte) in sector.iter_mut().zip(&repaired[start..start + 512]) {
                *out ^= *byte;
            }
        }
        let start = sfx_offset + missing_index * 512;
        repaired[start..start + 512].copy_from_slice(&sector);
        let actual = (!crc32::crc32(&sector) & 0xffff) as u16;
        let expected = u16::from_le_bytes(
            tags[missing_index * 2..missing_index * 2 + 2]
                .try_into()
                .unwrap(),
        );
        if actual != expected {
            return Err(RarError::Crc {
                expected: expected as u32,
                actual: actual as u32,
                context: "RAR4 recovery rebuilt sector".into(),
            });
        }
    }
    Ok(Some(repaired))
}

/// Repair the legacy archive at `src` into `dst` when it carries a
/// PROTECT_HEAD recovery record. Returns `Ok(true)` when something was
/// rebuilt, `Ok(false)` when the archive was already intact (nothing
/// written), and an error when it has no usable recovery record.
pub fn repair_legacy_archive_path(src: &std::path::Path, dst: &std::path::Path) -> RarResult<bool> {
    let bytes = std::fs::read(src).map_err(RarError::Io)?;
    let scan = scan_protect(&bytes)?;
    let Some(protect) = scan.protect else {
        return Err(RarError::Unsupported(
            "archive has no legacy PROTECT_HEAD recovery record".into(),
        ));
    };
    let repaired = repair_protect_head(&bytes, scan.sfx_offset, &protect)?;
    let Some(repaired) = repaired else {
        return Ok(false);
    };
    // Keep the write atomic: stage next to the destination, then rename.
    let tmp = crate::io_util::temp_sibling_path(dst);
    std::fs::write(&tmp, &repaired).map_err(RarError::Io)?;
    std::fs::rename(&tmp, dst).map_err(RarError::Io)?;
    Ok(true)
}

fn find_bytes(haystack: &[u8], needle: &[u8], limit: usize) -> Option<usize> {
    let window = &haystack[..haystack.len().min(limit)];
    window.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protect_scan_finds_and_repairs_damage() {
        // A genuine RAR 2.5 archive with a 5% PROTECT_HEAD recovery record.
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/rar40/repair/rar250_protect_head_rr5.rar"
        );
        let original = std::fs::read(fixture).expect("read fixture");
        let scan = scan_protect(&original).expect("scan");
        let protect = scan.protect.expect("protect record present");
        assert_eq!(&protect.mark, b"Protect!");

        // Damage 64 bytes inside sector 1 (same spot the reference test uses).
        let mut damaged = original.clone();
        let damage_offset = 512 + 16;
        damaged[damage_offset..damage_offset + 64].fill(0xa5);

        let rebuilt = repair_protect_head(&damaged, 0, &protect)
            .expect("repair")
            .expect("damage found");
        assert_eq!(rebuilt, original, "repair restores the original bytes");

        // An intact archive reports nothing to do.
        assert!(
            repair_protect_head(&original, 0, &protect)
                .expect("intact scan")
                .is_none()
        );
    }
}
