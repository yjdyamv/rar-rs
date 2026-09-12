//! Edit engine: the append prelude and the combined RAR4 edit transaction.
//!
//! [`append_prelude`] gates/positions an append (multi-volume and locked
//! archives are refused; solid archives defer to [`super::repack`]);
//! [`edit_rar4`] stages one rewrite that composes deletes, renames, the
//! archive comment, recovery-record rebuilds and per-member comments, then
//! replaces the archive atomically. Multi-volume sets follow the per-volume
//! rule (rename + comment only).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::comment::{build_comment_block, comment_block_name_is_cmt, encode_comment_text};
use super::headers::{file_header_name, rebuild_rar4_header, rename_file_header};
use super::layout::{
    emit_block, header_password, main_flags, patch_main_header, read_block_view,
    refuse_unsupported_containers, scan_layout,
};
use super::repack::repack_solid_archive;
use super::{CMT_HEAD_SIZE, RECOVERY_HEAD_SIZE};
use crate::archive::RarArchive;
use crate::archive::transaction::EditSummary;
use crate::error::{RarError, RarResult};
use crate::format::rar4::{
    FILE_HEAD, MAIN_HEAD, MHD_LOCK, MHD_PASSWORD, MHD_RECOVERY, MHD_SOLID, MHD_VOLUME, NEWSUB_HEAD,
};
use crate::fs::atomic::{commit_files, read_write_create, replace_file, temp_sibling_path};
use crate::fs::volume::volume_base_of;
use crate::recovery::legacy_rr::{
    build_legacy_recovery_block, recovery_sector_count, scan_protect_with_password,
};
/// Header-level edits across a multi-volume RAR4 set: rename and archive
/// comment. Each volume is rewritten as its own block stream (official `rar`
/// does not rebalance volumes, so a volume may grow past `-v`). Every
/// FILE_HEAD carrying a renamed member's name is rebuilt — a split member
/// repeats its name in each volume's chunk header — and a comment change
/// inserts/removes the `CMT` block right after the first volume's main
/// header (WinRAR's placement). The whole set is committed through the shared
/// multi-file transaction, so a failure restores the previous volumes.
fn apply_multivolume_edits(
    archive: &mut RarArchive,
    rename_map: &HashMap<usize, String>,
    comment: Option<&[u8]>,
) -> RarResult<EditSummary> {
    let replace_comment = comment.is_some();
    let mut by_name: HashMap<String, String> = HashMap::new();
    for (idx, new_name) in rename_map {
        let entry = archive.entries.get(*idx).ok_or(RarError::StaleEntryId)?;
        by_name.insert(
            entry.name().trim_end_matches('/').to_string(),
            new_name.clone(),
        );
    }
    let password = header_password(archive);
    let password_bytes = password.map(str::as_bytes);
    let mut matched: HashSet<String> = HashSet::new();
    let mut install: Vec<(PathBuf, PathBuf)> = Vec::new();
    for volume in archive.volume_paths.clone() {
        let bytes = fs::read(&volume).map_err(RarError::Io)?;
        let sig = if volume == archive.path {
            archive.sfx_offset as usize
        } else {
            crate::detect::find_bytes(&bytes, crate::detect::RAR4_SIGNATURE).ok_or_else(|| {
                RarError::Format(format!(
                    "RAR4: {} has no archive signature",
                    volume.display()
                ))
            })?
        };
        let mut out = Vec::with_capacity(bytes.len() + 64);
        out.extend_from_slice(&bytes[..sig + 7]);
        let mut pos = sig + 7;
        let mut hp: Option<&[u8]> = None;
        while pos + 7 <= bytes.len() {
            let start = pos;
            let view = read_block_view(&bytes, pos, hp)?;
            if view.head_type == MAIN_HEAD {
                let flags = main_flags(&view.header)?;
                if flags & MHD_PASSWORD != 0 {
                    hp = password_bytes;
                }
                out.extend_from_slice(&bytes[start..start + view.total]);
                // A comment change inserts its CMT block right after the
                // first volume's main header (WinRAR's placement).
                if volume == archive.path
                    && let Some(text) = comment
                    && !text.is_empty()
                {
                    let (payload, unicode) = encode_comment_text(text);
                    let block = build_comment_block(&payload, unicode);
                    emit_block(
                        &mut out,
                        &block[..CMT_HEAD_SIZE],
                        &block[CMT_HEAD_SIZE..],
                        password,
                    )?;
                }
            } else if view.head_type == FILE_HEAD {
                let name = file_header_name(&view.header)?;
                let key = name.trim_end_matches('/');
                if let Some(new_name) = by_name.get(key) {
                    let new_header = rename_file_header(&view.header, new_name)?;
                    emit_block(&mut out, &new_header, view.data(&bytes, start), password)?;
                    matched.insert(key.to_string());
                } else {
                    out.extend_from_slice(&bytes[start..start + view.total]);
                }
            } else if replace_comment
                && view.head_type == NEWSUB_HEAD
                && view.header.len() >= 32
                && comment_block_name_is_cmt(&view.header)
            {
                // Dropped: the replacement was emitted after the main header.
            } else {
                out.extend_from_slice(&bytes[start..start + view.total]);
            }
            pos = start + view.total;
        }
        let tmp = temp_sibling_path(&volume);
        fs::write(&tmp, &out).map_err(RarError::Io)?;
        install.push((tmp, volume));
    }
    let parent = archive
        .path
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let base = volume_base_of(&archive.path);
    if let Err(error) = commit_files(&parent, &base, &install, &[]) {
        for (tmp, _) in &install {
            let _ = fs::remove_file(tmp);
        }
        return Err(error);
    }
    // Re-scan the committed set so the in-memory catalog and every rebuilt
    // header CRC reflect the new bytes.
    archive.open_read()?;
    Ok(EditSummary {
        deleted: 0,
        renamed: matched.len(),
    })
}

