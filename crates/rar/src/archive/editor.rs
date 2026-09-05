//! Typed, mutation-capable archive API built on the legacy [`RarArchive`]
//! facade and its index-based rewrite core.
//!
//! [`ArchiveEditor`] exposes a duplicate-safe entry catalog (the same ID
//! scheme as the read-only role) plus structural edits — delete and rename —
//! addressed by catalog ID instead of member name.
//!
//! Transaction model: every edit performs one atomic rewrite of the archive
//! file (staged in a sibling file, moved over the original only on success),
//! exactly like the legacy name-based operations. A failed rewrite leaves
//! the original untouched. After a *successful* edit the catalog is
//! re-scanned and its generation changes, so every ID issued before the
//! edit becomes stale ([`crate::RarError::StaleEntryId`]).
//!
//! An [`EditPlan`] combines several delete/rename/comment/recovery changes
//! in one rewrite; the single-op convenience methods (`delete_entries`,
//! `rename_entries`) are thin wrappers around it.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::RarArchive;
use super::reader::{Entries, EntryId, EntryMatches, EntryRef, allocate_catalog_token};
use crate::error::{RarError, RarResult};

/// Mutable archive role with duplicate-safe entry identities.
///
/// This type wraps the existing [`RarArchive`] implementation but exposes
/// only catalog listing and ID-based structural edits. Open the archive,
/// resolve members to [`EntryId`]s through the catalog, then delete or
/// rename them; IDs issued before an edit fail with
/// [`RarError::StaleEntryId`] afterwards.
pub struct ArchiveEditor {
    archive: RarArchive,
    catalog_token: u64,
}

impl ArchiveEditor {
    /// Open an archive for editing with a full scan and no password.
    pub fn open(path: impl AsRef<Path>) -> RarResult<Self> {
        Self::open_with_password(path, "")
    }

    /// Open an archive for editing with a password (needed for header-
    /// encrypted archives; unused for plain ones).
    pub fn open_with_password(path: impl AsRef<Path>, password: &str) -> RarResult<Self> {
        let archive = if password.is_empty() {
            RarArchive::open(path)?
        } else {
            RarArchive::open_with_password(path, password)?
        };
        Ok(Self {
            archive,
            catalog_token: allocate_catalog_token()?,
        })
    }

    /// Iterate over all members in archive order.
    pub fn entries(&self) -> Entries<'_> {
        Entries::new(self.catalog_token, &self.archive.entries)
    }

    /// Resolve an entry ID to metadata.
    ///
    /// Returns [`RarError::StaleEntryId`] when the ID came from another
    /// reader/editor, or was issued before the last successful edit.
    pub fn entry(&self, id: EntryId) -> RarResult<EntryRef<'_>> {
        let index = self.resolve_id(id)?;
        Ok(EntryRef::new(id, &self.archive.entries[index]))
    }

    /// Iterate over every member with the exact stored `name`.
    pub fn entries_named<'editor, 'query>(
        &'editor self,
        name: &'query str,
    ) -> EntryMatches<'editor, 'query> {
        EntryMatches::new(self.catalog_token, name, &self.archive.entries)
    }

    /// Resolve exactly one member with the stored `name`.
    ///
    /// Missing names return [`RarError::MemberNotFound`]; duplicate names
    /// return [`RarError::AmbiguousMember`] with the number of matches.
    /// Use [`Self::entries_named`] to pick a duplicate by ID.
    pub fn unique_entry(&self, name: &str) -> RarResult<EntryId> {
        let mut matches = self.entries_named(name);
        let first = matches.next().ok_or_else(|| RarError::MemberNotFound {
            name: name.to_string(),
        })?;
        let additional = matches.count();
        if additional != 0 {
            return Err(RarError::AmbiguousMember {
                name: name.to_string(),
                matches: additional + 1,
            });
        }
        Ok(first.id())
    }
}

/// One structural edit in an [`EditPlan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditOp {
    /// Delete the member identified by the ID (like `rar d`).
    Delete(EntryId),
    /// Rename the member identified by the ID (like `rar rn`); a directory
    /// rename is expanded to its descendants.
    Rename(EntryId, String),
    /// Replace the archive comment (like `rar c`); empty bytes remove the
    /// existing comment. Only valid on single-volume archives.
    SetComment(Vec<u8>),
    /// Rebuild the inline recovery record protecting this percent of the
    /// archive (like `rar rr`; 0..=100). Only valid on single-volume
    /// archives. Without this op the record is rebuilt at its original
    /// percentage when the archive had one.
    SetRecovery(u8),
}

