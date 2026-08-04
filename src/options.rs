//! Public option structs for archive creation and extraction.

use crate::codec::FilterPolicy;

/// Options controlling RAR5 archive creation.
///
/// All fields default to the same behavior as the existing `create*`
/// constructors; enable only the features you need.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    /// Output-filter policy for compressed members. AutoSize (the default)
    /// tries Delta / E8 / E8E9 / ARM candidates per member and keeps the
    /// smallest result; filtered members are non-solid and capped at
    /// [`crate::codec::AUTO_FILTER_MAX_BUFFER`] bytes.
    pub filter: FilterPolicy,
}

/// Options controlling extraction and buffered reads.
///
/// The defaults are deliberately safe: unsafe member names are rejected,
/// and per-file / total output sizes are bounded. Relax them only for
/// trusted archives.
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
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            safe_paths: true,
            max_unpacked_bytes: Some(4 * 1024 * 1024 * 1024),
            max_total_unpacked_bytes: Some(32 * 1024 * 1024 * 1024),
        }
    }
}