// ── Edit engine ────────────────────────────────────────────────────────────

/// Result of preparing an existing RAR4 archive for append.
pub(crate) struct AppendPrelude {
    /// The archive is solid: appending defers to a whole-archive repack at
    /// close (nothing is staged or truncated here).
    pub solid: bool,
    /// Absolute byte offset where the append starts (the first byte of the
    /// trailing NEWSUB `RR` record, or of the end-of-archive block); `None`
    /// for a solid archive (the repack rebuilds the whole file).
    pub truncate_pos: Option<u64>,
    /// Parity-sector count of the archive's NEWSUB record, if any (the
    /// record is dropped by the truncation and rebuilt at close with the
    /// same strength).
    pub rr_sectors: Option<u32>,
    /// The archive is `-hp` header-encrypted: the appended blocks must be
    /// header-encrypted with the archive password too.
    pub header_encrypted: bool,
}

/// One member buffered for a deferred solid-archive append.
pub(crate) struct SolidAppendEntry {
    pub name: String,
    pub data: Vec<u8>,
    pub level: u8,
    pub mtime: u32,
    pub mtime_ns: u32,
}

/// Prepare an existing single-volume RAR4 archive for appending members.
/// The archive's main flags gate the edit (multi-volume and locked archives
/// are refused). Non-solid archives truncate at the trailing NEWSUB recovery
/// record / end-of-archive block; solid archives defer to a whole-archive
/// repack at close (the writer cannot continue an existing chain).
/// `-hp` archives are appended to under the same header encryption (the
/// password is required and reported by the prelude).
pub(crate) fn append_prelude(archive: &RarArchive) -> RarResult<AppendPrelude> {
    let bytes = fs::read(&archive.path).map_err(RarError::Io)?;
    let layout = scan_layout(
        &bytes,
        archive.sfx_offset as usize,
        header_password(archive),
    )?;
    refuse_unsupported_containers(archive, layout.main_flags)?;
    if layout.main_flags & MHD_LOCK != 0 {
        return Err(RarError::ArchiveLocked);
    }
    let header_encrypted = layout.header_encrypted;
    let hp = if header_encrypted {
        header_password(archive).map(str::as_bytes)
    } else {
        None
    };
    let solid = layout.main_flags & MHD_SOLID != 0;
    if solid {
        // RAR 2.5-era PROTECT_HEAD records cannot be repacked in place.
        let rr_sectors = match scan_protect_with_password(&bytes, hp)?.protect {
            Some(protect) if &protect.mark == b"Protect+" => Some(protect.rec_sectors),
            Some(_) => {
                return Err(RarError::Unsupported(
                    "RAR4: archives with a PROTECT_HEAD recovery record cannot be appended to in place; recreate the archive".into(),
                ));
            }
            None => None,
        };
        return Ok(AppendPrelude {
            solid: true,
            truncate_pos: None,
            rr_sectors,
            header_encrypted,
        });
    }
    // A trailing NEWSUB record sits between the last member and the
    // end-of-archive block; truncating at its start drops it (it cannot
    // protect members appended after it) and it is rebuilt at close. RAR
    // 2.5-era PROTECT_HEAD records cannot be rebuilt this way.
    let (truncate_pos, rr_sectors) = match scan_protect_with_password(&bytes, hp)?.protect {
        Some(protect)
            if &protect.mark == b"Protect+" && protect.data_end <= layout.endarc_offset =>
        {
            (Some(protect.block_offset as u64), Some(protect.rec_sectors))
        }
        Some(_) => {
            return Err(RarError::Unsupported(
                "RAR4: archives with a PROTECT_HEAD recovery record cannot be appended to in place; recreate the archive".into(),
            ));
        }
        None => (Some(layout.endarc_offset as u64), None),
    };
    Ok(AppendPrelude {
        solid: false,
        truncate_pos,
        rr_sectors,
        header_encrypted,
    })
}

