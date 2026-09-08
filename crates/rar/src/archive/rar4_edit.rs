//! Header-level edits on existing RAR 1.5–4.x archives (ADR 0005, stage A).
//!
//! Two operations ship here: inline recovery-record add/replace (`rar rr`)
//! and lock (`rar k`). Both are pure header/block surgery — no member data
//! is decoded or recompressed, so solid and non-solid archives are handled
//! identically. The edits are refused up front for multi-volume and
//! header-encrypted (`-hp`) archives, which the later stages of the RAR4
//! edit rollout will cover.
//!
//! Lock (`k`) patches the 13-byte main header in place (mirroring the RAR5
//! lock). Recovery (`rr`) reads the whole archive into memory — the NEWSUB
//! record is XOR parity over the protected prefix, so the builder needs the
//! prefix anyway (the same bound the RAR4 creation path accepts) — and
//! stages a rewritten copy next to the archive before replacing it
//! atomically.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::RarArchive;
use crate::error::{RarError, RarResult};
use crate::format::rar4::{
    ENDARC_HEAD, LONG_BLOCK, MAIN_HEAD, MHD_LOCK, MHD_PASSWORD, MHD_RECOVERY, MHD_VOLUME,
};
use crate::fs::atomic::{read_write_create, replace_file, temp_sibling_path};
use crate::recovery::legacy_rr::{
    build_legacy_recovery_block, recovery_sector_count, scan_protect,
};

/// RAR4 header CRC16: standard CRC-32 truncated to 16 bits over
/// `bytes[2..]` (the body after the CRC field). The main header's CRC
/// coverage stops at 13 bytes when a nested comment is present, which is
/// exactly where the flags live, so patching the first 13 bytes and
/// recomputing over `[2..13]` is correct with or without a comment.
fn patch_main_header(main: &[u8], set_bits: u16) -> RarResult<Vec<u8>> {
    if main.len() < 13 || main[2] != MAIN_HEAD {
        return Err(RarError::Format(
            "RAR4: main header block is malformed".into(),
        ));
    }
    let flags = u16::from_le_bytes([main[3], main[4]]);
    let mut patched = main[..13].to_vec();
    patched[3..5].copy_from_slice(&(flags | set_bits).to_le_bytes());
    let crc = (crate::crc32::crc32(&patched[2..]) & 0xffff) as u16;
    patched[..2].copy_from_slice(&crc.to_le_bytes());
    Ok(patched)
}

/// Parse the flags out of a raw main header block.
fn main_flags(main: &[u8]) -> RarResult<u16> {
    if main.len() < 13 || main[2] != MAIN_HEAD {
        return Err(RarError::Format(
            "RAR4: main header block is malformed".into(),
        ));
    }
    Ok(u16::from_le_bytes([main[3], main[4]]))
}

/// Read the archive's main header block (which starts 7 bytes after the
/// signature, i.e. at `sfx_offset + 7`), returning its file-absolute
/// offset and raw bytes. Works for `-hp` archives too: their main header
/// is the plaintext marker.
fn read_main_from_file(path: &Path, sfx_offset: u64) -> RarResult<(u64, Vec<u8>)> {
    let mut file = File::open(path).map_err(RarError::Io)?;
    file.seek(SeekFrom::Start(sfx_offset))?;
    let mut sig = [0u8; 7];
    file.read_exact(&mut sig).map_err(RarError::Io)?;
    if &sig != crate::detect::RAR4_SIGNATURE {
        return Err(RarError::Format(
            "RAR4: signature mismatch while editing".into(),
        ));
    }
    let mut prefix = [0u8; 7];
    file.read_exact(&mut prefix).map_err(RarError::Io)?;
    let head_size = u16::from_le_bytes([prefix[5], prefix[6]]) as usize;
    if head_size < 7 {
        return Err(RarError::Format(
            "RAR4: main header head_size too small".into(),
        ));
    }
    let mut header = prefix.to_vec();
    if head_size > 7 {
        let mut rest = vec![0u8; head_size - 7];
        file.read_exact(&mut rest).map_err(RarError::Io)?;
        header.extend_from_slice(&rest);
    }
    Ok((sfx_offset + 7, header))
}

