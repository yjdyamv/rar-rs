//! Header-level edits on existing RAR 1.5–4.x archives (ADR 0005, stage A).
//!
//! Three operations ship here: member rename (`rar rn`, and `rar ch` case
//! conversion which routes through the same rename path), inline
//! recovery-record add/replace (`rar rr`) and lock (`rar k`). All are pure
//! header/block surgery — no member data is decoded or recompressed, so
//! solid and non-solid archives are handled identically. The edits are
//! refused up front for multi-volume and header-encrypted (`-hp`)
//! archives, which the later stages of the RAR4 edit rollout will cover.
//!
//! Rename rebuilds each FILE_HEAD's encoded name field in place (keeping
//! every other field byte-identical, including salt / nested comment /
//! extended time) and recomputes the header CRC16 over the reader's
//! coverage. Directory renames expand to descendants, mirroring the RAR5
//! engine. Recovery (`rr`) and any edit on an archive that carries a
//! NEWSUB record rebuild that record over the new prefix — the tags are
//! offset-based, so a renamed header would otherwise leave stale tags.
//!
//! Lock (`k`) patches the 13-byte main header in place (mirroring the RAR5
//! lock). Recovery reads the whole archive into memory — the NEWSUB record
//! is XOR parity over the protected prefix, so the builder needs the
//! prefix anyway (the same bound the RAR4 creation path accepts) — and
//! stages a rewritten copy next to the archive before replacing it
//! atomically.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::RarArchive;
use super::transaction::EditSummary;
use crate::error::{RarError, RarResult};
use crate::format::rar4::write::encode_file_name;
use crate::format::rar4::{
    ENDARC_HEAD, FHD_COMMENT, FHD_LARGE, FHD_SALT, FHD_UNICODE, FILE_HEAD, LONG_BLOCK, MAIN_HEAD,
    MHD_LOCK, MHD_PASSWORD, MHD_RECOVERY, MHD_VOLUME,
};
use crate::fs::atomic::{read_write_create, replace_file, temp_sibling_path};
use crate::recovery::legacy_rr::{
    build_legacy_recovery_block, recovery_sector_count, scan_protect,
};

/// RAR4 header CRC16: standard CRC-32 truncated to 16 bits over
/// `bytes[2..]` (the body after the CRC field).
fn header_crc16(body: &[u8]) -> u16 {
    (crate::crc32::crc32(body) & 0xffff) as u16
}

/// Patch the main-header flags: OR `set_bits` into the flags word at
/// bytes 3..5 and recompute the CRC16. The main header is 13 bytes; when a
/// nested comment is present the reader's CRC coverage stops at 13 bytes,
/// which is exactly where the flags live, so recomputing over `[2..13]` is
/// correct with or without a comment.
fn patch_main_header(main: &[u8], set_bits: u16) -> RarResult<Vec<u8>> {
    if main.len() < 13 || main[2] != MAIN_HEAD {
        return Err(RarError::Format(
            "RAR4: main header block is malformed".into(),
        ));
    }
    let flags = u16::from_le_bytes([main[3], main[4]]);
    let mut patched = main[..13].to_vec();
    patched[3..5].copy_from_slice(&(flags | set_bits).to_le_bytes());
    let crc = header_crc16(&patched[2..]);
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

// ── Layout scan ────────────────────────────────────────────────────────────

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
    /// Every FILE_HEAD block in archive order (file-absolute offset and raw
    /// header bytes). Archive-entry index `i` maps to `files[i]` on a
    /// single-volume archive (each member is one FILE_HEAD block).
    files: Vec<(usize, Vec<u8>)>,
}

/// Read a block envelope at `pos`: `(head_size, flags, total_size)`.
fn block_envelope(bytes: &[u8], pos: usize) -> RarResult<(usize, u16, usize)> {
    if pos + 7 > bytes.len() {
        return Err(RarError::Format("RAR4: truncated block".into()));
    }
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
    Ok((head_size, flags, total))
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
    let mut files = Vec::new();
    while pos + 7 <= bytes.len() {
        let start = pos;
        let head_type = bytes[pos + 2];
        let (head_size, _flags, total) = block_envelope(bytes, pos)?;
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
        } else if head_type == FILE_HEAD {
            files.push((start, bytes[start..start + head_size].to_vec()));
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
        files,
    })
}

// ── Rename ─────────────────────────────────────────────────────────────────

