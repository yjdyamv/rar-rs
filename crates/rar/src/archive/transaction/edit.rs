//! Edit entry points: planning, comment access and the locked check.

use super::*;

use std::fs::{self, File};
use std::path::Path;

use std::io::{Read, Seek, SeekFrom};

use super::super::{Mode, RarArchive};
use crate::crypto::parse_archive_encrypt_header;
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::ArchiveHeader;
use crate::format::rar5::{
    ARCHIVE_FLAG_LOCKED, BLOCK_TYPE_ARCHIVE_HEADER, BLOCK_TYPE_ENCRYPT_HEADER,
    BLOCK_TYPE_END_ARCHIVE, BLOCK_TYPE_SERVICE_HEADER,
};
use crate::fs::atomic::{read_write_create, replace_file, temp_sibling_path};

impl RarArchive {
    pub(crate) fn edit_plan(
        &mut self,
        delete_indexes: &[usize],
        renames: &[(usize, String)],
        force_rr: Option<u8>,
        comment: Option<&[u8]>,
    ) -> RarResult<EditSummary> {
        if self.mode != Mode::Read {
            return Err(RarError::Format(
                "edit requires an archive opened for reading".into(),
            ));
        }
        if force_rr.is_some_and(|percent| percent > 100) {
            return Err(RarError::InvalidOption(
                "recovery percent must be in 0..=100".into(),
            ));
        }
        if self.volume_paths.len() > 1 && (force_rr.is_some() || comment.is_some()) {
            return Err(RarError::Unsupported(
                "comment and recovery-record changes are not supported for multi-volume archives"
                    .into(),
            ));
        }
        self.ensure_write_ctx();
        if (!renames.is_empty() || force_rr.is_some() || comment.is_some())
            && self.main_header_is_locked()?
        {
            return Err(RarError::ArchiveLocked);
        }
        // Renaming or setting the archive comment rewrites header blocks; a
        // header-encrypted (RAR5 `-hp`) archive would need each rewritten or
        // new block re-encrypted, which this transaction does not implement.
        // Refuse before touching the file — the previous behaviour replaced
        // the archive with a corrupt one. Deletes and recovery-record
        // changes keep working (they only copy encrypted blocks verbatim or
        // write service records through the encrypting writer).
        if self.header_encryption && (!renames.is_empty() || comment.is_some()) {
            return Err(RarError::Unsupported(
                "renaming and archive comments are not supported for header-encrypted (RAR5 -hp) archives"
                    .into(),
            ));
        }

        // Delete mask: duplicates are fine (a member can only be deleted
        // once); out-of-range indexes are callers' bugs, so surface them.
        let mut deleted = vec![false; self.entries.len()];
        let mut deleted_count = 0usize;
        for &idx in delete_indexes {
            if idx >= deleted.len() {
                return Err(RarError::StaleEntryId);
            }
            if !deleted[idx] {
                deleted[idx] = true;
                deleted_count += 1;
            }
        }
        for (idx, _) in renames {
            if *idx >= self.entries.len() {
                return Err(RarError::StaleEntryId);
            }
            if deleted[*idx] {
                return Err(RarError::InvalidOption(
                    "cannot rename a member that the same edit deletes".into(),
                ));
            }
        }

        let (map, renamed_count) = super::super::rename::build_rename_map(&self.entries, renames)?;
        if deleted_count == 0 && renamed_count == 0 && force_rr.is_none() && comment.is_none() {
            return Err(RarError::Format("no members to edit".into()));
        }

        if deleted_count == self.entries.len() {
            // Matching `rar d`: deleting every member erases the archive
            // (every volume for multi-volume archives). Renames are empty
            // here — the disjointness check above leaves no target — and
            // comment/recovery changes would be silently dropped, so they
            // are refused too.
            if force_rr.is_some() || comment.is_some() {
                return Err(RarError::InvalidOption(
                    "cannot combine comment or recovery-record changes with deleting every member"
                        .into(),
                ));
            }
            if self.main_header_is_locked()? {
                return Err(RarError::ArchiveLocked);
            }
            for vol in &self.volume_paths {
                let _ = fs::remove_file(vol);
            }
            self.stream = None;
            self.entries.clear();
            self.read_ctx_mut().solid_state = None;
            self.read_ctx_mut().solid_decoded_through = -1;
            return Ok(EditSummary {
                deleted: deleted_count,
                renamed: 0,
            });
        }

        let chain = if deleted_count > 0 {
            let first_deleted = deleted.iter().position(|d| *d).unwrap();
            self.chain_range_around(first_deleted)
        } else {
            None
        };
        // The engine applies renames to verbatim copies only; a kept member
        // of a recompressed chain would silently keep its old name. Refuse
        // that combination up front. Deleted members are irrelevant (they
        // are not emitted), so only kept members in the chain matter.
        if let Some((start, end)) = chain {
            for idx in map.keys() {
                if !deleted[*idx] && (start..=end).contains(idx) {
                    return Err(RarError::Unsupported(
                        "renaming a member of a solid chain that also loses a member is not supported; split the edit into separate transactions"
                            .into(),
                    ));
                }
            }
        }

        self.rewrite_edit(deleted, chain, &map, force_rr, comment)?;
        Ok(EditSummary {
            deleted: deleted_count,
            renamed: renamed_count,
        })
    }

