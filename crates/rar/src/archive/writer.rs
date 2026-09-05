//! Typed, transactional archive writer built on the legacy [`RarArchive`]
//! implementation.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::{BatchEntry, RarArchive};
use crate::error::{RarError, RarResult};
use crate::options::{CreateOptions, SolidReset};
use crate::version::ArchiveVersion;

const MIN_DICTIONARY_BYTES: u64 = 128 * 1024;
const MAX_RAR5_DICTIONARY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const DEFAULT_RAR70_DICTIONARY_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DICTIONARY_BYTES: u64 = 126 * 1024 * 1024 * 1024;
const MAX_THREADS: usize = 64;

/// A validated archive-member compression level in the range `0..=5`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CompressionLevel(u8);

impl CompressionLevel {
    /// Store input without compression.
    pub const STORE: Self = Self(0);
    /// Fastest compression.
    pub const FASTEST: Self = Self(1);
    /// Fast compression.
    pub const FAST: Self = Self(2);
    /// Normal compression, used by default.
    pub const NORMAL: Self = Self(3);
    /// Good compression.
    pub const GOOD: Self = Self(4);
    /// Best compression.
    pub const BEST: Self = Self(5);

    /// Return the numeric compression level.
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for CompressionLevel {
    fn default() -> Self {
        Self::NORMAL
    }
}

impl TryFrom<u8> for CompressionLevel {
    type Error = RarError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value <= Self::BEST.get() {
            Ok(Self(value))
        } else {
            Err(RarError::InvalidOption(format!(
                "compression level must be in 0..=5, got {value}"
            )))
        }
    }
}

/// A validated dictionary size accepted by the RAR5 and RAR7 writers.
///
/// Sizes from 128 KiB through 4 GiB must be powers of two and have a RAR5
/// dictionary log. Larger RAR7 sizes may use any byte count through 126 GiB.
///
/// A size above 4 GiB selects RAR7 (v70) members. On a [`WriterOptions`]
/// with [`ArchiveVersion::Rar50`] that selection is automatic, like
/// WinRAR's `-md`: the request is capped at twice the member size, so
/// small members stay plain v50 and only members whose effective
/// dictionary exceeds 4 GiB are written as v70. Use
/// [`ArchiveVersion::Rar70`] to force v70 members for every member.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DictionarySize(u64);

impl DictionarySize {
    /// Smallest supported dictionary size (128 KiB).
    pub const MIN: Self = Self(MIN_DICTIONARY_BYTES);
    /// Default dictionary requested for RAR5 and RAR7 creation (32 MiB).
    pub const DEFAULT: Self = Self(DEFAULT_RAR70_DICTIONARY_BYTES);
    /// Largest supported dictionary size (126 GiB).
    pub const MAX: Self = Self(MAX_DICTIONARY_BYTES);

    /// Construct a dictionary size from a RAR5 log (`128 KiB << log`).
    pub fn from_rar5_log(log: u8) -> RarResult<Self> {
        if log > 15 {
            return Err(RarError::InvalidOption(format!(
                "RAR5 dictionary log must be in 0..=15, got {log}"
            )));
        }
        Ok(Self(MIN_DICTIONARY_BYTES << log))
    }

    /// Return the dictionary size in bytes.
    pub const fn bytes(self) -> u64 {
        self.0
    }

    /// Return the RAR5 dictionary log, or `None` for a RAR7-only size.
    pub const fn rar5_log(self) -> Option<u8> {
        if self.0 <= MAX_RAR5_DICTIONARY_BYTES {
            Some((self.0.trailing_zeros() - MIN_DICTIONARY_BYTES.trailing_zeros()) as u8)
        } else {
            None
        }
    }
}