/// Apply one combined RAR4 edit transaction: delete members, rename
/// members, set/remove the archive comment, and/or add or rebuild the
/// recovery record, then atomically replace the archive and re-scan it.
/// All edits share one staged rewrite, so a failure leaves the original
/// file untouched.
///
/// Deleting members of a solid archive is refused (that needs the
/// decode->re-encode repack of stage C); non-solid archives drop the whole
/// FILE_HEAD + payload verbatim. Deleting every member erases the archive
/// file, matching `rar d`. `comment` mirrors the RAR5 engine's semantics:
/// `None` keeps the existing comment untouched, `Some(bytes)` installs it
/// (empty bytes remove it).
pub(crate) fn edit_rar4(
    archive: &mut RarArchive,
    deletes: &[usize],
    renames: &[(usize, String)],
    comment: Option<&[u8]>,
    force_rr: Option<u8>,
    member_comments: &[(usize, Option<Vec<u8>>)],
) -> RarResult<EditSummary> {
    if force_rr.is_some_and(|percent| percent > 100) {
        return Err(RarError::InvalidOption(
            "recovery percent must be in 0..=100".into(),
        ));
    }
    let bytes = fs::read(&archive.path).map_err(RarError::Io)?;
    let layout = scan_layout(
        &bytes,
        archive.sfx_offset as usize,
        header_password(archive),
    )?;
    if layout.main_flags & MHD_LOCK != 0 {
        return Err(RarError::ArchiveLocked);
    }
    // `-hp`: the password that decrypts the layout also re-encrypts every
    // block this rewrite rebuilds or inserts.
    let hp = if layout.header_encrypted {
        header_password(archive)
    } else {
        None
    };
    let hp_bytes = hp.map(str::as_bytes);

    // Delete mask: duplicates are harmless; indexes past the catalog are
    // stale. Deleting members of a solid archive needs the whole-archive
    // repack of stage C (decode -> re-encode), matching WinRAR 7.21+.
    let mut deleted = vec![false; archive.entries.len()];
    let mut deleted_count = 0usize;
    for &idx in deletes {
        if idx >= deleted.len() {
            return Err(RarError::StaleEntryId);
        }
        if !deleted[idx] {
            deleted[idx] = true;
            deleted_count += 1;
        }
    }
    // Deleting every member erases the archive file (matching `rar d`);
    // comment and recovery-record changes would be silently dropped, so
    // they are refused too, exactly like the RAR5 engine. This applies to
    // solid archives as well (no repack needed when nothing survives).
    if deleted_count == archive.entries.len() {
        if force_rr.is_some() || comment.is_some() || !renames.is_empty() {
            return Err(RarError::InvalidOption(
                "cannot combine comment, recovery-record or rename changes with deleting every member".into(),
            ));
        }
        std::fs::remove_file(&archive.path).map_err(RarError::Io)?;
        archive.entries.clear();
        return Ok(EditSummary {
            deleted: deleted_count,
            renamed: 0,
        });
    }

    // Multi-volume sets: official `rar` supports the header-level edits on a
    // volume set (rewriting each volume without rebalancing) but refuses
    // member delete/append ("Cannot modify volume"). Ours mirrors that:
    // renames are handled per volume; delete and the not-yet-supported
    // comment/recovery changes are refused clearly.
    if archive.volume_paths.len() > 1 || layout.main_flags & MHD_VOLUME != 0 {
        if deleted_count > 0 {
            return Err(RarError::Unsupported(
                "cannot delete members from a multi-volume RAR4 archive (volume rebalancing is required; official rar refuses too)".into(),
            ));
        }
        if force_rr.is_some() || !member_comments.is_empty() {
            return Err(RarError::Unsupported(
                "recovery-record and per-member-comment edits on multi-volume RAR4 archives are not supported (a volume set uses .rev recovery volumes)".into(),
            ));
        }
        let (rename_map, _) = crate::archive::rename::build_rename_map(&archive.entries, renames)?;
        return apply_multivolume_edits(archive, &rename_map, comment);
    }

    let is_solid = layout.main_flags & MHD_SOLID != 0;
    if deleted_count > 0 && is_solid {
        let (rename_map, renamed) =
            crate::archive::rename::build_rename_map(&archive.entries, renames)?;
        return repack_solid_archive(
            archive,
            &deleted,
            &rename_map,
            comment,
            force_rr,
            renamed,
            &[],
            member_comments,
        );
    }

    let (rename_map, renamed) =
        crate::archive::rename::build_rename_map(&archive.entries, renames)?;
    for (idx, _) in renames {
        if deleted[*idx] {
            return Err(RarError::InvalidOption(
                "cannot rename a member that the same edit deletes".into(),
            ));
        }
    }
    if layout.files.len() != archive.entries.len() {
        return Err(RarError::Format(
            "RAR4: member layout does not match the scan (unsupported archive shape)".into(),
        ));
    }
    if layout.files.is_empty()
        && (deleted_count > 0 || rename_map.keys().next().is_some() || force_rr.is_some())
    {
        return Err(RarError::Format(
            "RAR4: archive has no members to edit".into(),
        ));
    }

    // Decide the recovery-record action. A RAR 2.5-era PROTECT_HEAD record
    // (written after ENDARC, or with a non-NEWSUB mark) cannot be kept
    // valid through a prefix rewrite; refuse rather than leave a stale
    // record behind.
    let existing = match scan_protect_with_password(&bytes, hp_bytes)?.protect {
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
    // A comment change lands its NEWSUB `CMT` block right after the main
    // header (WinRAR's placement). `Some(empty)` removes the comment.
    let replace_comment = comment.is_some();
    if let Some(text) = comment
        && !text.is_empty()
    {
        let (payload, unicode) = encode_comment_text(text);
        let block = build_comment_block(&payload, unicode);
        // Only the 35-byte CMT header is header-encrypted; the payload
        // follows as plaintext data (the same rule as FILE members).
        emit_block(
            &mut out,
            &block[..CMT_HEAD_SIZE],
            &block[CMT_HEAD_SIZE..],
            hp,
        )?;
    }

    let mut pos = main_end;
    let mut file_index = 0usize;
    while pos < region_end {
        let start = pos;
        let view = read_block_view(&bytes, pos, hp_bytes)?;
        if view.head_type == FILE_HEAD {
            let data = view.data(&bytes, start);
            if deleted[file_index] {
                // Drop the member's header and payload verbatim.
            } else {
                let new_name = rename_map.get(&file_index);
                let new_comment = member_comments
                    .iter()
                    .find(|(i, _)| *i == file_index)
                    .map(|(_, c)| c.as_deref());
                if new_name.is_some() || new_comment.is_some() {
                    // The rebuilt header (rename and/or comment) is re-encrypted
                    // with a fresh salt; the member's payload is copied as-is.
                    let rebuilt = rebuild_rar4_header(
                        &view.header,
                        new_name.map(|s| s.as_str()),
                        new_comment,
                    )?;
                    emit_block(&mut out, &rebuilt, data, hp)?;
                } else {
                    // Untouched: copy the on-disk bytes (ciphertext included).
                    out.extend_from_slice(&bytes[start..start + view.total]);
                }
            }
            file_index += 1;
        } else if replace_comment
            && view.head_type == NEWSUB_HEAD
            && view.header.len() >= 32
            && comment_block_name_is_cmt(&view.header)
        {
            // A comment change replaces the existing CMT block (the new one
            // was already emitted after the main header).
        } else {
            out.extend_from_slice(&bytes[start..start + view.total]);
        }
        pos = start + view.total;
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
        // `-hp`: only the 54-byte NEWSUB header is encrypted; the tag table
        // and parity sectors stay plaintext so the record remains usable.
        emit_block(
            &mut out,
            &block[..RECOVERY_HEAD_SIZE],
            &block[RECOVERY_HEAD_SIZE..],
            hp,
        )?;
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
        deleted: deleted_count,
        renamed,
    })
}
