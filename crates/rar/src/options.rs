//! Public option structs for archive creation and extraction.

use crate::version::ArchiveVersion;

/// Options controlling RAR5 archive creation.
///
/// All fields default to the plain unencrypted single-volume create
/// behavior; enable only the features you need.
/// How the solid compression chain is split (WinRAR `-s` modifiers).
///
/// A solid archive packs several consecutive members as one continuous LZ
/// stream. Resetting the statistics (clearing the shared window / Huffman
/// tables) between groups typically lowers compression but speeds access to
/// individual members and improves damage resistance. `Continuous` matches
/// this implementation's default (and WinRAR's `-sd`): the statistics are
/// kept across the whole archive, including volume boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SolidReset {
    /// Keep the solid statistics across the whole archive (WinRAR `-sd`).
    #[default]
    Continuous,
    /// Always reset the solid statistics at the start of each new volume
    /// (WinRAR `-sv`). Single-volume archives are unaffected.
    PerVolume,
    /// Reset the solid statistics whenever the file extension of the next
    /// member changes (WinRAR `-se`); members sharing an extension stay in
    /// one group.
    PerExtension,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOptions {
    /// Target archive format version. `Rar40` writes the legacy RAR 3.x/4.x
    /// container (`Rar!\x1a\x07\x00`, 7-byte signature, fixed-width headers,
    /// 16-bit CRC). `Rar50` (default) writes the modern RAR5 container.
    /// `Rar70` selects RAR5 with v70 codec members.
    pub format_version: ArchiveVersion,
    /// Create a solid archive: consecutive compressed members share one
    /// LZ window (better ratio, slower random access). Single-volume only.
    pub solid: bool,
    /// How the solid chain is split (WinRAR `-s` modifiers `-sd`/`-sv`/`-se`).
    /// `Continuous` keeps the statistics across the whole archive (the
    /// default); `PerVolume` resets at every volume boundary; `PerExtension`
    /// resets when the member's file extension changes. Non-solid archives
    /// ignore this.
    pub solid_reset: SolidReset,
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
    /// Actual dictionary size in bytes for RAR7 (v70) members (WinRAR's
    /// `-md` above 4 GiB). Any value > 4 GiB is accepted (it need not be
    /// a power of two); the header encodes it as a 5-bit power-of-two
    /// base plus a 1/32 increment. Mutually exclusive with
    /// `dict_size_log` in practice (one `-md` switch only).
    pub dict_size_bytes: Option<u64>,
    /// Write RAR7 (v70) members (`comp_version` 1, DCX distance table)
    /// even when `dict_size_bytes` is at or below the 4 GiB threshold
    /// that normally selects v70 (WinRAR's `-md` semantics). The header
    /// is legal v70 — the format does not require a > 4 GiB dictionary —
    /// but WinRAR compatibility at this scale is not part of the
    /// validated surface, so this is mainly a test seam that runs the
    /// v70 code paths at small scale. Requires `dict_size_bytes`; no-op
    /// without it.
    pub force_v70: bool,
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
    /// Compression threads for this archive (like `-mt<N>`); `None` uses
    /// the process-global [`set_compression_threads`] setting (automatic
    /// sizing when that is 0). Scoped to the archive: concurrent archives
    /// with different thread counts each run on their own pool and never
    /// interfere. This field is only consulted when the `parallel` feature
    /// is enabled; without it compression is sequential regardless.
    pub threads: Option<usize>,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            format_version: ArchiveVersion::Rar50,
            solid: false,
            solid_reset: SolidReset::Continuous,
            quick_open: false,
            blake2: false,
            password: None,
            encrypt_headers: false,
            recovery_percent: None,
            recovery_volumes_percent: None,
            recovery_volume_count: None,
            volume_size: None,
            dict_size_log: None,
            dict_size_bytes: None,
            force_v70: false,
            save_ctime: false,
            save_atime: false,
            time_precision_seconds: false,
            save_mtime: true,
            save_owner: false,
            save_streams: false,
            threads: None,
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
    /// Rename the destination automatically when it already exists
    /// (like `-or`): `name.ext` becomes `name(1).ext`, `name(2).ext`, ...
    pub auto_rename: bool,
    /// Keep partially extracted files when a member fails to decode
    /// (like `-kb`): the incomplete output is left on disk.
    pub keep_broken: bool,
    /// Also restore the creation time (Windows) from the FILE_TIME extra
    /// record (like WinRAR's `-tsc` on extraction). Ignored on Unix,
    /// where the change time cannot be set.
    pub set_creation_time: bool,
    /// Also restore the last access time from the FILE_TIME extra record
    /// (like WinRAR's `-tsa` on extraction).
    pub set_access_time: bool,
    /// Maximum dictionary size accepted when decoding a member
    /// (`None` = unlimited). Defaults to 4 GiB, like WinRAR, which
    /// refuses archives whose dictionary exceeds 4 GiB (RAR7) unless
    /// `-mdx<size>` raises the cap.
    pub max_dict_size: Option<u64>,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            safe_paths: true,
            max_unpacked_bytes: Some(4 * 1024 * 1024 * 1024),
            max_total_unpacked_bytes: Some(32 * 1024 * 1024 * 1024),
            flat_paths: false,
            skip_existing: false,
            auto_rename: false,
            keep_broken: false,
            set_creation_time: false,
            set_access_time: false,
            max_dict_size: Some(4 * 1024 * 1024 * 1024),
        }
    }
}