impl TryFrom<u64> for DictionarySize {
    type Error = RarError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if !(MIN_DICTIONARY_BYTES..=MAX_DICTIONARY_BYTES).contains(&value) {
            return Err(RarError::InvalidOption(format!(
                "dictionary size must be in {MIN_DICTIONARY_BYTES}..={MAX_DICTIONARY_BYTES} bytes, got {value}"
            )));
        }
        if value <= MAX_RAR5_DICTIONARY_BYTES && !value.is_power_of_two() {
            return Err(RarError::InvalidOption(format!(
                "dictionary sizes through 4 GiB must be powers of two, got {value} bytes"
            )));
        }
        Ok(Self(value))
    }
}

/// A validated per-archive compression thread count in the range `0..=64`.
///
/// Only consulted when the `parallel` feature is enabled; without it
/// compression stays sequential regardless of this value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadCount(usize);

impl ThreadCount {
    /// Select automatic worker-pool sizing.
    pub const AUTOMATIC: Self = Self(0);

    /// Return the configured thread count (`0` means automatic).
    pub const fn get(self) -> usize {
        self.0
    }
}

impl TryFrom<usize> for ThreadCount {
    type Error = RarError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        if value <= MAX_THREADS {
            Ok(Self(value))
        } else {
            Err(RarError::InvalidOption(format!(
                "compression threads must be in 0..={MAX_THREADS}, got {value}"
            )))
        }
    }
}

/// Controls whether and where solid compression chains are reset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SolidMode {
    /// Do not share compression state between members.
    #[default]
    Disabled,
    /// Keep one solid chain across the archive, including volume boundaries.
    Continuous,
    /// Reset the solid chain at every volume boundary.
    PerVolume,
    /// Reset the solid chain when the next member's extension changes.
    PerExtension,
}

/// Options used by [`ArchiveWriter::create_with`].
///
/// Fields are private to keep this additive API extensible. Builder calls do
/// not perform I/O; all cross-field validation runs before staging is opened,
/// and no validated combination is silently downgraded by the writer.
#[derive(Clone)]
pub struct WriterOptions {
    format_version: ArchiveVersion,
    solid_mode: SolidMode,
    quick_open: bool,
    blake2: bool,
    password: Option<String>,
    encrypt_headers: bool,
    recovery_percent: Option<u8>,
    recovery_volumes_percent: Option<u8>,
    recovery_volume_count: Option<u32>,
    volume_size: Option<u64>,
    dictionary_size: Option<DictionarySize>,
    save_ctime: bool,
    save_atime: bool,
    time_precision_seconds: bool,
    save_mtime: bool,
    save_owner: bool,
    save_streams: bool,
    thread_count: Option<ThreadCount>,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            format_version: ArchiveVersion::Rar50,
            solid_mode: SolidMode::Disabled,
            quick_open: false,
            blake2: false,
            password: None,
            encrypt_headers: false,
            recovery_percent: None,
            recovery_volumes_percent: None,
            recovery_volume_count: None,
            volume_size: None,
            dictionary_size: None,
            save_ctime: false,
            save_atime: false,
            time_precision_seconds: false,
            save_mtime: true,
            save_owner: false,
            save_streams: false,
            thread_count: None,
        }
    }
}

impl WriterOptions {
    /// Create default RAR5 writer options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Select the archive/container and codec version.
    #[must_use]
    pub fn format_version(mut self, version: ArchiveVersion) -> Self {
        self.format_version = version;
        self
    }

    /// Select solid compression behavior.
    #[must_use]
    pub fn solid_mode(mut self, mode: SolidMode) -> Self {
        self.solid_mode = mode;
        self
    }

    /// Enable or disable the RAR5 quick-open record.
    #[must_use]
    pub fn quick_open(mut self, enabled: bool) -> Self {
        self.quick_open = enabled;
        self
    }

    /// Enable or disable BLAKE2sp member hashes.
    #[must_use]
    pub fn blake2(mut self, enabled: bool) -> Self {
        self.blake2 = enabled;
        self
    }

    /// Set the password used for member and optional header encryption.
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

    /// Enable or disable archive-header encryption.
    #[must_use]
    pub fn encrypt_headers(mut self, enabled: bool) -> Self {
        self.encrypt_headers = enabled;
        self
    }