/// Rebuild a FILE_HEAD block with a new encoded name: the fixed 32-byte
/// fields, high sizes, salt, nested comment and extended time are kept
/// byte-identical; only the name field (and the FHD_UNICODE bit / name
/// size) changes. The header CRC16 is recomputed over the reader's
/// coverage — for FILE_HEAD with a nested comment the coverage stops
/// before the comment, otherwise it covers the whole header body.
fn rename_file_header(header: &[u8], new_name: &str) -> RarResult<Vec<u8>> {
    if header.len() < 32 || header[2] != FILE_HEAD {
        return Err(RarError::Format(
            "RAR4: file header block is malformed".into(),
        ));
    }
    let flags = u16::from_le_bytes([header[3], header[4]]);
    let name_start = 32 + if flags & FHD_LARGE != 0 { 8 } else { 0 };
    if name_start + 2 > header.len() {
        return Err(RarError::Format(
            "RAR4: file header is missing its name field".into(),
        ));
    }
    let old_name_size = u16::from_le_bytes([header[26], header[27]]) as usize;
    let name_end = name_start + old_name_size;
    if name_end > header.len() {
        return Err(RarError::Format(
            "RAR4: file name extends past header".into(),
        ));
    }

    let (new_name_bytes, name_flags) = encode_file_name(new_name);
    let new_name_size = new_name_bytes.len();
    if new_name_size > u16::MAX as usize {
        return Err(RarError::InvalidOption(
            "RAR4: renamed member name is too long".into(),
        ));
    }
    let new_flags = (flags & !FHD_UNICODE) | name_flags;

    let mut out = Vec::with_capacity(header.len() - old_name_size + new_name_size);
    out.extend_from_slice(&header[..26]);
    out.extend_from_slice(&(new_name_size as u16).to_le_bytes());
    out.extend_from_slice(&header[28..name_start]);
    out.extend_from_slice(&new_name_bytes);
    out.extend_from_slice(&header[name_end..]);
    // The name grew or shrank: head_size moves with it (every other field
    // keeps its byte length).
    let new_head_size = out.len();
    if new_head_size > u16::MAX as usize {
        return Err(RarError::Format(
            "RAR4: renamed member header is too large".into(),
        ));
    }
    out[3..5].copy_from_slice(&new_flags.to_le_bytes());
    out[5..7].copy_from_slice(&(new_head_size as u16).to_le_bytes());

    let crc_end = if new_flags & FHD_COMMENT != 0 {
        let mut end = 32 + if new_flags & FHD_LARGE != 0 { 8 } else { 0 };
        end += new_name_size;
        if new_flags & FHD_SALT != 0 {
            end += 8;
        }
        end.min(out.len())
    } else {
        out.len()
    };
    let crc = header_crc16(&out[2..crc_end]);
    out[..2].copy_from_slice(&crc.to_le_bytes());
    Ok(out)
}

/// Resolve rename pairs (entry index -> new name) into a map, expanding
/// directory renames to their descendants exactly like the RAR5 engine
/// (`transaction.rs`): each directory member keeps a trailing `/`, every
/// other member whose name starts with `old/` is rewritten to `new/rest`,
/// and renaming the same member twice chains (the later pair sees the
/// earlier one's result as the old name).
fn build_rename_map(
    entries: &[crate::archive::ArchiveEntry],
    renames: &[(usize, String)],
) -> RarResult<(HashMap<usize, String>, usize)> {
    let mut map: HashMap<usize, String> = HashMap::new();
    let mut count = 0usize;
    for (idx, new) in renames {
        if *idx >= entries.len() {
            return Err(RarError::StaleEntryId);
        }
        let old_norm = map
            .get(idx)
            .map(|n| n.as_str())
            .unwrap_or(entries[*idx].name())
            .trim_end_matches('/')
            .to_string();
        let is_dir = entries[*idx].is_dir();
        let new_norm = new.trim_end_matches('/').to_string();
        if is_dir {
            map.insert(*idx, format!("{new_norm}/"));
            let prefix = format!("{old_norm}/");
            for (i, e) in entries.iter().enumerate() {
                if i == *idx || map.contains_key(&i) {
                    continue;
                }
                if let Some(rest) = e.name().strip_prefix(&prefix) {
                    map.insert(i, format!("{new_norm}/{rest}"));
                }
            }
        } else {
            map.insert(*idx, new_norm.clone());
        }
        count += 1;
    }
    Ok((map, count))
}

