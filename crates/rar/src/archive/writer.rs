//! Typed, transactional archive writer built on the legacy `RarArchive`
//! implementation.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::{BatchEntry, RarArchive};
use crate::error::{RarError, RarResult};
use crate::options::{CreateOptions, DictionarySize, FilterOptions, SolidReset};
use crate::version::ArchiveVersion;

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
    compression: ArchiveVersion,
    solid_mode: SolidMode,
    // The format-validated subset is read by `archive::create`'s
    // `WriteOptionFlags`, which is the single seam both write surfaces use.
    pub(super) quick_open: bool,
    pub(super) estimated_size: Option<u64>,
    pub(super) blake2: bool,
    password: Option<String>,
    pub(super) encrypt_headers: bool,
    pub(super) recovery_percent: Option<u8>,
    pub(super) recovery_sectors: Option<u32>,
    pub(super) recovery_volumes_percent: Option<u8>,
    pub(super) recovery_volume_count: Option<u32>,
    volume_size: Option<u64>,
    pub(super) dictionary_size: Option<DictionarySize>,
    save_ctime: bool,
    save_atime: bool,
    time_precision_seconds: bool,
    save_mtime: bool,
    pub(super) save_owner: bool,
    pub(super) save_streams: bool,
    thread_count: Option<ThreadCount>,
    filters: FilterOptions,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            compression: ArchiveVersion::V50,
            solid_mode: SolidMode::Disabled,
            quick_open: false,
            estimated_size: None,
            blake2: false,
            password: None,
            encrypt_headers: false,
            recovery_percent: None,
            recovery_sectors: None,
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
            filters: FilterOptions::default(),
        }
    }
}