    /// Add an inline recovery record using the given percentage.
    #[must_use]
    pub fn recovery_percent(mut self, percent: u8) -> Self {
        self.recovery_percent = Some(percent);
        self
    }

    /// Create recovery volumes using a percentage of the data-volume count.
    #[must_use]
    pub fn recovery_volumes_percent(mut self, percent: u8) -> Self {
        self.recovery_volumes_percent = Some(percent);
        self
    }

    /// Create an exact number of recovery volumes.
    #[must_use]
    pub fn recovery_volume_count(mut self, count: u32) -> Self {
        self.recovery_volume_count = Some(count);
        self
    }

    /// Split output into data volumes of at most `bytes` bytes.
    #[must_use]
    pub fn volume_size(mut self, bytes: u64) -> Self {
        self.volume_size = Some(bytes);
        self
    }

    /// Set the requested compression dictionary size. On `Rar50` a size
    /// above 4 GiB keeps the auto v50/v70 semantics (see [`DictionarySize`]);
    /// on `Rar70` every member is v70 with this size (32 MiB when unset).
    #[must_use]
    pub fn dictionary_size(mut self, size: DictionarySize) -> Self {
        self.dictionary_size = Some(size);
        self
    }

    /// Save creation/change timestamps in member metadata.
    #[must_use]
    pub fn save_ctime(mut self, enabled: bool) -> Self {
        self.save_ctime = enabled;
        self
    }

    /// Save access timestamps in member metadata.
    #[must_use]
    pub fn save_atime(mut self, enabled: bool) -> Self {
        self.save_atime = enabled;
        self
    }

    /// Store timestamps at one-second precision when enabled.
    #[must_use]
    pub fn time_precision_seconds(mut self, enabled: bool) -> Self {
        self.time_precision_seconds = enabled;
        self
    }

    /// Save modification timestamps in member metadata.
    #[must_use]
    pub fn save_mtime(mut self, enabled: bool) -> Self {
        self.save_mtime = enabled;
        self
    }

    /// Save Unix owner/group metadata when supported.
    #[must_use]
    pub fn save_owner(mut self, enabled: bool) -> Self {
        self.save_owner = enabled;
        self
    }

    /// Save NTFS alternate data streams when supported.
    #[must_use]
    pub fn save_streams(mut self, enabled: bool) -> Self {
        self.save_streams = enabled;
        self
    }

    /// Set a per-archive compression thread count (requires the `parallel`
    /// feature; otherwise compression stays sequential).
    #[must_use]
    pub fn thread_count(mut self, count: ThreadCount) -> Self {
        self.thread_count = Some(count);
        self
    }

    fn validate(&self) -> RarResult<()> {
        // The legacy writer silently skips quick-open for header-encrypted or
        // multi-volume archives; a validated typed option must never be
        // silently dropped, so those combinations are rejected up front.
        if self.quick_open && self.encrypt_headers {
            return Err(RarError::InvalidOption(
                "quick-open cannot be combined with header encryption".into(),
            ));
        }
        if self.quick_open && self.volume_size.is_some() {
            return Err(RarError::InvalidOption(
                "quick-open cannot be combined with data volumes".into(),
            ));
        }
        for (name, percent) in [
            ("recovery percent", self.recovery_percent),
            ("recovery-volume percent", self.recovery_volumes_percent),
        ] {
            if percent.is_some_and(|value| value > 100) {
                return Err(RarError::InvalidOption(format!(
                    "{name} must be in 0..=100"
                )));
            }
        }
        if self.volume_size == Some(0) {
            return Err(RarError::InvalidOption(
                "volume size must be greater than zero".into(),
            ));
        }
        if self.encrypt_headers && self.password.as_deref().is_none_or(str::is_empty) {
            return Err(RarError::InvalidOption(
                "header encryption requires a non-empty password".into(),
            ));
        }
        if self.recovery_percent.is_some() && self.volume_size.is_some() {
            return Err(RarError::InvalidOption(
                "inline recovery records cannot be combined with data volumes".into(),
            ));
        }
        if self.recovery_volumes_percent.is_some() && self.recovery_volume_count.is_some() {
            return Err(RarError::InvalidOption(
                "recovery-volume percent and exact count are mutually exclusive".into(),
            ));
        }
        if (self.recovery_volumes_percent.is_some() || self.recovery_volume_count.is_some())
            && self.volume_size.is_none()
        {
            return Err(RarError::InvalidOption(
                "recovery volumes require a data-volume size".into(),
            ));
        }

        if self.format_version == ArchiveVersion::Rar40 {
            crate::format::rar4::create::validate_rar4_only(
                self.quick_open,
                self.blake2,
                self.recovery_volumes_percent,
                self.recovery_volume_count,
                self.save_owner,
                self.save_streams,
                self.dictionary_size.is_some(),
            )?;
        }
        // A `Rar50` archive accepts every dictionary size; sizes above 4 GiB
        // keep WinRAR's auto semantics (see [`Self::into_legacy`]) instead of
        // being downgraded or rejected.
        Ok(())
    }

