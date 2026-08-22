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
    /// RAR 5.0 container + v50 compression.
    Rar50,
    /// RAR 5.0 container with the RAR7 (v70) large-dictionary codec.
    Rar70,
}

impl ArchiveVersion {
    /// All versions, in order.
    pub const ALL: [ArchiveVersion; 2] = [ArchiveVersion::Rar50, ArchiveVersion::Rar70];

    /// Stable machine-readable name (`"rar50"` / `"rar70"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            ArchiveVersion::Rar50 => "rar50",
            ArchiveVersion::Rar70 => "rar70",
        }
    }
}

impl std::fmt::Display for ArchiveVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