/// A sequence of structural edits applied to an [`ArchiveEditor`] in one
/// atomic rewrite transaction.
///
/// Ops are validated against the catalog before anything is written: stale
/// IDs, renaming a member the same plan deletes, and renaming a member of
/// a solid chain that also loses a member are all rejected up front, so a
/// failed [`ArchiveEditor::apply`] leaves every original file untouched.
#[derive(Clone, Debug, Default)]
pub struct EditPlan {
    ops: Vec<EditOp>,
}

impl EditPlan {
    /// Create an empty plan.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a delete of the member identified by `id`.
    #[must_use]
    pub fn delete(mut self, id: EntryId) -> Self {
        self.ops.push(EditOp::Delete(id));
        self
    }

    /// Queue a rename of the member identified by `id`.
    #[must_use]
    pub fn rename(mut self, id: EntryId, new_name: impl Into<String>) -> Self {
        self.ops.push(EditOp::Rename(id, new_name.into()));
        self
    }

    /// Queue an archive-comment replacement; empty bytes remove the
    /// existing comment (like `rar c`).
    #[must_use]
    pub fn set_comment(mut self, comment: impl Into<Vec<u8>>) -> Self {
        self.ops.push(EditOp::SetComment(comment.into()));
        self
    }

    /// Queue an inline recovery-record rebuild at `percent` (0..=100,
    /// like `rar rr`).
    #[must_use]
    pub fn set_recovery(mut self, percent: u8) -> Self {
        self.ops.push(EditOp::SetRecovery(percent));
        self
    }

    /// Number of queued operations.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether the plan holds no operations.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// The queued operations in order.
    pub fn ops(&self) -> &[EditOp] {
        &self.ops
    }
}

/// Outcome of one applied [`EditPlan`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditReport {
    deleted: usize,
    renamed: usize,
}

impl EditReport {
    /// Number of members deleted.
    pub const fn deleted(&self) -> usize {
        self.deleted
    }

    /// Number of members renamed (explicit rename ops; directory-expanded
    /// descendants are not counted).
    pub const fn renamed(&self) -> usize {
        self.renamed
    }

    /// Whether the plan changed nothing.
    pub const fn is_empty(&self) -> bool {
        self.deleted == 0 && self.renamed == 0
    }
}

impl ArchiveEditor {
    /// Apply an [`EditPlan`] as one atomic rewrite transaction.
    ///
    /// All operations share a single staged rewrite that replaces the
    /// original only when every part succeeds, so a failed plan (a stale
    /// ID, a member both deleted and renamed, or a solid-chain conflict)
    /// leaves every original file — and the catalog generation — untouched.
    /// On success the catalog is re-scanned and the generation changes,
    /// making every previously issued ID stale.
    ///
    /// Locked archives fail with [`RarError::ArchiveLocked`] (any rewrite
    /// rewrites the main header, which refuses locked archives). RAR4
    /// archives are refused with [`RarError::Unsupported`].
    pub fn apply(&mut self, plan: EditPlan) -> RarResult<EditReport> {
        self.ensure_rewritable()?;
        // Resolve every operation against the current catalog before any
        // rewrite starts; a stale ID fails the whole plan up front.
        let mut deletes = Vec::with_capacity(plan.ops.len());
        let mut renames = Vec::with_capacity(plan.ops.len());
        let mut comment: Option<Vec<u8>> = None;
        let mut force_rr: Option<u8> = None;
        for op in &plan.ops {
            match op {
                EditOp::Delete(id) => deletes.push(self.resolve_id(*id)?),
                EditOp::Rename(id, new_name) => {
                    renames.push((self.resolve_id(*id)?, new_name.clone()))
                }
                EditOp::SetComment(bytes) => {
                    if comment.is_some() {
                        return Err(RarError::InvalidOption(
                            "an edit plan can carry only one comment change".into(),
                        ));
                    }
                    comment = Some(bytes.clone());
                }
                EditOp::SetRecovery(percent) => {
                    if force_rr.is_some() {
                        return Err(RarError::InvalidOption(
                            "an edit plan can carry only one recovery-record change".into(),
                        ));
                    }
                    force_rr = Some(*percent);
                }
            }
        }
        let summary = self
            .archive
            .edit_plan(&deletes, &renames, force_rr, comment.as_deref())?;
        self.catalog_token = allocate_catalog_token()?;
        Ok(EditReport {
            deleted: summary.deleted,
            renamed: summary.renamed,
        })
    }