    /// Build the rename map (index -> new name) for resolved rename pairs,
    /// expanding directory renames to their descendants with the same rules
    /// as the name-based path. Returns the map and the number of explicit
    /// rename pairs.
    /// Run one staged edit rewrite (delete mask + rename map) against the
    /// original archive and reload the catalog. Single-volume archives are
    /// rewritten through a sibling file that replaces the original only on
    /// success; multi-volume archives are re-split at their volume size and
    /// their .rev recovery volumes regenerated.
    fn rewrite_edit(
        &mut self,
        deleted: Vec<bool>,
        chain: Option<(usize, usize)>,
        map: &std::collections::HashMap<usize, String>,
        force_rr: Option<u8>,
        comment: Option<&[u8]>,
    ) -> RarResult<()> {
        let rename_map = (!map.is_empty()).then_some(map);
        // Comment and recovery-record changes are validated out of the
        // multi-volume path above (rewrite_multivolume cannot carry them).
        debug_assert!(
            self.volume_paths.len() <= 1 || (force_rr.is_none() && comment.is_none()),
            "comment/recovery edits must be single-volume"
        );
        if self.volume_paths.len() > 1 {
            self.rewrite_multivolume(&deleted, chain, rename_map)?;
        } else {
            let src_path = self.path.clone();
            let tmp_path = temp_sibling_path(&src_path);
            let mut reader = File::open(&src_path)?;
            self.stream = Some(Box::new(read_write_create(&tmp_path)?));
            self.write_ctx_mut().locator.quick_open_entries.clear();
            // Rewriting rediscovers header encryption from the file itself.
            self.header_encryption = false;
            self.archive_encr = None;

            let result = self.rewrite_blocks(
                &mut reader,
                &deleted,
                chain,
                force_rr,
                rename_map,
                comment,
                &src_path,
                &tmp_path,
            );
            self.stream = None;
            match result {
                Ok(()) => replace_file(&tmp_path, &src_path)?,
                Err(e) => {
                    let _ = fs::remove_file(&tmp_path);
                    return Err(e);
                }
            }
        }

        self.mode = Mode::Read;
        self.read_ctx_mut().solid_state = None;
        self.read_ctx_mut().solid_decoded_through = -1;
        self.open_read()?;
        Ok(())
    }