    fn into_legacy(self) -> RarResult<CreateOptions> {
        self.validate()?;
        let (solid, solid_reset) = match self.solid_mode {
            SolidMode::Disabled => (false, SolidReset::Continuous),
            SolidMode::Continuous => (true, SolidReset::Continuous),
            SolidMode::PerVolume => (true, SolidReset::PerVolume),
            SolidMode::PerExtension => (true, SolidReset::PerExtension),
        };
        // Dictionary mapping is owned by the RAR5 format module (the RAR4
        // container takes no dictionary; validation above refuses one).
        let v70 = self.format_version == ArchiveVersion::Rar70;
        let (dictionary_log, dictionary_bytes) = if self.format_version == ArchiveVersion::Rar40 {
            (None, None)
        } else {
            crate::format::rar5::create::dictionary_fields(v70, self.dictionary_size)
        };

        Ok(CreateOptions {
            format_version: self.format_version,
            solid,
            solid_reset,
            quick_open: self.quick_open,
            blake2: self.blake2,
            password: self.password,
            encrypt_headers: self.encrypt_headers,
            recovery_percent: self.recovery_percent,
            recovery_volumes_percent: self.recovery_volumes_percent,
            recovery_volume_count: self.recovery_volume_count,
            volume_size: self.volume_size,
            dict_size_log: dictionary_log,
            dict_size_bytes: dictionary_bytes,
            force_v70: self.format_version == ArchiveVersion::Rar70,
            save_ctime: self.save_ctime,
            save_atime: self.save_atime,
            time_precision_seconds: self.time_precision_seconds,
            save_mtime: self.save_mtime,
            save_owner: self.save_owner,
            save_streams: self.save_streams,
            threads: self.thread_count.map(ThreadCount::get),
        })
    }
}

impl fmt::Debug for WriterOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriterOptions")
            .field("format_version", &self.format_version)
            .field("solid_mode", &self.solid_mode)
            .field("quick_open", &self.quick_open)
            .field("blake2", &self.blake2)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("encrypt_headers", &self.encrypt_headers)
            .field("recovery_percent", &self.recovery_percent)
            .field("recovery_volumes_percent", &self.recovery_volumes_percent)
            .field("recovery_volume_count", &self.recovery_volume_count)
            .field("volume_size", &self.volume_size)
            .field("dictionary_size", &self.dictionary_size)
            .field("save_ctime", &self.save_ctime)
            .field("save_atime", &self.save_atime)
            .field("time_precision_seconds", &self.time_precision_seconds)
            .field("save_mtime", &self.save_mtime)
            .field("save_owner", &self.save_owner)
            .field("save_streams", &self.save_streams)
            .field("thread_count", &self.thread_count)
            .finish()
    }
}

/// Options used by [`ArchiveWriter::append_with`].
#[derive(Clone, Default)]
pub struct AppendOptions {
    password: Option<String>,
    dictionary_size: Option<DictionarySize>,
    thread_count: Option<ThreadCount>,
}

