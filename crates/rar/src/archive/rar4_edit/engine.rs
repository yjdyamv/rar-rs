//! Edit engine: the append prelude and the combined RAR4 edit transaction.
//!
//! [`append_prelude`] gates/positions an append (multi-volume and locked
//! archives are refused; solid archives defer to [`super::repack`]);
//! [`edit_rar4`] stages one rewrite that composes deletes, renames, the
//! archive comment, recovery-record rebuilds and per-member comments, then
//! replaces the archive atomically. Blocks are streamed through bounded
//! buffers; only a rebuilt recovery record's protected prefix is retained
//! (its parity covers the whole prefix by construction). Multi-volume sets
//! follow the per-volume rule (rename + comment only).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::headers::{file_header_name, rebuild_rar4_header, rename_file_header};
use super::layout::{
    archive_is_locked, copy_range, emit_block, first_volume, header_password, locate_signature,
    main_flags, patch_main_header, refuse_unsupported_containers, scan_layout_stream,
};
use super::repack::repack_solid_archive;
use super::{CMT_HEAD_SIZE, RECOVERY_HEAD_SIZE};
use crate::archive::RarArchive;
use crate::archive::transaction::EditSummary;
use crate::error::{RarError, RarResult};
use crate::format::rar4::comment::{
    build_comment_block, comment_block_name_is_cmt, encode_comment_text,
};
use crate::format::rar4::{
    COMM_HEAD, EnvelopePolicy, FILE_HEAD, MAIN_HEAD, MHD_LOCK, MHD_PASSWORD, MHD_RECOVERY,
    MHD_SOLID, MHD_VOLUME, NEWSUB_HEAD, read_block,
};
use crate::fs::atomic::{commit_files, install_durable, read_write_create, temp_sibling_path};
use crate::fs::volume::{stale_volume_paths, volume_base_of};
use crate::recovery::legacy_rr::{build_legacy_recovery_block, recovery_sector_count};

