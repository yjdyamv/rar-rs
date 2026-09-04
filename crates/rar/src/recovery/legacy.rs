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
//! Two on-disk block shapes carry the same data layout:
//!
//! - `PROTECT_HEAD` (0x78), the RAR 2.5-era form: a 26-byte header whose
//!   `[12..14]` rec_sectors (u16), `[14..18]` total_blocks (u32) and
//!   `[18..26]` = `Protect!` mark sit in the fixed header.
//! - `NEWSUB` (0x7a) named `RR`, the RAR 3.x/4.x form WinRAR 6.23 writes:
//!   a FILE_HEAD-shaped header (32 fixed + name), then after the `RR` name a
//!   20-byte tail = `Protect+` + rec_sectors (u32) + total_blocks (u32) +
//!   zero. The data area (tags + parity) follows the header either way.
//!
//! The write side (`build_legacy_recovery_block`) emits the NEWSUB form, the
//! one WinRAR's RAR4 writer produces, so its own repair path handles it.
//!
//! Only archives whose creator added `-rr` are repairable — without the
//! redundant parity there is nothing to rebuild from. The legacy `.rev`
//! recovery volumes are a separate follow-up.

use crate::crc32;
use crate::detect::RAR4_SIGNATURE;
use crate::error::{RarError, RarResult};