impl AppendOptions {
    /// Create append options without a password or per-archive overrides.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the password used to open and append encrypted archives.
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

    /// Set the requested dictionary size for newly appended members.
    #[must_use]
    pub fn dictionary_size(mut self, size: DictionarySize) -> Self {
        self.dictionary_size = Some(size);
        self
    }

    /// Set a per-archive compression thread count (requires the `parallel`
    /// feature; otherwise compression stays sequential).
    #[must_use]
    pub fn thread_count(mut self, count: ThreadCount) -> Self {
        self.thread_count = Some(count);
        self
    }
}

impl fmt::Debug for AppendOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppendOptions")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("dictionary_size", &self.dictionary_size)
            .field("thread_count", &self.thread_count)
            .finish()
    }
}

/// Per-entry options used by typed writer add methods.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntryWriteOptions {
    compression_level: CompressionLevel,
}

impl EntryWriteOptions {
    /// Create options using [`CompressionLevel::NORMAL`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Select the member's compression level.
    #[must_use]
    pub fn compression_level(mut self, level: CompressionLevel) -> Self {
        self.compression_level = level;
        self
    }

    /// Return the configured compression level.
    pub const fn level(self) -> CompressionLevel {
        self.compression_level
    }
}

/// One borrowed entry to add with [`ArchiveWriter::add_batch`].
#[derive(Clone, Copy, Debug)]
pub enum WriteEntry<'a> {
    /// In-memory bytes stored under `name`.
    Bytes {
        /// Archive member name.
        name: &'a str,
        /// Raw member data.
        data: &'a [u8],
        /// Per-entry write options.
        options: EntryWriteOptions,
    },
    /// A file or recursively traversed directory from disk.
    File {
        /// Source filesystem path.
        path: &'a Path,
        /// Optional archive member name override.
        name: Option<&'a str>,
        /// Per-entry write options.
        options: EntryWriteOptions,
    },
    /// A directory header without recursive traversal.
    Directory {
        /// Source filesystem directory used for metadata.
        path: &'a Path,
        /// Optional archive name override; the basename is used when omitted.
        name: Option<&'a str>,
    },
}

/// Paths produced by a successfully committed writer transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteReport {
    volume_paths: Vec<PathBuf>,
}

impl WriteReport {
    /// Return every final data-volume path in volume order.
    pub fn volume_paths(&self) -> &[PathBuf] {
        &self.volume_paths
    }

    /// Consume the report and return every final data-volume path.
    pub fn into_volume_paths(self) -> Vec<PathBuf> {
        self.volume_paths
    }

    /// Return the primary archive path (the first data volume).
    pub fn primary_path(&self) -> &Path {
        self.volume_paths
            .first()
            .expect("a completed archive always has a data-volume path")
    }
}

/// A role-specific archive writer with explicit transactional commit.
///
/// Dropping this type aborts the transaction and removes staging files. Any
/// failed add operation also aborts and poisons the writer; only [`Self::finish`]
/// can commit output to final paths.
pub struct ArchiveWriter {
    archive: Option<RarArchive>,
}

impl ArchiveWriter {
    /// Begin creating an archive with default [`WriterOptions`].
    pub fn create(path: impl AsRef<Path>) -> RarResult<Self> {
        Self::create_with(path, WriterOptions::default())
    }

    /// Validate options and begin creating an archive.
    pub fn create_with(path: impl AsRef<Path>, options: WriterOptions) -> RarResult<Self> {
        let legacy = options.into_legacy()?;
        let archive = RarArchive::create_with_options(path, legacy)?;
        Ok(Self {
            archive: Some(archive),
        })
    }

    /// Begin appending to an existing single-volume RAR5 (RAR50/RAR70)
    /// archive with default [`AppendOptions`]. RAR4 archives are rejected
    /// with [`RarError::Unsupported`]: appending requires the RAR5 container.
    pub fn append(path: impl AsRef<Path>) -> RarResult<Self> {
        Self::append_with(path, AppendOptions::default())
    }