/// Refuse edits on multi-volume RAR4 sets (the rewrite would need volume
/// rebalancing; see ADR 0005) and header-encrypted (`-hp`) archives (every
/// rebuilt or inserted block would need password re-encryption).
fn refuse_unsupported_containers(archive: &RarArchive, main_flags: u16) -> RarResult<()> {
    if archive.volume_paths.len() > 1 || main_flags & MHD_VOLUME != 0 {
        return Err(RarError::Unsupported(
            "editing multi-volume RAR4 archives is not supported".into(),
        ));
    }
    if main_flags & MHD_PASSWORD != 0 {
        return Err(RarError::Unsupported(
            "editing header-encrypted (-hp) RAR4 archives is not supported yet".into(),
        ));
    }
    Ok(())
}

/// Lock the archive (`rar k`): set the MHD_LOCK bit in the main header and
/// recompute its CRC16, patching the 13-byte block in place. Locking is
/// irreversible; an already-locked archive is a no-op.
pub(crate) fn lock_archive(archive: &RarArchive) -> RarResult<()> {
    if archive.volume_paths.len() > 1 {
        return Err(RarError::Unsupported(
            "locking multi-volume RAR4 archives is not supported (lock the first volume instead)"
                .into(),
        ));
    }
    let (main_offset, main_header) = read_main_from_file(&archive.path, archive.sfx_offset)?;
    let flags = main_flags(&main_header)?;
    refuse_unsupported_containers(archive, flags)?;
    if flags & MHD_LOCK != 0 {
        return Ok(()); // already locked; nothing to do
    }
    let patched = patch_main_header(&main_header, MHD_LOCK)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&archive.path)
        .map_err(RarError::Io)?;
    file.seek(SeekFrom::Start(main_offset))?;
    file.write_all(&patched).map_err(RarError::Io)?;
    file.sync_all().map_err(RarError::Io)?;
    Ok(())
}

/// A parsed plaintext, single-volume RAR4 layout over an in-memory copy of
/// the archive.
struct Rar4Layout {
    /// Offset where the archive signature starts (SFX stub length); the
    /// recovery-record sector grid is anchored here.
    sfx_offset: usize,
    /// Absolute offset of the main header block.
    main_offset: usize,
    /// Raw main header bytes (13 bytes, or longer when a nested comment is
    /// present).
    main_header: Vec<u8>,
    /// Parsed main flags (as stored, LONG_BLOCK included).
    main_flags: u16,
    /// Absolute offset of the end-of-archive block.
    endarc_offset: usize,
}

