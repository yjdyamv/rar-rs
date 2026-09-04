//! Archive format versions.
//!
//! Mirrors the reference layout's `version` module: the crate writes RAR5
//! today; RAR7 (v70) dictionary encoding rides on the same container. The
//! enum keeps room for earlier families so callers can name the target
//! explicitly once they land.

/// A concrete archive format version this library can read or write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ArchiveVersion {
    /// RAR 4.x and earlier: the legacy `Rar!\x1a\x07\x00` container family
    /// (RAR 1.5 through 4.x). Read support only.
    Rar40,
    /// RAR 5.0 container + v50 compression.
    Rar50,
    /// RAR 5.0 container with the RAR7 (v70) large-dictionary codec.
    Rar70,
}

/// The default when no version is named is the RAR5.0 variant.
impl Default for ArchiveVersion {
    fn default() -> Self {
        ArchiveVersion::Rar50
    }
}

impl ArchiveVersion {
    /// All versions, in order.
    pub const ALL: [ArchiveVersion; 3] = [
        ArchiveVersion::Rar40,
        ArchiveVersion::Rar50,
        ArchiveVersion::Rar70,
    ];

    /// Stable machine-readable name (`"rar40"` / `"rar50"` / `"rar70"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            ArchiveVersion::Rar40 => "rar40",
            ArchiveVersion::Rar50 => "rar50",
            ArchiveVersion::Rar70 => "rar70",
        }
    }

    /// Whether this is the legacy RAR 1.5–4.x container family.
    pub const fn is_rar40(self) -> bool {
        matches!(self, ArchiveVersion::Rar40)
    }

    /// Whether this version selects the RAR7 (v70) extended distance code
    /// table (80 entries instead of the RAR5 64-entry table). The codec
    /// variant is exactly the archive version: v50 → 64 codes, v70 → 80.
    pub const fn uses_extra_dist(self) -> bool {
        matches!(self, ArchiveVersion::Rar70)
    }

    /// The codec variant for a member whose header flags the RAR7 (v70)
    /// algorithm: on read that is `comp_version == 1`, on write the
    /// presence of a byte-size dictionary (RAR7 carries the byte count,
    /// RAR5 a log2 field).
    pub const fn from_v70(v70: bool) -> Self {
        if v70 {
            ArchiveVersion::Rar70
        } else {
            ArchiveVersion::Rar50
        }
    }
}

impl std::fmt::Display for ArchiveVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
