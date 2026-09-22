//! Typed, read-only archive API built on top of the legacy `RarArchive`
//! facade.
//!
//! [`ArchiveReader`] exposes only listing, reading, verification-adjacent and
//! extraction operations, so creation and mutation cannot be reached through
//! this role; use [`ArchiveWriter`](crate::ArchiveWriter) for creation/append
//! and [`ArchiveEditor`](crate::ArchiveEditor) for edits.

use std::fmt;
use std::io::Write;
use std::iter::FusedIterator;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{ArchiveEntry, ExtractionReport, RarArchive};
use crate::error::{RarError, RarResult};
use crate::options::ExtractOptions;

static NEXT_CATALOG_TOKEN: AtomicU64 = AtomicU64::new(1);

pub(crate) fn allocate_catalog_token() -> RarResult<u64> {
    NEXT_CATALOG_TOKEN
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current != 0).then(|| current.checked_add(1).unwrap_or(0))
        })
        .map_err(|_| RarError::InvalidState("archive reader ID space is exhausted".into()))
}

/// Controls how an [`ArchiveReader`] discovers archive entries while opening.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanStrategy {
    /// Scan the archive's blocks to build the complete entry catalog.
    #[default]
    Full,
    /// Prefer the RAR5 quick-open record and transparently fall back to a full
    /// scan when the record is unavailable or unusable.
    PreferQuickOpen,
}

/// Options used by [`ArchiveReader::open_with`].
///
/// Fields are private so new options can be added without breaking struct
/// literals. Configure values through the builder methods.
#[derive(Clone, Default)]
pub struct OpenOptions {
    password: Option<String>,
    scan_strategy: ScanStrategy,
}

impl OpenOptions {
    /// Create options using a full scan and no password.
    pub fn new() -> Self {
        Self::default()
    }

    /// Supply the password used to decrypt archive headers or member data.
    #[must_use]
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Remove a previously configured password.
    #[must_use]
    pub fn without_password(mut self) -> Self {
        self.password = None;
        self
    }

    /// Select how the entry catalog is discovered.
    #[must_use]
    pub fn scan_strategy(mut self, strategy: ScanStrategy) -> Self {
        self.scan_strategy = strategy;
        self
    }
}

impl fmt::Debug for OpenOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenOptions")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("scan_strategy", &self.scan_strategy)
            .finish()
    }
}

/// Opaque identity of one member in an [`ArchiveReader`]'s entry catalog.
///
/// IDs distinguish duplicate member names and carry the member's packed-
/// payload position, so a catalog rebuild that only reorders entries (the
/// quick-open rescan before extraction) still resolves an ID to the member
/// it names. They are scoped to the reader or editor that created them:
/// using an ID with another facade returns [`RarError::StaleEntryId`], even
/// when both opened the same file. An ID whose member is no longer present
/// in the catalog fails the same way instead of addressing whatever member
/// now sits at its old index.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId {
    catalog_token: u64,
    index: usize,
    /// Packed-payload offset of the member's first chunk when the ID was
    /// minted (`None` only for degenerate entries without data chunks).
    data_offset: Option<u64>,
}

impl fmt::Debug for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EntryId(..)")
    }
}

impl EntryId {
    /// Mint an ID for `entry` at `index` in the catalog identified by
    /// `catalog_token`.
    pub(crate) fn mint(catalog_token: u64, index: usize, entry: &ArchiveEntry) -> Self {
        EntryId {
            catalog_token,
            index,
            data_offset: entry.chunks.first().map(|chunk| chunk.data_offset),
        }
    }

    /// Resolve this ID against a catalog: the token must match, and the
    /// member is located by its minted index when the catalog kept that
    /// order, otherwise by its packed-payload offset — a quick-open rescan
    /// can reorder the same member set under one token, and the offset keeps
    /// the ID pointing at the member it names.
    ///
    /// This is the one identity convention every facade (reader, editor)
    /// applies, so stale/valid semantics cannot differ between them.
    pub(crate) fn resolve(self, entries: &[ArchiveEntry], catalog_token: u64) -> RarResult<usize> {
        if self.catalog_token != catalog_token {
            return Err(RarError::StaleEntryId);
        }
        let matches_member = |entry: &ArchiveEntry| {
            entry.chunks.first().map(|chunk| chunk.data_offset) == self.data_offset
        };
        if let Some(entry) = entries.get(self.index)
            && matches_member(entry)
        {
            return Ok(self.index);
        }
        entries
            .iter()
            .position(matches_member)
            .ok_or(RarError::StaleEntryId)
    }
}