/// Walk the plaintext block stream of an in-memory RAR4 archive. The
/// archive must already be validated (the editor only reaches here after a
/// successful open scan), so headers are not CRC-checked again; only the
/// envelope bounds are.
fn scan_layout(bytes: &[u8], sfx_offset: usize) -> RarResult<Rar4Layout> {
    let sig = &bytes[sfx_offset..sfx_offset + 7];
    if sig != crate::detect::RAR4_SIGNATURE {
        return Err(RarError::Format(
            "RAR4: signature mismatch while editing".into(),
        ));
    }
    let mut pos = sfx_offset + 7;
    let mut main: Option<(usize, Vec<u8>, u16)> = None;
    let mut endarc: Option<usize> = None;
    while pos + 7 <= bytes.len() {
        let start = pos;
        let head_type = bytes[pos + 2];
        let flags = u16::from_le_bytes([bytes[pos + 3], bytes[pos + 4]]);
        let head_size = u16::from_le_bytes([bytes[pos + 5], bytes[pos + 6]]) as usize;
        if head_size < 7 {
            return Err(RarError::Format("RAR4: block head_size too small".into()));
        }
        let add_size = if flags & LONG_BLOCK != 0 {
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
        if head_type == MAIN_HEAD && main.is_none() {
            let header = bytes[start..start + head_size].to_vec();
            let flags = main_flags(&header)?;
            if flags & MHD_PASSWORD != 0 {
                return Err(RarError::Unsupported(
                    "editing header-encrypted (-hp) RAR4 archives is not supported yet".into(),
                ));
            }
            main = Some((start, header, flags));
        } else if head_type == ENDARC_HEAD {
            endarc = Some(start);
            break;
        }
        pos = start + total;
    }
    let (main_offset, main_header, main_flags) =
        main.ok_or_else(|| RarError::Format("RAR4: archive is missing its main header".into()))?;
    let endarc_offset = endarc.ok_or_else(|| {
        RarError::Format("RAR4: archive is missing its end-of-archive block".into())
    })?;
    Ok(Rar4Layout {
        sfx_offset,
        main_offset,
        main_header,
        main_flags,
        endarc_offset,
    })
}

/// Add or replace the inline recovery record (`rar rr N%`) on an existing
/// RAR4 archive. Member bytes are copied verbatim; the NEWSUB `0x7a`
/// record is rebuilt over the protected prefix (archive start through the
/// last member) with the WinRAR 6.23 sector formula, an existing NEWSUB
/// record is stripped first, and the MHD_RECOVERY main-header bit is set.
pub(crate) fn add_or_replace_recovery(archive: &mut RarArchive, percent: u8) -> RarResult<()> {
    if percent > 100 {
        return Err(RarError::InvalidOption(
            "recovery percent must be in 0..=100".into(),
        ));
    }
    let bytes = fs::read(&archive.path).map_err(RarError::Io)?;
    let sfx_offset = archive.sfx_offset as usize;
    let layout = scan_layout(&bytes, sfx_offset)?;
    refuse_unsupported_containers(archive, layout.main_flags)?;
    if layout.main_flags & MHD_LOCK != 0 {
        return Err(RarError::ArchiveLocked);
    }

    // Locate an existing recovery record. Only a NEWSUB `RR` record that
    // lies entirely before the end-of-archive block is replaceable in
    // place; RAR 2.5-era PROTECT_HEAD records (written after ENDARC) are
    // refused so the rewrite never leaves a stale record behind.
    let scan = scan_protect(&bytes)?;
    let (insert_at, tail_from) = match scan.protect {
        Some(protect) if protect.data_end <= layout.endarc_offset => {
            if &protect.mark != b"Protect+" {
                return Err(RarError::Unsupported(
                    "RAR4: archives with a RAR 2.x PROTECT_HEAD recovery record cannot gain a NEWSUB record in place; recreate the archive".into(),
                ));
            }
            (protect.block_offset, protect.data_end)
        }
        Some(_) => {
            return Err(RarError::Unsupported(
                "RAR4: recovery record is placed after the end-of-archive block and cannot be replaced in place; recreate the archive".into(),
            ));
        }
        None => (layout.endarc_offset, layout.endarc_offset),
    };

    // The record protects everything before it, sector-anchored at the
    // archive start (sfx_offset), matching the repair path's grid. The
    // tags/parity must cover the FINAL bytes on disk — the composed prefix
    // with the MHD_RECOVERY bit already patched into the main header —
    // otherwise the first sector's tag would not match the written file.
    let patched_main = patch_main_header(&layout.main_header, MHD_RECOVERY)?;
    let main_end = layout.main_offset + layout.main_header.len();
    let mut head = Vec::with_capacity(bytes.len() + 64);
    head.extend_from_slice(&bytes[..layout.main_offset]);
    head.extend_from_slice(&patched_main);
    head.extend_from_slice(&bytes[main_end..insert_at]);
    let prefix = &head[layout.sfx_offset..];
    if prefix.is_empty() {
        return Err(RarError::Format(
            "RAR4: nothing to protect with a recovery record".into(),
        ));
    }
    let rec_sectors = recovery_sector_count(prefix.len(), percent);
    let block = build_legacy_recovery_block(prefix, rec_sectors)?;

    let mut out = head;
    out.extend_from_slice(&block);
    out.extend_from_slice(&bytes[tail_from..]);

    let tmp_path = temp_sibling_path(&archive.path);
    {
        let mut file = read_write_create(&tmp_path).map_err(RarError::Io)?;
        file.write_all(&out).map_err(RarError::Io)?;
        file.sync_all().map_err(RarError::Io)?;
    }
    if let Err(error) = replace_file(&tmp_path, &archive.path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }
    // Re-scan the rewritten archive so the in-memory catalog matches the
    // file (members are unchanged by `rr`, but the validation re-checks
    // every block CRC against the new bytes).
    archive.open_read()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_main_header_sets_bits_and_keeps_crc_valid() {
        // Build the same 13-byte main header the writer produces.
        let main = crate::format::rar4::write::build_main_header(0);
        assert_eq!(main.len(), 13);
        let patched = patch_main_header(&main, MHD_LOCK | MHD_RECOVERY).unwrap();
        let flags = u16::from_le_bytes([patched[3], patched[4]]);
        assert_ne!(flags & MHD_LOCK, 0);
        assert_ne!(flags & MHD_RECOVERY, 0);
        // The stored CRC16 must cover patched[2..], like the reader checks.
        let crc = (crate::crc32::crc32(&patched[2..]) & 0xffff) as u16;
        assert_eq!(u16::from_le_bytes([patched[0], patched[1]]), crc);
        // Flags-only patch leaves everything else untouched.
        assert_eq!(&patched[5..], &main[5..]);
    }
}