    /// Validate options and begin appending to an existing archive. RAR4
    /// archives are rejected with [`RarError::Unsupported`]: appending
    /// requires the RAR5 container.
    pub fn append_with(path: impl AsRef<Path>, options: AppendOptions) -> RarResult<Self> {
        let mut archive = match options.password.as_deref() {
            Some(password) => RarArchive::open_append_with_password(path, password)?,
            None => RarArchive::open_append(path)?,
        };
        let configure = (|| {
            if let Some(size) = options.dictionary_size {
                archive.set_dictionary(
                    size.rar5_log(),
                    size.rar5_log().is_none().then_some(size.bytes()),
                )?;
            }
            if let Some(count) = options.thread_count {
                archive.set_compression_threads(Some(count.get()))?;
            }
            Ok(())
        })();
        if let Err(error) = configure {
            archive.abort();
            return Err(error);
        }
        Ok(Self {
            archive: Some(archive),
        })
    }

    /// Add a filesystem path, recursively traversing directories.
    pub fn add_path(
        &mut self,
        path: impl AsRef<Path>,
        options: EntryWriteOptions,
    ) -> RarResult<()> {
        self.apply(|archive| archive.add(path, options.level().get()))
    }

    /// Add a filesystem path under a custom archive name.
    pub fn add_path_as(
        &mut self,
        path: impl AsRef<Path>,
        name: &str,
        options: EntryWriteOptions,
    ) -> RarResult<()> {
        self.apply(|archive| archive.add_as(path, name, options.level().get()))
    }

    /// Add in-memory bytes under an archive member name.
    pub fn add_bytes(
        &mut self,
        name: &str,
        data: &[u8],
        options: EntryWriteOptions,
    ) -> RarResult<()> {
        self.apply(|archive| archive.add_bytes(name, data, options.level().get()))
    }

    /// Add a directory header without recursively adding its children.
    pub fn add_directory(&mut self, path: impl AsRef<Path>, name: &str) -> RarResult<()> {
        self.apply(|archive| archive.add_directory_only(path, name))
    }

    /// Add a link/copy redirect member (Unix or Windows symlink, junction,
    /// hardlink, or file copy) whose payload is a reference to another
    /// member. Mirrors the legacy [`RarArchive::add_redirect`]; callers add
    /// redirects after their data members, preserving archive order.
    pub fn add_redirect(&mut self, name: &str, redir_type: u64, target: &str) -> RarResult<()> {
        self.apply(|archive| archive.add_redirect(name, redir_type, target))
    }