/// An archive entry paired with its reader-scoped [`EntryId`].
#[derive(Clone, Copy)]
pub struct EntryRef<'a> {
    id: EntryId,
    entry: &'a ArchiveEntry,
}

impl<'a> EntryRef<'a> {
    pub(crate) fn new(id: EntryId, entry: &'a ArchiveEntry) -> Self {
        EntryRef { id, entry }
    }

    /// Return the reader-scoped identity of this entry.
    pub fn id(self) -> EntryId {
        self.id
    }

    /// Return the entry metadata exposed by the legacy [`ArchiveEntry`] API.
    pub fn metadata(self) -> &'a ArchiveEntry {
        self.entry
    }
}

impl fmt::Debug for EntryRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntryRef")
            .field("id", &self.id)
            .field("entry", self.entry)
            .finish()
    }
}

impl std::ops::Deref for EntryRef<'_> {
    type Target = ArchiveEntry;

    fn deref(&self) -> &Self::Target {
        self.entry
    }
}

/// Iterator over every member in archive order.
pub struct Entries<'a> {
    catalog_token: u64,
    entries: std::iter::Enumerate<std::slice::Iter<'a, ArchiveEntry>>,
}

impl<'a> Entries<'a> {
    pub(crate) fn new(catalog_token: u64, entries: &'a [ArchiveEntry]) -> Self {
        Entries {
            catalog_token,
            entries: entries.iter().enumerate(),
        }
    }
}

impl<'a> Iterator for Entries<'a> {
    type Item = EntryRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(|(index, entry)| EntryRef {
            id: EntryId::mint(self.catalog_token, index, entry),
            entry,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl DoubleEndedIterator for Entries<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.entries.next_back().map(|(index, entry)| EntryRef {
            id: EntryId::mint(self.catalog_token, index, entry),
            entry,
        })
    }
}

impl ExactSizeIterator for Entries<'_> {}
impl FusedIterator for Entries<'_> {}

/// Iterator over all entries whose stored name exactly matches a query.
///
/// Unlike the legacy name-based operations, this iterator does not collapse
/// duplicate names; each match carries its own [`EntryId`].
pub struct EntryMatches<'reader, 'query> {
    catalog_token: u64,
    name: &'query str,
    entries: std::iter::Enumerate<std::slice::Iter<'reader, ArchiveEntry>>,
}

impl<'reader, 'query> EntryMatches<'reader, 'query> {
    pub(crate) fn new(
        catalog_token: u64,
        name: &'query str,
        entries: &'reader [ArchiveEntry],
    ) -> Self {
        EntryMatches {
            catalog_token,
            name,
            entries: entries.iter().enumerate(),
        }
    }
}

impl<'reader> Iterator for EntryMatches<'reader, '_> {
    type Item = EntryRef<'reader>;

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.find_map(|(index, entry)| {
            (entry.name() == self.name).then_some(EntryRef {
                id: EntryId::mint(self.catalog_token, index, entry),
                entry,
            })
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, self.entries.size_hint().1)
    }
}

impl DoubleEndedIterator for EntryMatches<'_, '_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.entries
            .rfind(|(_, entry)| entry.name() == self.name)
            .map(|(index, entry)| EntryRef {
                id: EntryId::mint(self.catalog_token, index, entry),
                entry,
            })
    }
}

impl FusedIterator for EntryMatches<'_, '_> {}

/// One member that failed archive verification.
#[derive(Debug)]
pub struct VerificationFailure {
    entry_id: EntryId,
    error: RarError,
}

impl VerificationFailure {
    /// Return the identity of the member that failed verification.
    pub const fn entry_id(&self) -> EntryId {
        self.entry_id
    }

    /// Return the verification error for this member.
    pub const fn error(&self) -> &RarError {
        &self.error
    }

    /// Consume the failure and return its error.
    pub fn into_error(self) -> RarError {
        self.error
    }
}

/// Result of verifying every non-directory member in an archive.
#[derive(Debug)]
pub struct VerificationReport {
    checked: usize,
    failures: Vec<VerificationFailure>,
}

impl VerificationReport {
    /// Number of non-directory members checked.
    pub const fn checked(&self) -> usize {
        self.checked
    }

    /// Number of members that passed verification.
    pub fn passed(&self) -> usize {
        self.checked - self.failures.len()
    }

    /// Number of members that failed verification.
    pub fn failed(&self) -> usize {
        self.failures.len()
    }

    /// Whether every checked member passed verification.
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// Return per-member failures in archive order.
    pub fn failures(&self) -> &[VerificationFailure] {
        &self.failures
    }