/// Parse a WinRAR `-md<size>[k|m|g]` dictionary size into the
/// `(dict_size_log, dict_size_bytes)` pair used by [`CreateOptions`].
/// No unit means MiB.
///
/// Sizes in the RAR5 range (128 KiB ..= 4 GiB) must be a power of two and
/// map to a dict log (WinRAR rejects e.g. `-md3m` with "Unknown option");
/// anything above 4 GiB is accepted as-is (RAR7 v70 members), capped at
/// 128 GiB — the header's 5-bit power-of-two base plus a 1/32 increment
/// covers about 126 GiB, so 128 GiB is a safe round bound.
///
/// Returns `None` for empty, unparsable or out-of-range values.
pub fn parse_dict_size(s: &str) -> Option<(Option<u8>, Option<u64>)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1024u64),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1024 * 1024),
    };
    let bytes = num
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .filter(|b| *b >= 128 * 1024)?;
    if bytes <= 4 * 1024 * 1024 * 1024 {
        if !bytes.is_power_of_two() {
            return None;
        }
        // 128 KiB = 2^17, so log = trailing_zeros - 17 (0..=15).
        return Some((Some((bytes.trailing_zeros() - 17) as u8), None));
    }
    if bytes > 128 * 1024 * 1024 * 1024 {
        return None;
    }
    Some((None, Some(bytes)))
}

#[cfg(test)]
mod tests {
    use super::parse_dict_size;

    #[test]
    fn dict_size_parses_rar5_range() {
        assert_eq!(parse_dict_size("128k"), Some((Some(0), None)));
        assert_eq!(parse_dict_size("1m"), Some((Some(3), None)));
        assert_eq!(parse_dict_size("32m"), Some((Some(8), None)));
        assert_eq!(parse_dict_size("4g"), Some((Some(15), None)));
        // No unit means MiB.
        assert_eq!(parse_dict_size("64"), Some((Some(9), None)));
        // Case-insensitive suffix.
        assert_eq!(parse_dict_size("128K"), Some((Some(0), None)));
    }

    #[test]
    fn dict_size_rejects_invalid_rar5_values() {
        // Below the 128 KiB floor, non-power-of-two, unparsable, empty.
        assert_eq!(parse_dict_size("1k"), None);
        assert_eq!(parse_dict_size("3m"), None);
        assert_eq!(parse_dict_size("abc"), None);
        assert_eq!(parse_dict_size(""), None);
    }

    #[test]
    fn dict_size_parses_v70_range() {
        assert_eq!(
            parse_dict_size("5g"),
            Some((None, Some(5 * 1024 * 1024 * 1024)))
        );
        assert_eq!(
            parse_dict_size("64g"),
            Some((None, Some(64 * 1024 * 1024 * 1024)))
        );
        // 128 GiB is the accepted bound (the header's encodable range).
        assert_eq!(
            parse_dict_size("128g"),
            Some((None, Some(128 * 1024 * 1024 * 1024)))
        );
        assert_eq!(parse_dict_size("129g"), None);
    }
}