// ── Edit engine ────────────────────────────────────────────────────────────

/// Apply one combined RAR4 edit transaction: rename members and/or add or
/// rebuild the recovery record, then atomically replace the archive and
/// re-scan it. All edits share one staged rewrite, so a failure leaves the
/// original file untouched.
pub(crate) fn edit_rar4(
    archive: &mut RarArchive,
    renames: &[(usize, String)],
    force_rr: Option<u8>,
) -> RarResult<EditSummary> {
    if force_rr.is_some_and(|percent| percent > 100) {
        return Err(RarError::InvalidOption(
            "recovery percent must be in 0..=100".into(),
        ));
    }
    let bytes = fs::read(&archive.path).map_err(RarError::Io)?;
    let layout = scan_layout(&bytes, archive.sfx_offset as usize)?;
    refuse_unsupported_containers(archive, layout.main_flags)?;
    if layout.main_flags & MHD_LOCK != 0 {
        return Err(RarError::ArchiveLocked);
    }

    let (rename_map, renamed) = build_rename_map(&archive.entries, renames)?;
    if layout.files.len() != archive.entries.len() {
        return Err(RarError::Format(
            "RAR4: member layout does not match the scan (unsupported archive shape)".into(),
        ));
    }
    if layout.files.is_empty() && (rename_map.keys().next().is_some() || force_rr.is_some()) {
        return Err(RarError::Format(
            "RAR4: archive has no members to edit".into(),
        ));
    }

    // Decide the recovery-record action. A RAR 2.5-era PROTECT_HEAD record
    // (written after ENDARC, or with a non-NEWSUB mark) cannot be kept
    // valid through a prefix rewrite; refuse rather than leave a stale
    // record behind.
    let existing = match scan_protect(&bytes)?.protect {
        Some(protect)
            if &protect.mark == b"Protect+" && protect.data_end <= layout.endarc_offset =>
        {
            // Strip the old NEWSUB record; it lies entirely before the
            // end-of-archive block.
            Some((protect.block_offset, protect.data_end, protect.rec_sectors))
        }
        Some(_) => {
            return Err(RarError::Unsupported(
                "RAR4: archives with a PROTECT_HEAD recovery record cannot be edited in place; recreate the archive".into(),
            ));
        }
        None => None,
    };
    // `region_end`/`tail_from`: a rewrite with a record rebuilds the prefix
    // up to where the old record started (or the end-of-archive block) and
    // keeps everything from the old record's data end (or ENDARC) onward.
    let (region_end, tail_from, keep_sectors) = match (&force_rr, existing) {
        (_, Some((old_start, old_end, rec))) => (old_start, old_end, Some(rec)),
        (Some(_), None) => (layout.endarc_offset, layout.endarc_offset, None),
        (None, None) => (layout.endarc_offset, layout.endarc_offset, None),
    };
    let wants_record = force_rr.is_some() || keep_sectors.is_some();
    let patched_main = if wants_record {
        patch_main_header(&layout.main_header, MHD_RECOVERY)?
    } else {
        layout.main_header.clone()
    };
    let main_end = layout.main_offset + layout.main_header.len();
    let mut out = Vec::with_capacity(bytes.len() + 4096);
    out.extend_from_slice(&bytes[..layout.main_offset]);
    out.extend_from_slice(&patched_main);
    let mut pos = main_end;
    let mut file_index = 0usize;
    while pos < region_end {
        let head_type = bytes[pos + 2];
        let (head_size, _flags, total) = block_envelope(&bytes, pos)?;
        if head_type == FILE_HEAD {
            if let Some(new_name) = rename_map.get(&file_index) {
                let rebuilt = rename_file_header(&bytes[pos..pos + head_size], new_name)?;
                out.extend_from_slice(&rebuilt);
            } else {
                out.extend_from_slice(&bytes[pos..pos + head_size]);
            }
            out.extend_from_slice(&bytes[pos + head_size..pos + total]);
            file_index += 1;
        } else {
            out.extend_from_slice(&bytes[pos..pos + total]);
        }
        pos += total;
    }
    if pos != region_end {
        return Err(RarError::Format(
            "RAR4: block walk ended before the expected region end".into(),
        ));
    }

    // Append the recovery record when the plan wants one (a fresh record at
    // `percent`, or a rebuild keeping the original parity-sector strength),
    // then the tail (old record's data end onward, or ENDARC + trailing
    // bytes).
    if wants_record {
        let prefix = &out[layout.sfx_offset..];
        if prefix.is_empty() {
            return Err(RarError::Format(
                "RAR4: nothing to protect with a recovery record".into(),
            ));
        }
        let rec_sectors = match (force_rr, keep_sectors) {
            (Some(percent), _) => recovery_sector_count(prefix.len(), percent),
            (None, Some(rec)) => rec,
            (None, None) => unreachable!("wants_record implies a source"),
        };
        let block = build_legacy_recovery_block(prefix, rec_sectors)?;
        out.extend_from_slice(&block);
    }
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
    // file and every rebuilt header CRC is validated against the new bytes.
    archive.open_read()?;
    Ok(EditSummary {
        deleted: 0,
        renamed,
    })
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)] // legacy facade namelist/read kept for byte-parity checks
    use super::*;
    use crate::format::rar4::RAR4_METHOD_STORE;
    use crate::format::rar4::write::{FileHeaderParams, build_endarc, build_file_header};

    fn file_block(name: &str, payload: &[u8]) -> Vec<u8> {
        let (name_bytes, _flags) = encode_file_name(name);
        let mut h = build_file_header(&FileHeaderParams {
            flags: 0,
            packed_size: payload.len() as u32,
            unpacked_size: payload.len() as u32,
            host_os: 0,
            file_crc: crate::crc32::crc32(payload),
            file_time: 0,
            unp_ver: 20,
            method: RAR4_METHOD_STORE,
            name: &name_bytes,
            attr: 0x20,
            window_bits: 0,
            salt: None,
            ext_time: None,
        })
        .unwrap();
        h.extend_from_slice(payload);
        h
    }

    fn archive_bytes(member_blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut out = crate::detect::RAR4_SIGNATURE.to_vec();
        out.extend_from_slice(&crate::format::rar4::write::build_main_header(0));
        for block in member_blocks {
            out.extend_from_slice(block);
        }
        out.extend_from_slice(&build_endarc(0));
        out
    }

    #[test]
    fn patch_main_header_sets_bits_and_keeps_crc_valid() {
        let main = crate::format::rar4::write::build_main_header(0);
        assert_eq!(main.len(), 13);
        let patched = patch_main_header(&main, MHD_LOCK | MHD_RECOVERY).unwrap();
        let flags = u16::from_le_bytes([patched[3], patched[4]]);
        assert_ne!(flags & MHD_LOCK, 0);
        assert_ne!(flags & MHD_RECOVERY, 0);
        let crc = header_crc16(&patched[2..]);
        assert_eq!(u16::from_le_bytes([patched[0], patched[1]]), crc);
        assert_eq!(&patched[5..], &main[5..]);
    }

    #[test]
    fn rename_file_header_swaps_the_name_field_only() {
        let (name_bytes, name_flags) = encode_file_name("plain.txt");
        let header = build_file_header(&FileHeaderParams {
            flags: name_flags,
            packed_size: 100,
            unpacked_size: 100,
            host_os: 0,
            file_crc: 0x1234_5678,
            file_time: 0x9abc_def0,
            unp_ver: 20,
            method: RAR4_METHOD_STORE,
            name: &name_bytes,
            attr: 0x20,
            window_bits: 0,
            salt: Some([0x11; 8]),
            ext_time: Some(&[1, 2, 3, 4]),
        })
        .unwrap();
        let renamed = rename_file_header(&header, "重命名-ünï.txt").unwrap();

        // Fields outside the name (fixed 32-byte block + salt/ext-time tail)
        // are preserved byte-for-byte; only CRC, flags, head_size and the
        // name_size/name pair change.
        assert_eq!(&renamed[7..26], &header[7..26]);
        assert_eq!(&renamed[28..32], &header[28..32]);
        let new_hs = u16::from_le_bytes([renamed[5], renamed[6]]) as usize;
        assert_eq!(new_hs, renamed.len());
        // The new name decodes back through the reader path.
        let flags = u16::from_le_bytes([renamed[3], renamed[4]]);
        assert_ne!(flags & FHD_UNICODE, 0);
        let name_size = u16::from_le_bytes([renamed[26], renamed[27]]) as usize;
        let decoded = crate::format::rar4::decode_file_name(&renamed[32..32 + name_size], flags);
        assert_eq!(decoded, "重命名-ünï.txt");
        // CRC16 covers the whole (comment-free) header body, like the reader.
        let crc = header_crc16(&renamed[2..]);
        assert_eq!(u16::from_le_bytes([renamed[0], renamed[1]]), crc);
    }

    #[test]
    fn rename_members_roundtrips_through_open_and_keeps_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rename.rar");
        let a_payload = vec![0x42; 5_000];
        let b_payload = vec![0x77; 3_000];
        std::fs::write(
            &path,
            archive_bytes(&[
                file_block("a.bin", &a_payload),
                file_block("b.txt", &b_payload),
            ]),
        )
        .unwrap();

        let archive = RarArchive::open(&path).unwrap();
        assert_eq!(archive.namelist(), ["a.bin", "b.txt"]);

        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let a = editor.unique_entry("a.bin").unwrap();
        let b = editor.unique_entry("b.txt").unwrap();
        let report = editor
            .apply(
                crate::archive::editor::EditPlan::new()
                    .rename(a, "alpha.bin")
                    .rename(b, "贝塔.txt"),
            )
            .unwrap();
        assert_eq!(report.renamed(), 2);

        drop(editor);
        let mut archive = RarArchive::open(&path).unwrap();
        assert_eq!(archive.namelist(), ["alpha.bin", "贝塔.txt"]);
        assert_eq!(archive.read("alpha.bin").unwrap(), a_payload);
        assert_eq!(archive.read("贝塔.txt").unwrap(), b_payload);
    }

    #[test]
    fn dir_rename_expands_to_descendants_and_rebuilds_rr() {
        // Hand-build: d/ (directory member) + d/x.txt, plus a NEWSUB `rr`
        // record over the prefix and an ENDARC, mirroring the writer's
        // layout (members, RR, ENDARC).
        let (dir_name, _) = encode_file_name("d/");
        let dir_head = build_file_header(&FileHeaderParams {
            flags: 0,
            packed_size: 0,
            unpacked_size: 0,
            host_os: 2,
            file_crc: 0,
            file_time: 0,
            unp_ver: 20,
            method: RAR4_METHOD_STORE,
            name: &dir_name,
            attr: 0x10, // FILE_ATTRIBUTE_DIRECTORY
            window_bits: 0,
            salt: None,
            ext_time: None,
        })
        .unwrap();
        let x_payload = vec![0x5a; 2_000];
        let mut prefix = crate::detect::RAR4_SIGNATURE.to_vec();
        prefix.extend_from_slice(&crate::format::rar4::write::build_main_header(0));
        prefix.extend_from_slice(&dir_head);
        prefix.extend_from_slice(&file_block("d/x.txt", &x_payload));
        let rec = recovery_sector_count(prefix.len(), 10);
        let rr_block = build_legacy_recovery_block(&prefix, rec).unwrap();
        let mut full = prefix;
        full.extend_from_slice(&rr_block);
        full.extend_from_slice(&build_endarc(0));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dirs.rar");
        std::fs::write(&path, &full).unwrap();

        let mut editor = crate::archive::editor::ArchiveEditor::open(&path).unwrap();
        let names: Vec<String> = editor.entries().map(|e| e.name().to_string()).collect();
        assert_eq!(names, ["d/", "d/x.txt"]);
        let d = editor.unique_entry("d/").unwrap();
        let report = editor
            .apply(crate::archive::editor::EditPlan::new().rename(d, "renamed"))
            .unwrap();
        assert_eq!(report.renamed(), 1, "one explicit pair");

        drop(editor);
        let mut archive = RarArchive::open(&path).unwrap();
        assert_eq!(archive.namelist(), ["renamed/", "renamed/x.txt"]);
        assert_eq!(archive.read("renamed/x.txt").unwrap(), x_payload);

        // The archive still carries a valid, repairable recovery record
        // (rebuilt over the renamed prefix): damage a protected sector and
        // repair it back byte-for-byte.
        let rewritten = std::fs::read(&path).unwrap();
        assert!(scan_protect(&rewritten).unwrap().protect.is_some());
        let mut damaged = rewritten.clone();
        damaged[1_000..1_064].fill(0xcc);
        let damaged_path = dir.path().join("dmg.rar");
        std::fs::write(&damaged_path, &damaged).unwrap();
        let fixed_path = dir.path().join("fixed.rar");
        assert!(crate::recovery::repair_legacy_archive_path(&damaged_path, &fixed_path).unwrap());
        assert_eq!(std::fs::read(&fixed_path).unwrap(), rewritten);
    }
}