    /// Consume the report and return its per-member failures.
    pub fn into_failures(self) -> Vec<VerificationFailure> {
        self.failures
    }
}

/// Read-only archive role with duplicate-safe member identities.
///
/// This type wraps the existing `RarArchive` implementation but deliberately
/// exposes no creation, append, rewrite or locking operations. IDs embed both
/// the catalog generation and the member's packed-payload position, so a
/// catalog rebuild that only reorders entries (a quick-open rescan before
/// extraction) still resolves every issued ID to the member it names; only
/// IDs from another catalog — or for members the rebuilt catalog no longer
/// contains — fail as [`RarError::StaleEntryId`].
pub struct ArchiveReader {
    archive: RarArchive,
}

impl std::fmt::Debug for ArchiveReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveReader")
            .field("path", &self.archive.path)
            .field("entries", &self.archive.entries.len())
            .finish_non_exhaustive()
    }
}

impl ArchiveReader {
    /// Open an archive with a full scan and no password.
    pub fn open(path: impl AsRef<Path>) -> RarResult<Self> {
        Self::open_with(path, OpenOptions::default())
    }

    /// Open an archive using explicit password and catalog scan options.
    pub fn open_with(path: impl AsRef<Path>, options: OpenOptions) -> RarResult<Self> {
        let OpenOptions {
            password,
            scan_strategy,
        } = options;
        let archive = match (scan_strategy, password.as_deref()) {
            (ScanStrategy::Full, None) => RarArchive::open(path)?,
            (ScanStrategy::Full, Some(password)) => RarArchive::open_with_password(path, password)?,
            (ScanStrategy::PreferQuickOpen, None) => RarArchive::open_quick(path)?,
            (ScanStrategy::PreferQuickOpen, Some(password)) => {
                RarArchive::open_quick_with_password(path, password)?
            }
        };
        Ok(Self { archive })
    }

    /// Crate-internal salvage open (see [`RarArchive::open_salvage`]): scan a
    /// damaged archive tolerating corrupt block headers. Used by
    /// `reconstruct_archive_path` when the strict scan fails.
    pub(crate) fn open_salvage(path: impl AsRef<Path>, password: Option<&str>) -> RarResult<Self> {
        Ok(Self {
            archive: RarArchive::open_salvage(path, password)?,
        })
    }

    /// Configure Mark of the Web propagation for subsequent extractions
    /// (WinRAR's `-om`); `None` disables it.
    ///
    /// The archive file's own `Zone.Identifier` stream is copied onto each
    /// extracted file (filtered to the security zone unless
    /// [`crate::options::MarkOfTheWeb::all_fields`] is set). The setting is
    /// a no-op on non-Windows platforms.
    pub fn set_mark_of_the_web(&mut self, options: Option<crate::options::MarkOfTheWeb>) {
        self.archive.read_ctx_mut().motw = options;
    }

    /// Install the interactive overwrite prompt. When
    /// [`ExtractOptions::prompt_overwrite`](crate::options::ExtractOptions::prompt_overwrite)
    /// is set, the extraction loop calls this for each existing destination
    /// and acts on the returned
    /// [`OverwriteChoice`](crate::options::OverwriteChoice). The library never
    /// reads the terminal itself, so a front end supplies the prompt (and any
    /// "all" state) here. `None` removes it (the default).
    pub fn set_overwrite_prompt(&mut self, prompt: Option<Arc<crate::options::OverwritePrompt>>) {
        self.archive.read_ctx_mut().overwrite_prompt = prompt;
    }