    /// Delete the members identified by `ids` (like `rar d`); returns the
    /// number deleted.
    ///
    /// Exactly the listed members are removed — a directory member does not
    /// pull in its children. Deleting every member erases the archive file,
    /// matching `rar d`/the legacy delete. On success every previously
    /// issued ID becomes stale; on failure the archive and the catalog
    /// generation are untouched. Locked archives fail with
    /// [`RarError::ArchiveLocked`].
    ///
    /// RAR4 (legacy-container) archives are refused with
    /// [`RarError::Unsupported`]: the rewrite engine is RAR5-only.
    pub fn delete_entries(&mut self, ids: &[EntryId]) -> RarResult<usize> {
        let mut plan = EditPlan::new();
        for &id in ids {
            plan = plan.delete(id);
        }
        Ok(self.apply(plan)?.deleted)
    }

    /// Rename the members identified by `ids` (like `rar rn`); returns the
    /// number renamed.
    ///
    /// A directory rename is expanded to its descendants, exactly like the
    /// legacy name-based rename. Renaming a member onto a name that another
    /// member already has is allowed (duplicate names are first-class). On
    /// success every previously issued ID becomes stale.
    ///
    /// RAR4 (legacy-container) archives are refused with
    /// [`RarError::Unsupported`]: the rewrite engine is RAR5-only.
    pub fn rename_entries(&mut self, renames: &[(EntryId, String)]) -> RarResult<usize> {
        let mut plan = EditPlan::new();
        for (id, new_name) in renames {
            plan = plan.rename(*id, new_name.clone());
        }
        Ok(self.apply(plan)?.renamed)
    }

    /// Delete the members identified by `ids`, reporting rewrite progress
    /// through `progress` (`(done, total)`, monotonic, covering the whole
    /// rewrite and reaching `total` on success).
    pub fn delete_entries_with_progress(
        &mut self,
        ids: &[EntryId],
        progress: Option<Box<dyn FnMut(u64, u64) + Send>>,
    ) -> RarResult<usize> {
        let saved = self.archive.progress.take();
        self.archive.progress_member = 0;
        if let Some(cb) = progress {
            // The total is left unset: the rewrite loop reports the exact
            // work byte count (kept payloads + recompressed input), which
            // the tracker adopts as its denominator.
            let tracker = crate::write_progress::ProgressTracker::new(Some(cb));
            self.archive.progress = Some(std::sync::Arc::new(std::sync::Mutex::new(tracker)));
        }
        let result = self.delete_entries(ids);
        self.archive.progress = saved;
        result
    }

    /// Install or clear a caller-owned cooperative cancellation flag
    /// (checked between rewrite chunks and members).
    pub fn set_cancel_flag(&mut self, flag: Option<Arc<AtomicBool>>) {
        self.archive.set_cancel_flag(flag);
    }

    /// Lock the archive (like `rar k`): sets the `LOCKED` flag in the main
    /// archive header, making the archive read-only. Locking is
    /// irreversible. Multi-volume archives are refused with
    /// [`RarError::Unsupported`] (lock the first volume instead), matching
    /// the legacy method. Editing an already-locked archive fails with
    /// [`RarError::ArchiveLocked`] on the next [`Self::apply`].
    #[allow(deprecated)] // role seam: delegates to the legacy in-place patch
    pub fn lock(&mut self) -> RarResult<()> {
        self.archive.lock()
    }

    /// The surgical rewrite engine behind every edit operates on RAR5
    /// blocks; refuse legacy-container archives up front with a clear error
    /// instead of a confusing parse failure mid-rewrite.
    fn ensure_rewritable(&self) -> RarResult<()> {
        if self.archive.rar4 {
            return Err(RarError::Unsupported(
                "editing RAR4 archives is not supported; the rewrite engine is RAR5-only".into(),
            ));
        }
        Ok(())
    }

    fn resolve_id(&self, id: EntryId) -> RarResult<usize> {
        if !id.scoped_to(self.catalog_token) || id.catalog_index() >= self.archive.entries.len() {
            return Err(RarError::StaleEntryId);
        }
        Ok(id.catalog_index())
    }
}
