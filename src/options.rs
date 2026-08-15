//! Public option structs for archive creation and extraction.

/// Options controlling RAR5 archive creation.
///
/// All fields default to the same behavior as the existing `create*`
/// constructors; enable only the features you need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOptions {
    /// Create a solid archive: consecutive compressed members share one
    /// LZ window (better ratio, slower random access). Single-volume only.
    pub solid: bool,
    /// Add a RAR5 quick-open ("QO") service record containing a copy of
    /// every file header. Only effective for single-volume archives
    /// without header encryption.
    pub quick_open: bool,
    /// Write a BLAKE2sp hash record for every member (in addition to the
    /// regular CRC32 field), matching WinRAR's `-htb` behavior.
    pub blake2: bool,
    /// Optional AES-256 password for file-level encryption.
    pub password: Option<String>,
    /// Encrypt archive headers (file names and structure). Requires
    /// `password`; incompatible with multi-volume archives.
    pub encrypt_headers: bool,
    /// Add an inline recovery record protecting this percent (0-100) of
    /// the archive (WinRAR `-rr`). Incompatible with multi-volume.
    pub recovery_percent: Option<u8>,
    /// Create this many `.rev` recovery volumes as a percentage of the
    /// data volume count (WinRAR `-rvN%`). Requires `volume_size`.
    pub recovery_volumes_percent: Option<u8>,
    /// Create exactly this many `.rev` recovery volumes, auto-capped at
    /// the data volume count. Requires `volume_size`.
    pub recovery_volume_count: Option<u32>,
    /// Volume size in bytes; when set, produces a multi-volume archive.
    pub volume_size: Option<u64>,
    /// Dictionary size as a RAR5 log (`128 KiB << log`), like WinRAR's
    /// `-md`; `None` = WinRAR's default (32 MiB, capped at 2x the file
    /// size rounded down to a power of two). Valid logs: 0..=15
    /// (128 KiB .. 4 GiB).
    pub dict_size_log: Option<u8>,
    /// Save the creation time (Windows) / ctime (Unix inode change time)
    /// in the FILE_TIME extra record, like WinRAR's `-tsc`.
    pub save_ctime: bool,
    /// Save the last access time in the FILE_TIME extra record, like
    /// WinRAR's `-tsa`.
    pub save_atime: bool,
    /// Store timestamps at 1-second precision instead of nanoseconds,
    /// like WinRAR's `-ts...1` (all times of a member share one precision).
    pub time_precision_seconds: bool,
    /// Save the modification time (like WinRAR's `-tsm`; always on unless
    /// `-tsm-` / `-ts-` is given).
    pub save_mtime: bool,
    /// Save the owner and group (numeric ids) in an OWNER extra record on
    /// Unix (like WinRAR's `-ow`); no-op elsewhere.
    pub save_owner: bool,
    /// Save NTFS alternate data streams as "STM" service records (like
    /// WinRAR's `-os`); no-op off Windows.
    pub save_streams: bool,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            solid: false,
            quick_open: false,
            blake2: false,
            password: None,
            encrypt_headers: false,
            recovery_percent: None,
            recovery_volumes_percent: None,
            recovery_volume_count: None,
            volume_size: None,
            dict_size_log: None,
            save_ctime: false,
            save_atime: false,
            time_precision_seconds: false,
            save_mtime: true,
            save_owner: false,
            save_streams: false,
        }
    }
}

/// Options controlling extraction and buffered reads.
///
/// The defaults are deliberately safe: unsafe member names are rejected,
/// and per-file / total output sizes are bounded. Relax them only for
/// trusted archives.
///
/// Note: extraction to disk (`extract` / `extract_all`) is fully
/// streaming, so arbitrarily large members (multi-GiB) only need
/// `max_unpacked_bytes: None`. The 4 GiB default primarily guards the
/// in-memory `read` API, which materializes whole members in a `Vec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractOptions {
    /// Reject member names that could escape the destination directory
    /// (absolute paths, `..`, Windows drive components, NUL bytes) and
    /// verify resolved paths stay inside the destination.
    pub safe_paths: bool,
    /// Maximum uncompressed bytes allowed for a single member
    /// (`None` = unlimited).
    pub max_unpacked_bytes: Option<u64>,
    /// Maximum total uncompressed bytes allowed across one extraction
    /// (`None` = unlimited).
    pub max_total_unpacked_bytes: Option<u64>,
    /// Extract members flat: each member is written to the destination
    /// directory under its basename (no directory tree), like `rar e` /
    /// `unrar e`. The safe-path policy still applies — the member name is
    /// sanitized and contained before its basename is used.
    pub flat_paths: bool,
    /// Skip members whose destination already exists (like `-o-`): no
    /// overwrites, and existing files are left untouched.
    pub skip_existing: bool,
    /// Also restore the creation time (Windows) from the FILE_TIME extra
    /// record (like WinRAR's `-tsc` on extraction). Ignored on Unix,
    /// where the change time cannot be set.
    pub set_creation_time: bool,
    /// Also restore the last access time from the FILE_TIME extra record
    /// (like WinRAR's `-tsa` on extraction).
    pub set_access_time: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            safe_paths: true,
            max_unpacked_bytes: Some(4 * 1024 * 1024 * 1024),
            max_total_unpacked_bytes: Some(32 * 1024 * 1024 * 1024),
            flat_paths: false,
            skip_existing: false,
            set_creation_time: false,
            set_access_time: false,
        }
    }
}