    /// Iterate over all entries in archive order.
    pub fn entries(&self) -> Entries<'_> {
        Entries::new(self.archive.catalog_token(), &self.archive.entries)
    }

    /// Whether a legacy volume set used the newer `.partN.rar` numbering
    /// (`MHD_NEWNUMBERING`); display-only.
    pub fn is_new_numbering(&self) -> bool {
        self.archive.read_ctx().legacy.new_numbering
    }

    /// Whether the catalog came from a salvage scan that had to resync past a
    /// corrupt block (used by `rar r`'s reconstruct fallback to report the
    /// loss).
    pub(crate) fn salvage_damaged(&self) -> bool {
        self.archive.read_ctx().salvage_damaged
    }

    /// Whether the archive is solid: the main header carries the
    /// archive-level solid flag (legacy `MHD_SOLID`) and/or a member
    /// continues a solid chain (`FHD_SOLID`/`LHD_SOLID`).
    pub fn is_solid(&self) -> bool {
        self.archive.archive_solid
            || self
                .archive
                .entries
                .iter()
                .any(|entry| entry.header.comp_solid)
    }

    /// Resolve an entry ID to metadata.
    ///
    /// Returns [`RarError::StaleEntryId`] when the ID came from another
    /// reader or no longer identifies an entry in this catalog.
    pub fn entry(&self, id: EntryId) -> RarResult<EntryRef<'_>> {
        let index = self.resolve_id(id)?;
        Ok(EntryRef {
            id,
            entry: &self.archive.entries[index],
        })
    }

    /// Iterate over every entry with the exact stored `name`.
    pub fn entries_named<'reader, 'query>(
        &'reader self,
        name: &'query str,
    ) -> EntryMatches<'reader, 'query> {
        EntryMatches::new(self.archive.catalog_token(), name, &self.archive.entries)
    }

    /// Resolve exactly one entry with the stored `name`.
    ///
    /// Missing names return [`RarError::MemberNotFound`]; duplicate names
    /// return [`RarError::AmbiguousMember`] with the number of matches.
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

    /// Read one member into memory with the safe default limits.
    pub fn read_entry(&mut self, id: EntryId) -> RarResult<Vec<u8>> {
        self.read_entry_with_options(id, ExtractOptions::default())
    }

    /// Read one member into memory using explicit extraction limits.
    pub fn read_entry_with_options(
        &mut self,
        id: EntryId,
        options: ExtractOptions,
    ) -> RarResult<Vec<u8>> {
        let index = self.resolve_id(id)?;
        self.archive.read_at_index_with_options(index, options)
    }

    /// Stream one member into `writer` with the safe default limits.
    pub fn copy_entry_to(&mut self, id: EntryId, writer: &mut dyn Write) -> RarResult<u64> {
        self.copy_entry_to_with_options(id, writer, ExtractOptions::default())
    }

    /// Stream one member into `writer` using explicit extraction limits.
    pub fn copy_entry_to_with_options(
        &mut self,
        id: EntryId,
        writer: &mut dyn Write,
        options: ExtractOptions,
    ) -> RarResult<u64> {
        let index = self.resolve_id(id)?;
        self.archive
            .read_to_writer_at_index_with_options(index, writer, options)
    }

    /// Extract one member to `destination` with safe default options.
    pub fn extract_entry(
        &mut self,
        id: EntryId,
        destination: impl AsRef<Path>,
    ) -> RarResult<PathBuf> {
        self.extract_entry_with_options(id, destination, ExtractOptions::default())
    }

    /// Extract one member to `destination` with explicit options.
    pub fn extract_entry_with_options(
        &mut self,
        id: EntryId,
        destination: impl AsRef<Path>,
        options: ExtractOptions,
    ) -> RarResult<PathBuf> {
        let index = self.resolve_id(id)?;
        self.archive
            .extract_at_index_with_options(index, destination, options)
    }

    /// Verify every non-directory member with safe default limits.
    ///
    /// Member-specific errors are retained in the returned report so callers
    /// can identify duplicate-name failures by ID. Cancellation aborts the
    /// operation immediately instead of being recorded as a member failure.
    pub fn verify(&mut self) -> RarResult<VerificationReport> {
        self.verify_with_options(ExtractOptions::default())
    }

    /// Verify every non-directory member using explicit extraction limits.
    pub fn verify_with_options(
        &mut self,
        options: ExtractOptions,
    ) -> RarResult<VerificationReport> {
        let mut ids = Vec::new();
        let mut total_unpacked = 0u64;
        for entry in self.entries().filter(|entry| !entry.is_dir()) {
            total_unpacked = total_unpacked.checked_add(entry.size()).ok_or_else(|| {
                RarError::LimitExceeded {
                    limit: options.max_total_unpacked_bytes.unwrap_or(u64::MAX),
                    context: "total unpacked size overflow while verifying archive".into(),
                }
            })?;
            if let Some(limit) = options.max_total_unpacked_bytes
                && total_unpacked > limit
            {
                return Err(RarError::LimitExceeded {
                    limit,
                    context: format!(
                        "total unpacked size {total_unpacked} exceeds limit while verifying {}",
                        entry.name()
                    ),
                });
            }
            ids.push(entry.id());
        }
        self.verify_ids_with_options(&ids, options)
    }

    /// Verify only the listed member IDs (used by filtered `t` runs).
    pub fn verify_ids_with_options(
        &mut self,
        ids: &[EntryId],
        options: ExtractOptions,
    ) -> RarResult<VerificationReport> {
        let mut total_unpacked = 0u64;
        for &id in ids {
            let entry = self.entry(id)?;
            total_unpacked = total_unpacked.checked_add(entry.size()).ok_or_else(|| {
                RarError::LimitExceeded {
                    limit: options.max_total_unpacked_bytes.unwrap_or(u64::MAX),
                    context: "total unpacked size overflow while verifying archive".into(),
                }
            })?;
            if let Some(limit) = options.max_total_unpacked_bytes
                && total_unpacked > limit
            {
                return Err(RarError::LimitExceeded {
                    limit,
                    context: format!(
                        "total unpacked size {total_unpacked} exceeds limit while verifying {}",
                        entry.name()
                    ),
                });
            }
        }

        let mut failures = Vec::new();
        let mut sink = std::io::sink();

        for &id in ids {
            if let Err(error) = self.copy_entry_to_with_options(id, &mut sink, options) {
                if matches!(error, RarError::Cancelled) {
                    return Err(error);
                }
                failures.push(VerificationFailure {
                    entry_id: id,
                    error,
                });
            }
        }

        Ok(VerificationReport {
            checked: ids.len(),
            failures,
        })
    }

    /// Read the archive-level comment (the `CMT` service block), if any.
    ///
    /// Returns the raw comment bytes as stored in the archive; `None` when
    /// the archive carries no comment.
    pub fn comment(&mut self) -> RarResult<Option<Vec<u8>>> {
        self.archive.get_comment()
    }

    /// Extract all archive entries with safe default options.
    pub fn extract_all(&mut self, destination: impl AsRef<Path>) -> RarResult<ExtractionReport> {
        self.archive
            .extract_all_with_options(destination, ExtractOptions::default())
    }

    /// Extract all archive entries with explicit options, returning what was
    /// written and what the skip-existing policy left untouched.
    pub fn extract_all_with_options(
        &mut self,
        destination: impl AsRef<Path>,
        options: ExtractOptions,
    ) -> RarResult<ExtractionReport> {
        self.archive.extract_all_with_options(destination, options)
    }

    /// Extract the listed member IDs, returning what was written and what the
    /// skip-existing policy left untouched. The CLI routes every `x`/`e` run
    /// through it, filtered or not.
    ///
    /// The listed members are checked against `max_total_unpacked_bytes`
    /// up front (before anything is written), unlike a whole-archive
    /// extraction, which checks the running total member by member — the
    /// limit is the same, the failure point is not.
    pub fn extract_ids_with_options(
        &mut self,
        ids: &[EntryId],
        destination: impl AsRef<Path>,
        options: ExtractOptions,
    ) -> RarResult<ExtractionReport> {
        let mut total_unpacked = 0u64;
        for &id in ids {
            let entry = self.entry(id)?;
            total_unpacked = total_unpacked.checked_add(entry.size()).ok_or_else(|| {
                RarError::LimitExceeded {
                    limit: options.max_total_unpacked_bytes.unwrap_or(u64::MAX),
                    context: "total unpacked size overflow".into(),
                }
            })?;
            if let Some(limit) = options.max_total_unpacked_bytes
                && total_unpacked > limit
            {
                return Err(RarError::LimitExceeded {
                    limit,
                    context: format!(
                        "total unpacked size {total_unpacked} exceeds limit while extracting {}",
                        entry.name()
                    ),
                });
            }
        }

        let destination = destination.as_ref();
        std::fs::create_dir_all(destination).map_err(RarError::Io)?;
        let indexes: Vec<usize> = ids
            .iter()
            .map(|&id| self.resolve_id(id))
            .collect::<RarResult<_>>()?;
        // The whole catalog in order is the common case (no name filter):
        // take the whole-archive path so its parallel heuristic applies.
        if indexes.len() == self.archive.entries.len()
            && indexes.iter().enumerate().all(|(i, &idx)| i == idx)
        {
            return self.archive.extract_all_with_options(destination, options);
        }

        let mut report = ExtractionReport::default();
        for &id in ids {
            // Resolve freshly per member: the first extraction can rebuild a
            // quick-open catalog, which may reorder the indexes.
            let index = self.resolve_id(id)?;
            self.archive
                .extract_index_with_options(index, destination, options, &mut report)?;
        }
        Ok(report)
    }

    /// Install or clear a caller-owned cooperative cancellation flag.
    pub fn set_cancel_flag(&mut self, flag: Option<Arc<AtomicBool>>) {
        self.archive.set_cancel_flag(flag);
    }

    fn resolve_id(&self, id: EntryId) -> RarResult<usize> {
        id.resolve(&self.archive.entries, self.archive.catalog_token())
    }
}