/// Header-level edits across a multi-volume RAR4 set: rename and archive
/// comment. Each volume is rewritten as its own block stream (official `rar`
/// does not rebalance volumes, so a volume may grow past `-v`). Every
/// FILE_HEAD carrying a renamed member's name is rebuilt — a split member
/// repeats its name in each volume's chunk header — and a comment change
/// inserts/removes the `CMT` block right after the first volume's main
/// header (WinRAR's placement), regardless of which part was opened. The
/// whole set is committed through the shared multi-file transaction, so a
/// failure restores the previous volumes.
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
    let first_path = first_volume(archive).to_path_buf();
    let mut matched: HashSet<String> = HashSet::new();
    let mut install: Vec<(PathBuf, PathBuf)> = Vec::new();
    for volume in archive.volume_paths.clone() {
        let is_first = volume == first_path;
        let tmp = temp_sibling_path(&volume);
        let staged = (|| -> RarResult<()> {
            let mut src = File::open(&volume).map_err(RarError::Io)?;
            // The first volume may carry an SFX stub; every later volume
            // starts at its own signature. `archive.sfx_offset` belongs to
            // the opened file, so it is only trusted for the opened first
            // volume.
            let sig = if is_first && volume == archive.path {
                archive.sfx_offset as usize
            } else {
                locate_signature(&mut src)?
            };
            let file_len = src.metadata().map_err(RarError::Io)?.len();
            let mut out = read_write_create(&tmp).map_err(RarError::Io)?;
            // Signature (and any SFX stub) through the end of the signature.
            copy_range(&mut src, &mut out, 0, sig as u64 + 7)?;
            src.seek(SeekFrom::Start(sig as u64 + 7))
                .map_err(RarError::Io)?;
            let mut pos = sig as u64 + 7;
            let mut hp: Option<&[u8]> = None;
            // The same latch in the `&str` form `emit_block` re-encrypts
            // with. Both stay `None` on a plain set even when a password
            // was supplied for the edit: only `MHD_PASSWORD` turns headers
            // into ciphertext.
            let mut hp_password: Option<&str> = None;
            while pos < file_len {
                let view = read_block(&mut src, hp.is_some(), hp, EnvelopePolicy::EDIT)?
                    .ok_or_else(|| RarError::Format("RAR4: truncated block stream".into()))?;
                if view.head_type == MAIN_HEAD {
                    let flags = main_flags(&view.header)?;
                    if flags & MHD_PASSWORD != 0 {
                        hp_password = password;
                        hp = password.map(str::as_bytes);
                    }
                    out.write_all(view.raw_header()).map_err(RarError::Io)?;
                    // A comment change inserts its CMT block right after the
                    // first volume's main header (WinRAR's placement).
                    if is_first
                        && let Some(text) = comment
                        && !text.is_empty()
                    {
                        let (payload, unicode) = encode_comment_text(text);
                        let block = build_comment_block(&payload, unicode);
                        let mut emitted = Vec::new();
                        emit_block(
                            &mut emitted,
                            &block[..CMT_HEAD_SIZE],
                            &block[CMT_HEAD_SIZE..],
                            hp_password,
                        )?;
                        out.write_all(&emitted).map_err(RarError::Io)?;
                    }
                } else if view.head_type == FILE_HEAD {
                    let name = file_header_name(&view.header)?;
                    let key = name.trim_end_matches('/');
                    if let Some(new_name) = by_name.get(key) {
                        let new_header = rename_file_header(&view.header, new_name)?;
                        let mut emitted = Vec::new();
                        emit_block(&mut emitted, &new_header, &[], hp_password)?;
                        out.write_all(&emitted).map_err(RarError::Io)?;
                        copy_range(&mut src, &mut out, view.data_offset(), view.add_size)?;
                        matched.insert(key.to_string());
                    } else {
                        out.write_all(view.raw_header()).map_err(RarError::Io)?;
                        copy_range(&mut src, &mut out, view.data_offset(), view.add_size)?;
                    }
                } else if replace_comment
                    && view.head_type == NEWSUB_HEAD
                    && view.header.len() >= 32
                    && comment_block_name_is_cmt(&view.header)
                {
                    // Dropped: the replacement was emitted after the main
                    // header.
                } else {
                    out.write_all(view.raw_header()).map_err(RarError::Io)?;
                    copy_range(&mut src, &mut out, view.data_offset(), view.add_size)?;
                }
                pos = view.end();
                src.seek(SeekFrom::Start(pos)).map_err(RarError::Io)?;
            }
            out.sync_all().map_err(RarError::Io)?;
            Ok(())
        })();
        if let Err(error) = staged {
            let _ = fs::remove_file(&tmp);
            for (tmp, _) in &install {
                let _ = fs::remove_file(tmp);
            }
            return Err(error);
        }
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
    crate::format::shared::extract::open::open_read(archive)?;
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

/// Prepare an existing single-volume RAR4 archive for appending members.
/// The archive's main flags gate the edit (multi-volume and locked archives
/// are refused). Non-solid archives truncate at the trailing NEWSUB recovery
/// record / end-of-archive block; solid archives defer to a whole-archive
/// repack at close (the writer cannot continue an existing chain).
/// `-hp` archives are appended to under the same header encryption (the
/// password is required and reported by the prelude).
pub(crate) fn append_prelude(archive: &RarArchive) -> RarResult<AppendPrelude> {
    let layout = {
        let mut src = File::open(&archive.path).map_err(RarError::Io)?;
        scan_layout_stream(
            &mut src,
            archive.sfx_offset as usize,
            header_password(archive),
        )?
    };
    refuse_unsupported_containers(archive, layout.main_flags)?;
    if layout.main_flags & MHD_LOCK != 0 {
        return Err(RarError::ArchiveLocked);
    }
    let header_encrypted = layout.header_encrypted;
    let solid = layout.main_flags & MHD_SOLID != 0;
    if solid {
        // RAR 2.5-era PROTECT_HEAD records cannot be repacked in place.
        let rr_sectors = match &layout.protect {
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
    let (truncate_pos, rr_sectors) = match &layout.protect {
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

/// Erase an RAR4 archive: every data volume of the set plus any `.rev`
/// recovery volumes for the same base. The journaled commit parks each file
/// first, so a failure restores the whole set and the operation never leaves
/// orphan volumes behind or reports success after a partial removal.
fn erase_rar4_archive(archive: &mut RarArchive, deleted: usize) -> RarResult<EditSummary> {
    let parent = archive
        .path
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let base = volume_base_of(&archive.path);
    let mut retire = archive.volume_paths.clone();
    for stale in stale_volume_paths(
        &parent,
        &base,
        true,
        &retire,
        &crate::recovery::rev3::rev_name_belongs_to_set,
    ) {
        if !retire.contains(&stale) {
            retire.push(stale);
        }
    }
    retire.sort();
    retire.dedup();
    commit_files(&parent, &base, &[], &retire)?;
    archive.entries.clear();
    Ok(EditSummary {
        deleted,
        renamed: 0,
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
/// file (every volume of a set), matching `rar d`. `comment` mirrors the
/// RAR5 engine's semantics: `None` keeps the existing comment untouched,
/// `Some(bytes)` installs it (empty bytes remove it).
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
    let layout = {
        let mut src = File::open(&archive.path).map_err(RarError::Io)?;
        scan_layout_stream(
            &mut src,
            archive.sfx_offset as usize,
            header_password(archive),
        )?
    };
    // The lock bit lives on the set's first volume; a later part opened on
    // its own still sees (and refuses) a locked archive.
    if layout.main_flags & MHD_LOCK != 0
        || (first_volume(archive) != archive.path.as_path() && archive_is_locked(archive)?)
    {
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
    // solid archives as well (no repack needed when nothing survives) and
    // to multi-volume sets (every volume is removed).
    if deleted_count == archive.entries.len() {
        if force_rr.is_some() || comment.is_some() || !renames.is_empty() {
            return Err(RarError::InvalidOption(
                "cannot combine comment, recovery-record or rename changes with deleting every member".into(),
            ));
        }
        return erase_rar4_archive(archive, deleted_count);
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
    let existing = match &layout.protect {
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
    let replace_comment = comment.is_some();

    // The rewrite streams the original archive into a sibling staging file
    // block by block: member payloads are copied through a bounded buffer,
    // never materialized.
    let src_path = archive.path.clone();
    let tmp_path = temp_sibling_path(&src_path);
    let rewrite = (|| -> RarResult<()> {
        let mut src = File::open(&src_path).map_err(RarError::Io)?;
        let mut out = read_write_create(&tmp_path).map_err(RarError::Io)?;
        // The SFX stub (when any) is copied verbatim.
        copy_range(&mut src, &mut out, 0, layout.main_offset as u64)?;
        out.write_all(&patched_main).map_err(RarError::Io)?;
        // A comment change lands its NEWSUB `CMT` block right after the main
        // header (WinRAR's placement). `Some(empty)` removes the comment.
        if let Some(text) = comment
            && !text.is_empty()
        {
            let (payload, unicode) = encode_comment_text(text);
            let block = build_comment_block(&payload, unicode);
            // Only the 35-byte CMT header is header-encrypted; the payload
            // follows as plaintext data (the same rule as FILE members).
            let mut emitted = Vec::new();
            emit_block(
                &mut emitted,
                &block[..CMT_HEAD_SIZE],
                &block[CMT_HEAD_SIZE..],
                hp,
            )?;
            out.write_all(&emitted).map_err(RarError::Io)?;
        }

        src.seek(SeekFrom::Start(main_end as u64))
            .map_err(RarError::Io)?;
        let mut pos = main_end as u64;
        let mut file_index = 0usize;
        // Whether the block right after the current member (its standalone
        // comment) belongs to a member whose comment is being replaced or
        // was deleted: that block is dropped.
        let mut drop_standalone_comment = false;
        while pos < region_end as u64 {
            let view = read_block(&mut src, hp_bytes.is_some(), hp_bytes, EnvelopePolicy::EDIT)?
                .ok_or_else(|| RarError::Format("RAR4: truncated block stream".into()))?;
            if view.head_type == FILE_HEAD {
                if deleted[file_index] {
                    // Drop the member's header and payload verbatim (and
                    // its standalone comment block, when one follows).
                    drop_standalone_comment = true;
                } else {
                    let new_name = rename_map.get(&file_index);
                    let comment_change = member_comments
                        .iter()
                        .find(|(i, _)| *i == file_index)
                        .map(|(_, c)| c.as_deref());
                    drop_standalone_comment = comment_change.is_some();
                    if new_name.is_some() || comment_change.is_some() {
                        // The rebuilt header (rename and/or legacy nested
                        // comment stripped) is re-encrypted with a fresh
                        // salt; the member's payload is copied as-is.
                        let rebuilt = rebuild_rar4_header(
                            &view.header,
                            new_name.map(|s| s.as_str()),
                            comment_change.is_some(),
                        )?;
                        let mut emitted = Vec::new();
                        emit_block(&mut emitted, &rebuilt, &[], hp)?;
                        out.write_all(&emitted).map_err(RarError::Io)?;
                        copy_range(&mut src, &mut out, view.data_offset(), view.add_size)?;
                        // RAR 3.x/4.x stores the replacement comment as a
                        // standalone COMM_HEAD block after the member data.
                        if let Some(Some(text)) = comment_change
                            && !text.is_empty()
                        {
                            let block = crate::format::rar4::write::build_file_comment_block(text);
                            // `HEAD_SIZE` spans the payload, so under `-hp`
                            // the whole block is one encrypted header block.
                            let mut emitted = Vec::new();
                            emit_block(&mut emitted, &block, &[], hp)?;
                            out.write_all(&emitted).map_err(RarError::Io)?;
                        }
                    } else {
                        // Untouched: copy the on-disk bytes (ciphertext
                        // included).
                        out.write_all(view.raw_header()).map_err(RarError::Io)?;
                        copy_range(&mut src, &mut out, view.data_offset(), view.add_size)?;
                    }
                }
                file_index += 1;
            } else if view.head_type == COMM_HEAD && drop_standalone_comment {
                // The member's old standalone comment: dropped (the
                // replacement, if any, was emitted after its data).
                drop_standalone_comment = false;
            } else if replace_comment
                && view.head_type == NEWSUB_HEAD
                && view.header.len() >= 32
                && comment_block_name_is_cmt(&view.header)
            {
                // A comment change replaces the existing CMT block (the new
                // one was already emitted after the main header).
            } else {
                out.write_all(view.raw_header()).map_err(RarError::Io)?;
                copy_range(&mut src, &mut out, view.data_offset(), view.add_size)?;
            }
            pos = view.end();
            src.seek(SeekFrom::Start(pos)).map_err(RarError::Io)?;
        }
        if pos != region_end as u64 {
            return Err(RarError::Format(
                "RAR4: block walk ended before the expected region end".into(),
            ));
        }

        // Append the recovery record when the plan wants one (a fresh record
        // at `percent`, or a rebuild keeping the original parity-sector
        // strength). The parity covers the whole new prefix, so it is read
        // back from the staged file instead of being held while writing.
        if wants_record {
            let out_len = out.stream_position().map_err(RarError::Io)? as usize;
            let prefix_len = out_len.checked_sub(layout.sfx_offset).ok_or_else(|| {
                RarError::Format("RAR4: rewritten prefix is shorter than the archive start".into())
            })?;
            if prefix_len == 0 {
                return Err(RarError::Format(
                    "RAR4: nothing to protect with a recovery record".into(),
                ));
            }
            let mut prefix = vec![0u8; prefix_len];
            {
                let mut reader = File::open(&tmp_path).map_err(RarError::Io)?;
                reader.seek(SeekFrom::Start(layout.sfx_offset as u64))?;
                reader.read_exact(&mut prefix).map_err(RarError::Io)?;
            }
            let rec_sectors = match (force_rr, keep_sectors) {
                (Some(percent), _) => recovery_sector_count(prefix.len(), percent),
                (None, Some(rec)) => rec,
                (None, None) => unreachable!("wants_record implies a source"),
            };
            let block = build_legacy_recovery_block(&prefix, rec_sectors)?;
            // `-hp`: only the 54-byte NEWSUB header is encrypted; the tag
            // table and parity sectors stay plaintext so the record remains
            // usable.
            let mut header_out = Vec::new();
            emit_block(&mut header_out, &block[..RECOVERY_HEAD_SIZE], &[], hp)?;
            out.write_all(&header_out).map_err(RarError::Io)?;
            out.write_all(&block[RECOVERY_HEAD_SIZE..])
                .map_err(RarError::Io)?;
        }

        // Keep the tail (old record's data end onward, or ENDARC + trailing
        // bytes) verbatim.
        let file_len = src.metadata().map_err(RarError::Io)?.len();
        if tail_from as u64 > file_len {
            return Err(RarError::Format(
                "RAR4: recovery tail lies past the archive end".into(),
            ));
        }
        copy_range(
            &mut src,
            &mut out,
            tail_from as u64,
            file_len - tail_from as u64,
        )?;
        out.sync_all().map_err(RarError::Io)?;
        Ok(())
    })();
    if let Err(error) = rewrite {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }
    // Validate the staged rewrite before installing it: reopening it runs
    // the same CRC-checked block scan the reader uses, so a structurally
    // broken output (misaligned blocks, a stale header CRC) can never
    // replace the original. A failed check leaves the original untouched.
    let staged = match hp {
        Some(password) => RarArchive::open_with_password(&tmp_path, password).map(drop),
        None => RarArchive::open(&tmp_path).map(drop),
    };
    if let Err(error) = staged {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }
    if let Err(error) = install_durable(&tmp_path, &archive.path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }
    // Re-scan the rewritten archive so the in-memory catalog matches the
    // file and every rebuilt header CRC is validated against the new bytes.
    crate::format::shared::extract::open::open_read(archive)?;
    Ok(EditSummary {
        deleted: deleted_count,
        renamed,
    })
}