/// `PROTECT_HEAD` / `RR` NEWSUB parsed from a legacy archive
/// (file-absolute positions).
#[derive(Debug, Clone)]
pub(crate) struct Rar4Protect {
    /// Number of 512-byte parity sectors.
    pub rec_sectors: u32,
    /// Number of 512-byte sectors the record declares as protected.
    pub total_blocks: u32,
    /// Must be the 8 bytes `Protect!` (0x78) or `Protect+` (0x7a).
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
/// the reader's own scan) and walk the blocks looking for a recovery record
/// (either the PROTECT_HEAD 0x78 or the NEWSUB 0x7a `RR` form).
pub(crate) fn scan_protect(bytes: &[u8]) -> RarResult<Rar4ProtectScan> {
    const MARK_HEAD: u8 = 0x72;
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

        // RAR 2.5-era PROTECT_HEAD (0x78): 26-byte fixed header with the
        // `Protect!` mark in the last eight bytes.
        if head_type == 0x78
            && head_size == 26
            && flags & 0x8000 != 0
            && bytes.get(start + 18..start + 26) == Some(b"Protect!")
        {
            let rec_sectors = u16::from_le_bytes(bytes[start + 12..start + 14].try_into().unwrap());
            let total_blocks =
                u32::from_le_bytes(bytes[start + 14..start + 18].try_into().unwrap());
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
                rec_sectors: u32::from(rec_sectors),
                total_blocks,
                mark: [0x50, 0x72, 0x6f, 0x74, 0x65, 0x63, 0x74, 0x21], // "Protect!"
                data_start,
                data_end,
                block_offset: start,
            });
            break;
        }

        // RAR 3.x/4.x NEWSUB (0x7a) named `RR`: FILE_HEAD-shaped header
        // whose 20-byte tail after the name is `Protect+` + rec_sectors
        // (u32) + total_blocks (u32) + zero.
        if head_type == 0x7a
            && flags & 0x8000 != 0
            && head_size >= 32 + 2 + 20
            && bytes.get(start + 7 + 25..start + 7 + 25 + 2) == Some(b"RR")
        {
            let body = &bytes[start + 7..start + head_size];
            let name_size = u16::from_le_bytes(body[19..21].try_into().unwrap()) as usize;
            let tail = start + 7 + 25 + name_size;
            if bytes.get(tail..tail + 8) == Some(b"Protect+") {
                let rec_sectors =
                    u32::from_le_bytes(bytes[tail + 8..tail + 12].try_into().unwrap());
                let total_blocks =
                    u32::from_le_bytes(bytes[tail + 12..tail + 16].try_into().unwrap());
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
                    mark: [0x50, 0x72, 0x6f, 0x74, 0x65, 0x63, 0x74, 0x2b], // "Protect+"
                    data_start,
                    data_end,
                    block_offset: start,
                });
                break;
            }
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
    if &protect.mark != b"Protect!" && &protect.mark != b"Protect+" {
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

// ── Write side ──────────────────────────────────────────────────────────────

/// Sector CRC16 for one 512-byte protected sector: the low 16 bits of the
/// ones-complemented CRC32 (matches both the 0x78 read path and WinRAR's
/// RAR4 NEWSUB tags, verified against 6.23 output).
pub(crate) fn sector_tag(sector: &[u8]) -> u16 {
    (!crate::crc32::crc32(sector) & 0xffff) as u16
}

/// Number of 512-byte sectors a legacy `-rrN%` record declares over a
/// `prefix` of `prefix_len` bytes, mirroring WinRAR 6.23: `N%` of the
/// protected bytes, rounded down to whole sectors, with a floor of two
/// sectors so a tiny archive still gets a usable record. The `-rrN` (no
/// percent) count form is used verbatim by the caller instead.
pub(crate) fn recovery_sector_count(prefix_len: usize, percent: u8) -> u32 {
    let percent = u64::from(percent);
    let count = (prefix_len as u64)
        .saturating_mul(percent)
        .saturating_div(100 * 512);
    (count as u32).max(2)
}

/// Build a complete RAR 3.x/4.x NEWSUB (0x7a) recovery block — the shape
/// WinRAR 6.23's RAR4 writer produces for `-rr`: a FILE_HEAD-shaped header
/// named `RR` with a `Protect+` tail, followed by the tag table (one
/// sector CRC16 per protected 512-byte sector) and `rec_sectors` XOR
/// parity sectors. `prefix` is the archive bytes the record protects
/// (everything written before this block); the 512-byte sector grid is
/// anchored at `prefix[0]`, and `total_blocks` covers the whole prefix
/// (the final partial sector is still CRC-protected and parity-covered).
///
/// The caller appends this block between the last member and the
/// end-of-archive block.
pub(crate) fn build_legacy_recovery_block(prefix: &[u8], rec_sectors: u32) -> RarResult<Vec<u8>> {
    if rec_sectors == 0 {
        return Err(RarError::Format(
            "RAR4: recovery record needs at least one parity sector".into(),
        ));
    }
    let total_blocks = prefix.len().div_ceil(512) as u32;
    if total_blocks == 0 {
        return Err(RarError::Format(
            "RAR4: recovery record over an empty archive".into(),
        ));
    }
    // Complete 512-byte sectors before this block; a partial final sector
    // (whose tail would overlap this very block) is tagged but never folded
    // into a parity group — the repair path only rebuilds complete sectors.
    let full_sectors = prefix.len() / 512;

    // Sector tags (little-endian u16 each): every declared sector, with a
    // partial tail zero-padded for its tag CRC, matching the reader.
    let mut tags = Vec::with_capacity(total_blocks as usize * 2);
    let mut parity = vec![0u8; rec_sectors as usize * 512];
    let mut sector = [0u8; 512];
    for block in 0..total_blocks as usize {
        let start = block * 512;
        let end = (start + 512).min(prefix.len());
        sector[..end - start].copy_from_slice(&prefix[start..end]);
        sector[end - start..].fill(0);
        tags.extend_from_slice(&sector_tag(&sector).to_le_bytes());
        if block < full_sectors {
            // Parity group: every complete sector whose index ≡ block
            // (mod rec_sectors) XORs into parity[block % rec_sectors].
            let slot = block % rec_sectors as usize;
            for (out, byte) in parity[slot * 512..slot * 512 + 512].iter_mut().zip(&sector) {
                *out ^= *byte;
            }
        }
    }

    let data_len = tags.len() + parity.len();
    let mut header = Vec::with_capacity(32 + 2 + 20);
    // CRC placeholder + type + flags (LONG_BLOCK) + head_size + add_size.
    header.extend_from_slice(&[0u8; 2]);
    header.push(0x7a); // NEWSUB_HEAD
    header.extend_from_slice(&(0xC000u16).to_le_bytes()); // LONG_BLOCK
    header.extend_from_slice(&(54u16).to_le_bytes()); // head_size
    header.extend_from_slice(&(data_len as u32).to_le_bytes()); // packed/add
    header.extend_from_slice(&(data_len as u32).to_le_bytes()); // unpacked
    header.push(2); // host_os: Windows
    header.extend_from_slice(&0u32.to_le_bytes()); // file_crc (unused)
    header.extend_from_slice(&0u32.to_le_bytes()); // file_time
    header.push(29); // unp_ver
    header.push(0x30); // method: store
    header.extend_from_slice(&2u16.to_le_bytes()); // name_size
    header.extend_from_slice(&0u32.to_le_bytes()); // file_attr
    header.extend_from_slice(b"RR");
    header.extend_from_slice(b"Protect+");
    header.extend_from_slice(&rec_sectors.to_le_bytes());
    header.extend_from_slice(&total_blocks.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    debug_assert_eq!(header.len(), 54);

    // Header CRC16 over bytes[2..] (the RAR4 convention).
    let crc = (crate::crc32::crc32(&header[2..]) & 0xffff) as u16;
    header[0..2].copy_from_slice(&crc.to_le_bytes());

    let mut out = header;
    out.extend_from_slice(&tags);
    out.extend_from_slice(&parity);
    Ok(out)
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

    /// Build a NEWSUB (0x7a) recovery block over a synthetic prefix and
    /// round-trip it through the same scan/repair path a legacy archive
    /// would: scan recognises the `RR` record, an intact copy reports no
    /// damage, and damage inside a protected sector is rebuilt exactly.
    #[test]
    fn newsub_rr_block_roundtrips_through_scan_and_repair() {
        use crate::rar40::RAR4_METHOD_STORE;
        use crate::rar40::write::{FileHeaderParams, build_file_header, build_main_header};

        // A minimal but structurally valid archive: signature + main header
        // + one stored member, whose payload fills most of the protected
        // range (the scan walks real RAR4 blocks, so the prefix cannot be
        // arbitrary bytes).
        let mut payload = Vec::with_capacity(200_000);
        let mut seed = 0x1234_5678u32;
        while payload.len() < 200_000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            payload.push((seed >> 24) as u8);
        }
        let fh = build_file_header(&FileHeaderParams {
            flags: 0,
            packed_size: payload.len() as u32,
            unpacked_size: payload.len() as u32,
            host_os: 0,
            file_crc: 0,
            file_time: 0,
            unp_ver: 20,
            method: RAR4_METHOD_STORE,
            name: b"d.bin",
            attr: 0x20,
            window_bits: 0,
            salt: None,
            ext_time: None,
        })
        .expect("file head");
        let mut prefix = b"Rar!\x1a\x07\x00".to_vec();
        prefix.extend_from_slice(&build_main_header(0));
        prefix.extend_from_slice(&fh);
        prefix.extend_from_slice(&payload);

        // 10% recovery, matching the WinRAR 6.23 rec formula.
        let rec = recovery_sector_count(prefix.len(), 10);
        assert_eq!(
            rec,
            (prefix.len() as u64 * 10 / 51_200) as u32,
            "rec formula"
        );
        let block = build_legacy_recovery_block(&prefix, rec).expect("build");
        let total_blocks = prefix.len().div_ceil(512) as u32;
        assert_eq!(
            block.len(),
            54 + total_blocks as usize * 2 + rec as usize * 512
        );

        // Append the RR block + ENDARC.
        let mut archive = prefix.clone();
        let rr_offset = archive.len();
        archive.extend_from_slice(&block);
        archive.extend_from_slice(&[0, 0, 0x7b, 0, 0, 7, 0]); // ENDARC

        let scan = scan_protect(&archive).expect("scan");
        let protect = scan.protect.expect("NEWSUB RR record present");
        assert_eq!(&protect.mark, b"Protect+");
        assert_eq!(protect.rec_sectors, rec);
        assert_eq!(protect.total_blocks, total_blocks);
        assert_eq!(protect.block_offset, rr_offset);
        assert_eq!(protect.data_start, rr_offset + 54);

        // Intact: nothing to repair.
        assert!(
            repair_protect_head(&archive, 0, &protect)
                .expect("intact scan")
                .is_none()
        );

        // Damage a stretch of a protected data sector (inside `payload`, far
        // from the recovery block itself) and rebuild it byte-identically.
        let mut damaged = archive.clone();
        let damage_at = 40_000;
        damaged[damage_at..damage_at + 128].fill(0x5a);
        let repaired = repair_protect_head(&damaged, 0, &protect)
            .expect("repair")
            .expect("damage found");
        assert_eq!(repaired, archive, "NEWSUB RR repair restores the prefix");

        // Two damaged sectors in the SAME parity group exceed that group's
        // capacity (one parity sector can rebuild one member), even though
        // the global count is far below the parity sector total.
        let mut hopeless = archive.clone();
        hopeless[0..32].fill(0x7e);
        hopeless[(rec as usize * 512)..(rec as usize * 512 + 32)].fill(0x7e);
        assert!(
            repair_protect_head(&hopeless, 0, &protect).is_err(),
            "two damaged sectors in one parity group must fail"
        );
    }
}
