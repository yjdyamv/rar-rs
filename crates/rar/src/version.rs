//! Archive containers and compression versions.
//!
//! Two orthogonal concepts:
//!
//! - [`ArchiveFormat`] is the container family: the physical envelope and
//!   header set. `Rar40` is the legacy `Rar!\x1a\x07\x00` (7-byte, fixed-width
//!   headers) family; `Rar5` is the modern 8-byte-signature container. This is
//!   what a writer selects.
//! - [`ArchiveVersion`] is the member compression version inside the RAR5
//!   container: `Rar50` (v50 codec, 64-entry distance table) or `Rar70` (RAR7
//!   v70 codec, 80-entry DCX table). Readers report this per member; it is
//!   never the container. RAR4 archives report their legacy unpack version as
//!   `Entry::format_version`/`unpack_version`, not as an `ArchiveVersion`.

/// The RAR container family: the physical envelope, signature and header set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ArchiveFormat {
    /// Legacy RAR 1.5–4.x container (`Rar!\x1a\x07\x00`, 7-byte signature,
    /// fixed-width headers, 16-bit header CRC).
    Rar40,
    /// Modern RAR5 container (8-byte signature) hosting both the v50 and
    /// RAR7 (v70) member codec versions.
    Rar5,
}

/// The default when no format is named is the modern RAR5 container.
impl Default for ArchiveFormat {
    fn default() -> Self {
        ArchiveFormat::Rar5
    }
}

impl ArchiveFormat {
    /// All container families, in order.
    pub const ALL: [ArchiveFormat; 2] = [ArchiveFormat::Rar40, ArchiveFormat::Rar5];

    /// Stable machine-readable name (`"rar40"` / `"rar5"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            ArchiveFormat::Rar40 => "rar40",
            ArchiveFormat::Rar5 => "rar5",
        }
    }

    /// Whether this is the legacy RAR 1.5–4.x container family.
    pub const fn is_rar40(self) -> bool {
        matches!(self, ArchiveFormat::Rar40)
    }
}

impl std::fmt::Display for ArchiveFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A member compression version inside the RAR5 container.
///
/// This is the codec/algorithm version, not the container: both variants
/// share the RAR5 container and its envelope. RAR4 members never map here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ArchiveVersion {
    /// RAR5 v50 compression (64-entry distance table).
    Rar50,
    /// RAR7 (v70) compression with the 80-entry extended distance table.
    Rar70,
}

/// The default when no version is named is the RAR5 v50 codec.
impl Default for ArchiveVersion {
    fn default() -> Self {
        ArchiveVersion::Rar50
    }
}

impl ArchiveVersion {
    /// All member versions, in order.
    pub const ALL: [ArchiveVersion; 2] = [ArchiveVersion::Rar50, ArchiveVersion::Rar70];

    /// Stable machine-readable name (`"rar50"` / `"rar70"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            ArchiveVersion::Rar50 => "rar50",
            ArchiveVersion::Rar70 => "rar70",
        }
    }

    /// Whether this version selects the RAR7 (v70) extended distance code
    /// table (80 entries instead of the RAR5 64-entry table): v50 → 64
    /// codes, v70 → 80.
    pub const fn uses_extra_dist(self) -> bool {
        matches!(self, ArchiveVersion::Rar70)
    }

    /// The codec version for a member whose header flags the RAR7 (v70)
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