    /// Add borrowed entries in order, preserving duplicate names.
    pub fn add_batch(&mut self, entries: &[WriteEntry<'_>]) -> RarResult<()> {
        let legacy: Vec<_> = entries
            .iter()
            .map(|entry| match *entry {
                WriteEntry::Bytes {
                    name,
                    data,
                    options,
                } => BatchEntry::Bytes {
                    name,
                    data,
                    level: options.level().get(),
                },
                WriteEntry::File {
                    path,
                    name,
                    options,
                } => BatchEntry::File {
                    path,
                    name,
                    level: options.level().get(),
                },
                WriteEntry::Directory { path, name } => BatchEntry::Directory { path, name },
            })
            .collect();
        self.apply(|archive| archive.add_batch(&legacy))
    }

    /// Install or clear a caller-owned cancellation flag.
    pub fn set_cancel_flag(&mut self, flag: Option<Arc<AtomicBool>>) -> RarResult<()> {
        self.with_archive(|archive| archive.set_cancel_flag(flag))
    }

    /// Install or clear the write-progress callback.
    pub fn set_progress_callback(
        &mut self,
        callback: Option<Box<dyn FnMut(u64, u64) + Send>>,
    ) -> RarResult<()> {
        self.with_archive(|archive| archive.set_progress_callback(callback))
    }

    /// Override the progress callback's total input-byte denominator.
    pub fn set_progress_total(&mut self, total: u64) -> RarResult<()> {
        self.with_archive(|archive| archive.set_progress_total(total))
    }

    /// Finalize the transaction and commit the staged output to its final
    /// path(s), returning the final data paths in volume order.
    ///
    /// Commit granularity: a single-volume archive appears at its final path
    /// atomically (the whole file is moved into place). Multi-volume output is
    /// moved volume by volume — every volume file is individually complete
    /// once moved, but an interruption between renames can leave a partial
    /// set at the final names. `.rev` recovery volumes, when requested, are
    /// generated only after every data volume is committed; if that step
    /// fails, `finish` returns the error but the data volumes are already on
    /// disk.
    pub fn finish(mut self) -> RarResult<WriteReport> {
        let mut archive = self.archive.take().ok_or_else(Self::poisoned_error)?;
        if let Err(error) = archive.close() {
            archive.abort();
            return Err(error);
        }
        let volume_paths = archive.volume_paths.clone();
        debug_assert!(!volume_paths.is_empty());
        Ok(WriteReport { volume_paths })
    }

    fn apply(&mut self, operation: impl FnOnce(&mut RarArchive) -> RarResult<()>) -> RarResult<()> {
        let result = match self.archive.as_mut() {
            Some(archive) => operation(archive),
            None => return Err(Self::poisoned_error()),
        };
        if let Err(error) = result {
            if let Some(mut archive) = self.archive.take() {
                archive.abort();
            }
            return Err(error);
        }
        Ok(())
    }

    fn with_archive(&mut self, operation: impl FnOnce(&mut RarArchive)) -> RarResult<()> {
        let archive = self.archive.as_mut().ok_or_else(Self::poisoned_error)?;
        operation(archive);
        Ok(())
    }

    fn poisoned_error() -> RarError {
        RarError::InvalidState("archive writer transaction has been aborted".into())
    }
}

impl Drop for ArchiveWriter {
    fn drop(&mut self) {
        if let Some(mut archive) = self.archive.take() {
            archive.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_RAR70_DICTIONARY_BYTES, DictionarySize, WriterOptions};
    use crate::version::ArchiveVersion;

    #[test]
    fn rar70_options_force_byte_sized_default_dictionary() {
        let options = WriterOptions::new()
            .format_version(ArchiveVersion::Rar70)
            .into_legacy()
            .unwrap();
        assert!(options.force_v70);
        assert_eq!(options.dict_size_log, None);
        assert_eq!(
            options.dict_size_bytes,
            Some(DEFAULT_RAR70_DICTIONARY_BYTES)
        );
    }

    #[test]
    fn rar50_dictionary_mapping_keeps_legacy_auto_semantics() {
        // A RAR5 log dictionary up to 4 GiB maps to the log field.
        let small = WriterOptions::new()
            .format_version(ArchiveVersion::Rar50)
            .dictionary_size(DictionarySize::try_from(64 * 1024 * 1024u64).unwrap())
            .into_legacy()
            .unwrap();
        assert_eq!(small.format_version, ArchiveVersion::Rar50);
        assert_eq!(small.dict_size_log, Some(9)); // 64 MiB = 128 KiB << 9
        assert_eq!(small.dict_size_bytes, None);
        assert!(!small.force_v70);

        // A > 4 GiB request keeps the legacy byte-size field (auto v70:
        // only members whose effective dictionary exceeds 4 GiB become
        // v70; small members stay v50 with the capped log).
        let big = WriterOptions::new()
            .format_version(ArchiveVersion::Rar50)
            .dictionary_size(DictionarySize::try_from(6 * 1024 * 1024 * 1024u64).unwrap())
            .into_legacy()
            .unwrap();
        assert_eq!(big.format_version, ArchiveVersion::Rar50);
        assert_eq!(big.dict_size_log, None);
        assert_eq!(big.dict_size_bytes, Some(6 * 1024 * 1024 * 1024));
        assert!(!big.force_v70, "Rar50 never forces v70");
    }
}
