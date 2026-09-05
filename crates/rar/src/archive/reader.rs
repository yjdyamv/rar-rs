//! Typed, read-only archive API built on top of the legacy [`RarArchive`]
//! facade.
//!
//! [`ArchiveReader`] exposes only listing, reading, verification-adjacent and
//! extraction operations. Archive creation and mutation remain available on
//! [`RarArchive`] for compatibility, but cannot be reached through this role.

use std::fmt;
use std::io::Write;
use std::iter::FusedIterator;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{ArchiveEntry, RarArchive};
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
/// IDs distinguish duplicate member names. They are scoped to the reader that
/// created them: using an ID with another reader returns
/// [`RarError::StaleEntryId`], even when both readers opened the same file.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId {
    catalog_token: u64,
    index: usize,
}

impl fmt::Debug for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EntryId(..)")
    }
}

impl EntryId {
    /// Whether this ID was minted for the catalog with `token`.
    pub(crate) const fn scoped_to(self, token: u64) -> bool {
        self.catalog_token == token
    }

    /// Position of the member in the catalog that minted this ID.
    pub(crate) const fn catalog_index(self) -> usize {
        self.index
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
            id: EntryId {
                catalog_token: self.catalog_token,
                index,
            },
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
            id: EntryId {
                catalog_token: self.catalog_token,
                index,
            },
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
                id: EntryId {
                    catalog_token: self.catalog_token,
                    index,
                },
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
                id: EntryId {
                    catalog_token: self.catalog_token,
                    index,
                },
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
/// This type wraps the existing [`RarArchive`] implementation but deliberately
/// exposes no creation, append, rewrite or locking operations.
pub struct ArchiveReader {
    archive: RarArchive,
    catalog_token: u64,
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
        Ok(Self {
            archive,
            catalog_token: allocate_catalog_token()?,
        })
    }

    /// Iterate over all entries in archive order.
    pub fn entries(&self) -> Entries<'_> {
        Entries {
            catalog_token: self.catalog_token,
            entries: self.archive.entries.iter().enumerate(),
        }
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
        EntryMatches {
            catalog_token: self.catalog_token,
            name,
            entries: self.archive.entries.iter().enumerate(),
        }
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

        let mut failures = Vec::new();
        let mut sink = std::io::sink();

        for id in ids.iter().copied() {
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

    /// Extract all archive entries with safe default options.
    pub fn extract_all(&mut self, destination: impl AsRef<Path>) -> RarResult<()> {
        self.archive.extract_all(destination)
    }

    /// Extract all archive entries with explicit options.
    pub fn extract_all_with_options(
        &mut self,
        destination: impl AsRef<Path>,
        options: ExtractOptions,
    ) -> RarResult<()> {
        self.archive.extract_all_with_options(destination, options)
    }

    /// Install or clear a caller-owned cooperative cancellation flag.
    pub fn set_cancel_flag(&mut self, flag: Option<Arc<AtomicBool>>) {
        self.archive.set_cancel_flag(flag);
    }

    fn resolve_id(&self, id: EntryId) -> RarResult<usize> {
        if id.catalog_token != self.catalog_token || id.index >= self.archive.entries.len() {
            return Err(RarError::StaleEntryId);
        }
        Ok(id.index)
    }
}