impl WriterOptions {
    /// Create default RAR5 writer options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Select the member compression version. The container family follows
    /// the version: `v50`/`v70` write the RAR5 container, `v14` the DOS-era
    /// RAR 1.3/1.4 `RE~^` container (STORE/Unpack15, solid chains, comments,
    /// `-p` and old-style `.rar`/`.rNN` volumes), and `v15`/`v20`/`v29`
    /// write the legacy RAR 1.5–4.x container with per-member `unp_ver 15`/
    /// `20`/`29` (`-ma15` / `-ma2` / `-ma4`); none of the legacy formats
    /// take a configurable dictionary. Only writable versions are accepted —
    /// `v26` and `v36` are read-only and rejected at validation, never
    /// silently downgraded (see [`ArchiveVersion::is_writable`]).
    /// The owning v50/v70 policy lives in the private `format::rar5::create`
    /// module.
    #[must_use]
    pub fn compression(mut self, version: ArchiveVersion) -> Self {
        self.compression = version;
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

    /// Supply the expected final archive size, so the main header's locator
    /// reserves its quick-open and recovery offset fields at the width WinRAR
    /// uses for an archive that size (3 / 4 / 5 / 6 bytes for estimates below
    /// 512 / 2^16 / 2^23). WinRAR derives that estimate from the planned
    /// member set before it writes the main header; without it rar-rs keeps
    /// its default preallocated 5-byte fields. RAR5 only; the value must be
    /// greater than zero. This changes bytes only (the offsets are patched in
    /// at close time either way).
    #[must_use]
    pub fn estimated_size(mut self, bytes: u64) -> Self {
        self.estimated_size = Some(bytes);
        self
    }

    /// Drop a previously configured size estimate (see [`Self::estimated_size`]).
    #[must_use]
    pub fn without_estimated_size(mut self) -> Self {
        self.estimated_size = None;
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
    ///
    /// With [`Self::volume_size`] every data volume carries its own record
    /// protecting that volume (WinRAR's `-rr` with `-v`), so it can be
    /// combined with `.rev` recovery volumes. Validation still rejects formats
    /// without recovery records (`v14`, and the RAR5-only fields on legacy
    /// writers) with `InvalidOption`.
    #[must_use]
    pub fn recovery_percent(mut self, percent: u8) -> Self {
        self.recovery_percent = Some(percent);
        self
    }

    /// Add an inline recovery record of exactly `sectors` parity sectors
    /// (WinRAR RAR4 `-rr<N>`; the RAR4 record's native unit). Mutually
    /// exclusive with [`Self::recovery_percent`]. RAR5 records are sized by
    /// percent only, so validation rejects this for a RAR5 writer. With
    /// [`Self::volume_size`] each volume gets its own `sectors`-sector record.
    #[must_use]
    pub fn recovery_sectors(mut self, sectors: u32) -> Self {
        self.recovery_sectors = Some(sectors);
        self
    }

    /// Create recovery volumes using a percentage of the data-volume count.
    #[must_use]
    pub fn recovery_volumes_percent(mut self, percent: u8) -> Self {
        self.recovery_volumes_percent = Some(percent);
        self
    }

    /// Create an exact number of recovery volumes.
    ///
    /// Requires [`Self::volume_size`] and cannot be combined with a
    /// recovery-volume percentage; refused for RAR 1.3/1.4 with
    /// `InvalidOption`.
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

    /// Set the requested compression dictionary size. On v50 compression a
    /// size above 4 GiB keeps the auto v50/v70 semantics (see
    /// [`DictionarySize`]); on v70 every member is v70 with this size
    /// (32 MiB when unset).
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

    /// Set the compression filter policy (WinRAR's `-mc`): automatic,
    /// disabled or forced delta / x86 filters.
    #[must_use]
    pub fn filters(mut self, filters: FilterOptions) -> Self {
        self.filters = filters;
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
        if let Some(channels) = self.filters.delta_channels
            && !(1..=31).contains(&channels)
        {
            return Err(RarError::InvalidOption(format!(
                "delta filter channels must be in 1..=31, got {channels}"
            )));
        }
        // The combination rules live in `options` so the plain
        // `CreateOptions` struct rejects exactly the same set: a validated
        // typed option must never be silently dropped or clamped.
        crate::options::validate_combinations(crate::options::CombinationRules {
            quick_open: self.quick_open,
            encrypt_headers: self.encrypt_headers,
            password: self.password.as_deref(),
            recovery_percent: self.recovery_percent,
            recovery_sectors: self.recovery_sectors,
            recovery_volumes_percent: self.recovery_volumes_percent,
            recovery_volume_count: self.recovery_volume_count,
            volume_size: self.volume_size,
        })?;
        crate::options::require_writable_version(self.compression)?;
        crate::options::validate_locator_estimate(self.estimated_size, self.compression)?;
        let solid_reset = match self.solid_mode {
            SolidMode::PerVolume => SolidReset::PerVolume,
            SolidMode::PerExtension => SolidReset::PerExtension,
            SolidMode::Disabled | SolidMode::Continuous => SolidReset::Continuous,
        };
        crate::options::validate_solid_reset(self.compression, solid_reset)?;
        if self.compression.is_legacy() || self.compression.is_rar13() {
            super::create::validate_write_options(
                self.compression,
                &super::create::WriteOptionFlags::from_writer(self),
            )?;
        }
        // A v50 archive accepts every dictionary size; sizes above 4 GiB
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
        // Dictionary mapping is owned by the RAR5 format module (the legacy
        // v29 pipeline takes no dictionary; validation above refuses one).
        let v70 = self.compression == ArchiveVersion::V70;
        let (dictionary_log, dictionary_bytes) = if self.compression.is_legacy() {
            (None, None)
        } else {
            super::create::rar5_dictionary_fields(v70, self.dictionary_size)
        };

        Ok(CreateOptions {
            compression: self.compression,
            solid,
            solid_reset,
            quick_open: self.quick_open,
            estimated_size: self.estimated_size,
            blake2: self.blake2,
            password: self.password,
            encrypt_headers: self.encrypt_headers,
            recovery_percent: self.recovery_percent,
            recovery_sectors: self.recovery_sectors,
            recovery_volumes_percent: self.recovery_volumes_percent,
            recovery_volume_count: self.recovery_volume_count,
            volume_size: self.volume_size,
            dict_size_log: dictionary_log,
            dict_size_bytes: dictionary_bytes,
            force_v70: self.compression == ArchiveVersion::V70,
            filters: self.filters,
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
            .field("compression", &self.compression)
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
#[non_exhaustive]
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

impl std::fmt::Debug for ArchiveWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveWriter")
            .field(
                "path",
                &self.archive.as_ref().map(|archive| archive.path.as_path()),
            )
            .field(
                "entries",
                &self.archive.as_ref().map(|archive| archive.entries.len()),
            )
            .finish_non_exhaustive()
    }
}

// The typed writer role delegates to the legacy `RarArchive` engine
// (create, add*, close). The engine is doc-hidden compat surface (ADR 0006);
// the delegation seam and this module are the supported API.
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

    /// Begin appending to an existing single-volume archive (RAR5, or the
    /// legacy RAR 1.5–4.x container through its append path) with default
    /// [`AppendOptions`]. Multi-volume archives are rejected.
    pub fn append(path: impl AsRef<Path>) -> RarResult<Self> {
        Self::append_with(path, AppendOptions::default())
    }

    /// Validate options and begin appending to an existing archive. Works for
    /// single-volume RAR5 and legacy RAR4 containers; multi-volume archives
    /// are rejected.
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
    /// member. Mirrors the legacy `RarArchive::add_redirect`; callers add
    /// redirects after their data members, preserving archive order.
    pub fn add_redirect(&mut self, name: &str, redir_type: u64, target: &str) -> RarResult<()> {
        self.apply(|archive| archive.add_redirect(name, redir_type, target))
    }

    /// Add a redirect member carrying the link's modification time (WinRAR
    /// stores it like a regular member's; `mtime_ns` adds the high-precision
    /// FILE_TIME extra record when non-zero).
    pub fn add_redirect_with_time(
        &mut self,
        name: &str,
        redir_type: u64,
        target: &str,
        mtime: u32,
        mtime_ns: Option<u32>,
    ) -> RarResult<()> {
        self.apply(|archive| {
            archive.add_redirect_with_time(name, redir_type, target, mtime, mtime_ns)
        })
    }

    /// Queue the archive comment written ahead of every member. Supported by
    /// the legacy RAR4 and RAR 1.3/1.4 create paths (their comment precedes
    /// the first member header); RAR5 comments are attached after creation
    /// through the editor role.
    pub fn set_archive_comment(&mut self, comment: Option<Vec<u8>>) -> RarResult<()> {
        self.apply(|archive| {
            if !archive.is_legacy() {
                return Err(RarError::Unsupported(
                    "archive comments must be queued before creation for RAR4/RAR 1.3/1.4; \
                     use the editor for RAR5"
                        .into(),
                ));
            }
            archive.set_rar4_writer_comment(comment);
            Ok(())
        })
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
    /// atomically (the whole file is moved into place). A multi-volume set is
    /// committed as one transaction: pre-existing volumes are parked, the
    /// staged set is installed, and any error restores the previous set, so a
    /// failed commit never leaves a mix of old and new parts. A shorter
    /// overwrite retires the leftovers of a longer previous set, and the
    /// stale `.rev` files of an overwritten set go with it. The commit is
    /// journaled, so a process killed between the renames is rolled back or
    /// completed the next time the archive is written. `.rev` recovery
    /// volumes, when requested, are generated from the staged volumes and
    /// installed by that same transaction, so a committed set always carries
    /// its parity and a failed recovery build aborts the whole commit (the
    /// previous set stays intact).
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
    use super::WriterOptions;
    use crate::options::{DEFAULT_RAR7_DICTIONARY_BYTES, DictionarySize};
    use crate::version::ArchiveVersion;

    #[test]
    fn v70_compression_forces_byte_sized_default_dictionary() {
        let options = WriterOptions::new()
            .compression(ArchiveVersion::V70)
            .into_legacy()
            .unwrap();
        assert!(options.force_v70);
        assert_eq!(options.dict_size_log, None);
        assert_eq!(options.dict_size_bytes, Some(DEFAULT_RAR7_DICTIONARY_BYTES));
    }

    #[test]
    fn v50_dictionary_mapping_keeps_legacy_auto_semantics() {
        // A RAR5 log dictionary up to 4 GiB maps to the log field.
        let small = WriterOptions::new()
            .dictionary_size(DictionarySize::try_from(64 * 1024 * 1024u64).unwrap())
            .into_legacy()
            .unwrap();
        assert_eq!(small.compression, ArchiveVersion::V50);
        assert_eq!(small.dict_size_log, Some(9)); // 64 MiB = 128 KiB << 9
        assert_eq!(small.dict_size_bytes, None);
        assert!(!small.force_v70);

        // A > 4 GiB request keeps the legacy byte-size field (auto v70:
        // only members whose effective dictionary exceeds 4 GiB become
        // v70; small members stay v50 with the capped log).
        let big = WriterOptions::new()
            .dictionary_size(DictionarySize::try_from(6 * 1024 * 1024 * 1024u64).unwrap())
            .into_legacy()
            .unwrap();
        assert_eq!(big.compression, ArchiveVersion::V50);
        assert_eq!(big.dict_size_log, None);
        assert_eq!(big.dict_size_bytes, Some(6 * 1024 * 1024 * 1024));
        assert!(!big.force_v70, "v50 never forces v70");
    }

    #[test]
    fn v29_selects_the_legacy_container_without_dictionary() {
        let options = WriterOptions::new()
            .compression(ArchiveVersion::V29)
            .into_legacy()
            .unwrap();
        assert_eq!(options.compression, ArchiveVersion::V29);
        assert!(options.compression.is_legacy());
        assert_eq!(options.dict_size_log, None);
        assert_eq!(options.dict_size_bytes, None);
        assert!(!options.force_v70);
    }

    #[test]
    fn read_only_versions_are_rejected_on_the_writer() {
        for version in [ArchiveVersion::V26, ArchiveVersion::V36] {
            assert!(
                WriterOptions::new()
                    .compression(version)
                    .validate()
                    .is_err(),
                "{version} must not be writable"
            );
        }
        // Writable versions pass validation with default options.
        for version in [
            ArchiveVersion::V15,
            ArchiveVersion::V20,
            ArchiveVersion::V29,
            ArchiveVersion::V50,
            ArchiveVersion::V70,
        ] {
            WriterOptions::new()
                .compression(version)
                .validate()
                .unwrap();
        }
    }

    #[test]
    fn default_writer_is_v50() {
        let options = WriterOptions::new().into_legacy().unwrap();
        assert_eq!(options.compression, ArchiveVersion::V50);
        assert!(!options.compression.is_legacy());
    }
}
