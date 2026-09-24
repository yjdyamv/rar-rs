//! Edit entry points: planning, comment access and the locked check.

use super::*;

use std::fs::File;
use std::path::Path;

use std::io::{Read, Seek, SeekFrom};

use super::super::{Mode, RarArchive};
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::{
    BlockCursor, parse_service_block_name, parse_service_recovery_percent,
};
use crate::format::rar5::{ARCHIVE_FLAG_LOCKED, BLOCK_TYPE_END_ARCHIVE, BLOCK_TYPE_SERVICE_HEADER};

impl RarArchive {
    pub(crate) fn edit_plan(
        &mut self,
        delete_indexes: &[usize],
        renames: &[(usize, String)],
        force_rr: Option<u8>,
        comment: Option<&[u8]>,
    ) -> RarResult<EditSummary> {
        if self.mode != Mode::Read {
            return Err(RarError::format(
                "edit requires an archive opened for reading",
            ));
        }
        // A lone volume of a set (the other parts missing, so discovery
        // reports one path) must not be rewritten as if it were a
        // single-volume archive: the scan drops continuation fragments and
        // the block walk would then edit the wrong members or truncate
        // split ones. RAR4 refuses the same shape in its own edit entry
        // points.
        if self.volume_paths.len() <= 1 && self.main_header_declares_volume_set()? {
            return Err(RarError::unsupported(
                "cannot edit an incomplete multi-volume archive; open the first volume with every part present",
            ));
        }
        if force_rr.is_some_and(|percent| percent > 100) {
            return Err(RarError::invalid_option(
                "recovery percent must be in 0..=100",
            ));
        }
        if self.volume_paths.len() > 1 && comment.is_some() {
            return Err(RarError::unsupported(
                "comment changes are not supported for multi-volume archives",
            ));
        }
        self.ensure_write_ctx();
        if (!renames.is_empty() || force_rr.is_some() || comment.is_some())
            && self.main_header_is_locked()?
        {
            return Err(RarError::ArchiveLocked);
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
                return Err(RarError::invalid_option(
                    "cannot rename a member that the same edit deletes",
                ));
            }
        }

        let (map, renamed_count) = super::super::rename::build_rename_map(&self.entries, renames)?;
        if deleted_count == 0 && renamed_count == 0 && force_rr.is_none() && comment.is_none() {
            return Err(RarError::format("no members to edit"));
        }

        if deleted_count == self.entries.len() {
            // Matching `rar d`: deleting every member erases the archive
            // (every volume and `.rev` recovery volume for multi-volume
            // archives). Renames are empty here — the disjointness check
            // above leaves no target — and comment/recovery changes would be
            // silently dropped, so they are refused too.
            if force_rr.is_some() || comment.is_some() {
                return Err(RarError::invalid_option(
                    "cannot combine comment or recovery-record changes with deleting every member",
                ));
            }
            if self.main_header_is_locked()? {
                return Err(RarError::ArchiveLocked);
            }
            self.erase_archive_files()?;
            self.stream = None;
            self.entries.clear();
            self.read_ctx_mut().solid_state = None;
            self.read_ctx_mut().solid_decoded_through = -1;
            return Ok(EditSummary {
                deleted: deleted_count,
                renamed: 0,
            });
        }

        // Every affected solid chain, not only the one holding the lowest
        // deleted index: an archive can hold several disjoint chains (a STORE
        // fallback, a filter reset or an extension reset breaks one), and each
        // chain that loses a non-tail member must be recompressed. A deleted
        // member outside a single "first" chain would otherwise leave its
        // surviving solid successors copied verbatim, referencing a window
        // that is no longer in the output. `chain_range_around` returns the
        // same range for every member of one chain, so identical ranges are
        // deduplicated.
        let mut chains: Vec<(usize, usize)> = Vec::new();
        for (idx, &is_deleted) in deleted.iter().enumerate() {
            if is_deleted
                && let Some(range) = self.chain_range_around(idx)
                && !chains.contains(&range)
            {
                chains.push(range);
            }
        }
        chains.sort_unstable();
        // The engine applies renames to verbatim copies only; a kept member
        // of a recompressed chain would silently keep its old name. Refuse
        // that combination up front. Deleted members are irrelevant (they
        // are not emitted), so only kept members in a chain matter.
        for (start, end) in &chains {
            for idx in map.keys() {
                if !deleted[*idx] && (*start..=*end).contains(idx) {
                    return Err(RarError::unsupported(
                        "renaming a member of a solid chain that also loses a member is not supported; split the edit into separate transactions",
                    ));
                }
            }
        }

