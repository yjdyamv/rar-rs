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
//! Edit *planning* (combining several delete/rename/comment/recovery
//! changes in one rewrite) is a later step; each call here is one
//! transaction.

use std::collections::HashSet;
use std::path::Path;

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

    /// Delete the members identified by `ids` (like `rar d`); returns the
    /// number deleted.
    ///
    /// Exactly the listed members are removed — a directory member does not
    /// pull in its children. Deleting every member erases the archive file,
    /// matching `rar d`/the legacy delete. On success every previously
    /// issued ID becomes stale; on failure the archive and the catalog
    /// generation are untouched.
    ///
    /// Locked archives: like the legacy path, renaming and erasing a locked
    /// archive is refused with [`RarError::ArchiveLocked`]; deleting a
    /// subset of a locked archive is permitted.
    ///
    /// RAR4 (legacy-container) archives are refused with
    /// [`RarError::Unsupported`]: the rewrite engine is RAR5-only.
    pub fn delete_entries(&mut self, ids: &[EntryId]) -> RarResult<usize> {
        self.ensure_rewritable()?;
        let indexes = self.resolve_ids(ids)?;
        let count = self.archive.delete_indexes(&indexes)?;
        self.catalog_token = allocate_catalog_token()?;
        Ok(count)
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
        self.ensure_rewritable()?;
        let pairs: Vec<(usize, String)> = renames
            .iter()
            .map(|(id, new_name)| Ok((self.resolve_id(*id)?, new_name.clone())))
            .collect::<RarResult<_>>()?;
        let count = self.archive.rename_indexes(&pairs)?;
        self.catalog_token = allocate_catalog_token()?;
        Ok(count)
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

    /// Resolve every ID, rejecting stale ones before any edit starts and
    /// deduplicating repeated IDs (a member can only be deleted once).
    fn resolve_ids(&self, ids: &[EntryId]) -> RarResult<Vec<usize>> {
        let mut indexes = Vec::with_capacity(ids.len());
        let mut seen = HashSet::with_capacity(ids.len());
        for &id in ids {
            let index = self.resolve_id(id)?;
            if seen.insert(index) {
                indexes.push(index);
            }
        }
        Ok(indexes)
    }

    fn resolve_id(&self, id: EntryId) -> RarResult<usize> {
        if !id.scoped_to(self.catalog_token) || id.catalog_index() >= self.archive.entries.len() {
            return Err(RarError::StaleEntryId);
        }
        Ok(id.catalog_index())
    }
}