    /// Read the archive comment (the "CMT" service block for RAR5, the
    /// NEWSUB `CMT` block for RAR 1.5–4.x), if any.
    ///
    /// Header-encrypted archives store the comment encrypted; reading it
    /// requires the password and is not supported yet.
    pub fn get_comment(&mut self) -> RarResult<Option<Vec<u8>>> {
        if self.rar13 {
            return self.rar13_archive_comment();
        }
        if self.rar4 {
            return super::super::rar4_edit::read_comment(self);
        }
        let mut reader = File::open(&self.path)?;
        reader.seek(SeekFrom::Start(self.sfx_offset + 8))?;
        while let Some(meta) = crate::format::rar5::headers::read_block(
            &mut reader,
            self.archive_block_key()?.as_ref(),
        )? {
            match meta.block_type {
                BLOCK_TYPE_END_ARCHIVE => break,
                BLOCK_TYPE_SERVICE_HEADER
                    if self.service_block_name(&meta)?.as_deref() == Some("CMT") =>
                {
                    // The comment size comes from the service header, so it
                    // is capped before it can drive an allocation (a hand-made
                    // archive can declare any size and still pass the CRC) and
                    // narrowed with `try_from` so 32-bit targets report an
                    // error instead of silently truncating the comment.
                    let limit = self.read_ctx().extract_options.metadata_limit();
                    if meta.raw.data_size > limit {
                        return Err(RarError::LimitExceeded {
                            limit,
                            context: format!(
                                "archive comment declares {} bytes",
                                meta.raw.data_size
                            ),
                        });
                    }
                    let declared = usize::try_from(meta.raw.data_size).map_err(|_| {
                        RarError::LimitExceeded {
                            limit,
                            context: "archive comment size does not fit in usize".into(),
                        }
                    })?;
                    let mut data = vec![0u8; declared];
                    reader.seek(SeekFrom::Start(meta.data_offset))?;
                    reader.read_exact(&mut data)?;
                    return Ok(Some(data));
                }
                _ => {}
            }
            // Advance past the data area.
            reader.seek(SeekFrom::Start(meta.data_end))?;
        }
        Ok(None)
    }

    /// Parse the main archive header and report whether the archive is
    /// locked. Runs before any destructive step of [`Self::delete`] so the
    /// erase-everything path is covered too.
    pub(crate) fn main_header_is_locked(&mut self) -> RarResult<bool> {
        let mut reader = File::open(&self.path)?;
        reader.seek(SeekFrom::Start(self.sfx_offset + 8))?;
        self.header_encryption = false;
        self.archive_encr = None;
        let first = crate::format::rar5::headers::read_block(
            &mut reader,
            self.archive_block_key()?.as_ref(),
        )?
        .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
        let main = match first.block_type {
            BLOCK_TYPE_ENCRYPT_HEADER => {
                let params = parse_archive_encrypt_header(&first.raw)?;
                self.handle_archive_encrypt_header(params)?;
                crate::format::rar5::headers::read_block(
                    &mut reader,
                    self.archive_block_key()?.as_ref(),
                )?
                .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?
            }
            BLOCK_TYPE_ARCHIVE_HEADER => first,
            _ => {
                return Err(RarError::Format(
                    "archive is missing the main header".into(),
                ));
            }
        };
        let ah = ArchiveHeader::from_raw(&main.raw)?;
        Ok(ah.flags & ARCHIVE_FLAG_LOCKED != 0)
    }

    /// Index range `[s, e]` of the solid chain affected by deleting member
    /// `idx`, when one exists.
    ///
    /// A member joins its predecessor's window when its header carries the
    /// solid flag, so the chain extends backwards while the entry at the
    /// boundary is solid and forwards while the next entry is solid (the
    /// first member of a chain is not flagged solid, matching the writer).
    /// Deleting the last member of a chain leaves the earlier members
    /// decodable (their windows are untouched), so no chain needs to be
    /// recompressed in that case.
    fn chain_range_around(&self, idx: usize) -> Option<(usize, usize)> {
        let mut s = idx;
        while s > 0 && self.entries[s].header.comp_solid {
            s -= 1;
        }
        let mut e = idx;
        while e + 1 < self.entries.len() && self.entries[e + 1].header.comp_solid {
            e += 1;
        }
        (idx < e).then_some((s, e))
    }

    /// Rewrite the archive file, omitting deleted members.
    ///
    /// `reader` reads the original archive; the rewritten bytes go to
    /// `self.stream` (the replacement file). With the `parallel` feature,
    /// verbatim block data is prefetched by a background thread and the
    /// affected solid chain is recompressed while the tail is already
    /// being read. Inline recovery records are rebuilt when the original
    /// had one (the percentage is carried over).
    #[allow(clippy::too_many_arguments)] // mirrors the delete() state machine
    pub(crate) fn rewrite_blocks(
        &mut self,
        reader: &mut File,
        deleted: &[bool],
        chain: Option<(usize, usize)>,
        force_rr: Option<u8>,
        rename_map: Option<&std::collections::HashMap<usize, String>>,
        comment: Option<&[u8]>,
        src_path: &Path,
        tmp_path: &Path,
    ) -> RarResult<()> {
        let plan = self.plan_rewrite(reader, deleted, chain, force_rr, rename_map, comment)?;
        self.execute_rewrite(&plan, src_path, tmp_path)?;
        Ok(())
    }
}