        self.rewrite_edit(deleted, &chains, &map, force_rr, comment)?;
        Ok(EditSummary {
            deleted: deleted_count,
            renamed: renamed_count,
        })
    }

    /// Remove every file of an archive whose members are all being deleted:
    /// the discovered data volumes plus the `.rev` recovery volumes sharing
    /// their base (multi-volume sets). The whole set is retired as one
    /// journaled commit, so a failure (a locked volume) rolls every removal
    /// back and the archive is never left half-erased.
    fn erase_archive_files(&self) -> RarResult<()> {
        let base = crate::fs::volume::volume_base_of(&self.path);
        let parent = crate::fs::atomic::parent_dir(&self.path);
        let mut victims = self.volume_paths.clone();
        if self.volume_paths.len() > 1 {
            victims.extend(crate::fs::volume::stale_volume_paths(
                &parent,
                &base,
                false,
                &self.volume_paths,
                &crate::recovery::rev3::rev_name_belongs_to_set,
            ));
        }
        // A directory (or other non-file) at a victim path is a conflict:
        // `commit_files` would park and retire it, then strand the parked
        // entry because only files are dropped on success.
        if let Some(conflict) = victims.iter().find(|path| path.exists() && !path.is_file()) {
            return Err(RarError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{}: refusing to erase a non-file entry", conflict.display()),
            )));
        }
        crate::fs::atomic::commit_files(&parent, &base, &[], &victims)
    }

    /// Set `recovery_percent` for a multi-volume rewrite: an explicit `-rr`
    /// wins, otherwise the strength the set's own per-volume records carry is
    /// preserved, so editing a protected set rebuilds its records instead of
    /// silently dropping them (the same advantage the single-volume path has;
    /// the official `rar` CLI drops the record unless `-rr` is repeated).
    fn carry_multivolume_recovery(&mut self, force_rr: Option<u8>) -> RarResult<()> {
        self.recovery_percent = match force_rr {
            Some(percent) => Some(percent),
            None => self.volume_recovery_percent()?,
        };
        Ok(())
    }

    /// The recovery percent carried by a multi-volume set's own records
    /// (WinRAR writes the same strength into every volume), or `None` when
    /// the set has no inline recovery record.
    fn volume_recovery_percent(&mut self) -> RarResult<Option<u8>> {
        let first = self
            .volume_paths
            .first()
            .cloned()
            .unwrap_or_else(|| self.path.clone());
        let mut reader = File::open(&first)?;
        // The scan needs the archive-level encryption state (a `-hp` set's
        // service headers are ciphertext), which a plain open does not cache.
        self.clear_archive_encryption();
        let _ = crate::format::rar5::extract::open::read_main_header(self, &mut reader)?;
        let file_len = reader.metadata().map_err(RarError::Io)?.len();
        let mut blocks = BlockCursor::new(
            file_len,
            crate::format::rar5::extract::verify::archive_block_key(self)?,
        );
        while let Some(meta) = blocks.next(&mut reader)? {
            match meta.block_type {
                BLOCK_TYPE_END_ARCHIVE => break,
                BLOCK_TYPE_SERVICE_HEADER
                    if parse_service_block_name(&meta.raw.header_data)?.as_deref()
                        == Some("RR") =>
                {
                    return Ok(parse_service_recovery_percent(&meta.raw.header_data));
                }
                _ => {}
            }
        }
        Ok(None)
    }

    /// Run one staged edit rewrite (delete mask + rename map) against the
    /// original archive and reload the catalog. Single-volume archives are
    /// rewritten through a sibling file that replaces the original only on
    /// success; multi-volume archives are re-split at their volume size and
    /// their .rev recovery volumes regenerated.
    fn rewrite_edit(
        &mut self,
        deleted: Vec<bool>,
        chains: &[(usize, usize)],
        map: &std::collections::HashMap<usize, String>,
        force_rr: Option<u8>,
        comment: Option<&[u8]>,
    ) -> RarResult<()> {
        let rename_map = (!map.is_empty()).then_some(map);
        // Comment changes are validated out of the multi-volume path above
        // (`rewrite_multivolume` cannot carry them).
        debug_assert!(
            self.volume_paths.len() <= 1 || comment.is_none(),
            "comment edits must be single-volume"
        );
        if self.volume_paths.len() > 1 {
            // Probe the main header before the multi-volume rewrite. The
            // probe derives header encryption (and the locked flag) from the
            // file; a delete-only plan would otherwise skip it, and
            // `rewrite_multivolume` would re-split the encrypted blocks
            // without re-encrypting them — silently corrupting the set.
            if self.main_header_is_locked()? {
                return Err(RarError::ArchiveLocked);
            }
            self.carry_multivolume_recovery(force_rr)?;
            self.rewrite_multivolume(&deleted, chains, rename_map)?;
        } else {
            let src_path = self.path.clone();
            let (mut staged, file) = crate::fs::atomic::StagedFile::create(&src_path)?;
            let tmp_path = staged.path().to_path_buf();
            let mut reader = File::open(&src_path)?;
            self.stream = Some(Box::new(file));
            self.write_ctx_mut().locator.quick_open_entries.clear();
            // Rewriting rediscovers header encryption from the file itself.
            self.clear_archive_encryption();

            let result = self.rewrite_blocks(
                &mut reader,
                &deleted,
                chains,
                force_rr,
                rename_map,
                comment,
                &src_path,
                &tmp_path,
            );
            // Close the write handle before installing the staged file.
            self.stream = None;
            result?;
            staged.commit()?;
        }

        self.mode = Mode::Read;
        self.read_ctx_mut().solid_state = None;
        self.read_ctx_mut().solid_decoded_through = -1;
        crate::format::shared::extract::open::open_read(self)?;
        Ok(())
    }

    /// Read the archive comment (the "CMT" service block for RAR5, the
    /// NEWSUB `CMT` block for RAR 1.5–4.x), if any.
    ///
    /// Header-encrypted archives store the comment encrypted; reading it
    /// requires the archive password, which the open path has already
    /// verified and cached (`archive_block_key`).
    pub fn get_comment(&mut self) -> RarResult<Option<Vec<u8>>> {
        if self.is_rar13() {
            return crate::format::rar13::extract::rar13_archive_comment(self);
        }
        if self.is_rar4() {
            return super::super::rar4_edit::read_comment(self);
        }
        // The comment sits in the first volume (WinRAR's placement), which is
        // not necessarily the part the caller opened.
        let first = self
            .volume_paths
            .first()
            .cloned()
            .unwrap_or_else(|| self.path.clone());
        let mut reader = File::open(&first)?;
        // Consume the optional plaintext encryption header (`-hp`) and the
        // main header in one place, deriving the archive-level encryption
        // state from the file: a plain open scans members with a per-volume
        // key and never populates it, so `self.header_encryption` cannot be
        // trusted here. The walk below then starts at the first member or
        // service block, decrypted with that key.
        self.clear_archive_encryption();
        let _ = crate::format::rar5::extract::open::read_main_header(self, &mut reader)?;
        let file_len = reader.metadata().map_err(RarError::Io)?.len();
        let mut blocks = BlockCursor::new(
            file_len,
            crate::format::rar5::extract::verify::archive_block_key(self)?,
        );
        while let Some(meta) = blocks.next(&mut reader)? {
            match meta.block_type {
                BLOCK_TYPE_END_ARCHIVE => break,
                BLOCK_TYPE_SERVICE_HEADER
                    if parse_service_block_name(&meta.raw.header_data)?.as_deref()
                        == Some("CMT") =>
                {
                    // The comment size comes from the service header, so it
                    // is capped before it can drive an allocation (a hand-made
                    // archive can declare any size and still pass the CRC) and
                    // narrowed with `try_from` so 32-bit targets report an
                    // error instead of silently truncating the comment.
                    let limit = self.read_ctx().extract_options.metadata_limit();
                    if meta.raw.data_size > limit {
                        return Err(RarError::limit_exceeded(
                            limit,
                            format!("archive comment declares {} bytes", meta.raw.data_size),
                        ));
                    }
                    let declared = usize::try_from(meta.raw.data_size).map_err(|_| {
                        RarError::limit_exceeded(
                            limit,
                            "archive comment size does not fit in usize",
                        )
                    })?;
                    let mut data = vec![0u8; declared];
                    reader.seek(SeekFrom::Start(meta.data_offset))?;
                    reader.read_exact(&mut data)?;
                    return Ok(Some(data));
                }
                _ => {}
            }
        }
        Ok(None)
    }

    /// Parse the main archive header and report whether the archive is
    /// locked. Runs before any destructive step of [`Self::delete`] so the
    /// erase-everything path is covered too.
    pub(crate) fn main_header_is_locked(&mut self) -> RarResult<bool> {
        let mut reader = File::open(&self.path)?;
        self.clear_archive_encryption();
        let main = crate::format::rar5::extract::open::read_main_header(self, &mut reader)?;
        Ok(main.parsed.flags & ARCHIVE_FLAG_LOCKED != 0)
    }

    /// Whether the main header marks the archive as part of a volume set.
    /// `volume_paths` alone cannot answer this: opening a lone middle part
    /// (its siblings missing) makes discovery report a single path.
    fn main_header_declares_volume_set(&mut self) -> RarResult<bool> {
        let mut reader = File::open(&self.path)?;
        self.clear_archive_encryption();
        let main = crate::format::rar5::extract::open::read_main_header(self, &mut reader)?;
        Ok(main.parsed.volume_number.is_some()
            || main.parsed.flags & crate::format::rar5::ARCHIVE_FLAG_VOLUME_NUM != 0)
    }

    /// Index range `[s, e]` of the solid chain affected by deleting member
    /// `idx`, when one exists.
    ///
    /// A member joins its predecessor's window when its header carries the
    /// solid flag, so the chain extends backwards to the first member that
    /// is not solid and forwards while the next entries are solid (the first
    /// member of a chain is not flagged solid, matching the writer).
    /// Directories are transparent: they carry no data and the reader's
    /// `find_solid_chain_start` walks across them in both directions, so a
    /// non-solid directory between two solid members is not a chain
    /// boundary. Deleting the last member of a chain leaves the earlier
    /// members decodable (their windows are untouched), so no chain needs to
    /// be recompressed in that case.
    pub(crate) fn chain_range_around(&self, idx: usize) -> Option<(usize, usize)> {
        // Anchor the walk at `idx` when it participates in a window, or at
        // the next non-directory when the edit removes a directory.
        let mut p = idx;
        while p < self.entries.len() && self.entries[p].is_dir() {
            p += 1;
        }
        if p >= self.entries.len() {
            return None;
        }
        let mut s = p;
        while self.entries[s].header.comp_solid {
            let Some(previous) = (0..s).rev().find(|&i| !self.entries[i].is_dir()) else {
                break;
            };
            s = previous;
        }
        let mut e = p;
        while let Some(next) = (e + 1..self.entries.len()).find(|&i| !self.entries[i].is_dir()) {
            if self.entries[next].header.comp_solid {
                e = next;
            } else {
                break;
            }
        }
        (idx < e).then_some((s, e))
    }

    /// Rewrite the archive file, omitting deleted members.
    ///
    /// `reader` reads the original archive; the rewritten bytes go to
    /// `self.stream` (the replacement file). With the `parallel` feature,
    /// verbatim block data is prefetched by a background thread and the
    /// affected solid chains are recompressed while the tail is already
    /// being read. Inline recovery records are rebuilt when the original
    /// had one (the percentage is carried over).
    #[allow(clippy::too_many_arguments)] // mirrors the delete() state machine
    pub(crate) fn rewrite_blocks(
        &mut self,
        reader: &mut File,
        deleted: &[bool],
        chains: &[(usize, usize)],
        force_rr: Option<u8>,
        rename_map: Option<&std::collections::HashMap<usize, String>>,
        comment: Option<&[u8]>,
        src_path: &Path,
        tmp_path: &Path,
    ) -> RarResult<()> {
        let plan = self.plan_rewrite(reader, deleted, chains, force_rr, rename_map, comment)?;
        self.execute_rewrite(&plan, src_path, tmp_path)?;
        Ok(())
    }
}
