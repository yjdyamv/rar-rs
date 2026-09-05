//! Archive compression versions.
//!
//! [`ArchiveVersion`] is the single version table spanning every member
//! codec this library reads and writes — the legacy RAR 1.5–4.x family
//! (`v15`–`v36`, keyed by the member `unp_ver` field) and the RAR5 family
//! (`v50`/`v70`, keyed by the member `comp_version` field):
//!
//! | Version | Codec                        | Container          | Writable |
//! |---------|------------------------------|--------------------|----------|
//! | `v15`   | RAR15 (adaptive-Huffman LZ)  | RAR 1.5–4.x        | —        |
//! | `v20`   | RAR20 (LZSS + Huffman)       | RAR 1.5–4.x        | —        |
//! | `v26`   | RAR20 (LZSS + Huffman)       | RAR 1.5–4.x        | —        |
//! | `v29`   | RAR29 (LZSS + Huffman + PPMd)| RAR 1.5–4.x        | yes      |
//! | `v36`   | RAR29 (same codec as `v29`)  | RAR 1.5–4.x        | —        |
//! | `v50`   | RAR5 v50 (64-entry distance) | RAR5               | yes      |
//! | `v70`   | RAR7 (80-entry DCX)          | RAR5               | yes      |
//!
//! The physical container is *derived* from the version, not a separate
//! axis: `v15`–`v36` always live in the legacy `Rar!\x1a\x07\x00` (7-byte
//! signature, fixed-width headers) container, `v50`/`v70` in the modern
//! 8-byte-signature RAR5 container. Readers report a version per member
//! ([`crate::ArchiveEntry::version`]); writers select the version on the
//! writer options ([`crate::WriterOptions::compression`] /
//! `CreateOptions::format_version`). Only `v29`, `v50` and `v70` are
//! writable: the v15/v20/v26/v36 readers exist for interoperability, and
//! writers reject them.

/// A member compression version in the archive version table.
///
/// The version selects the codec (`RAR15`/`RAR20`/`RAR29`/`RAR50`/`RAR70`)
/// and fixes the container family the member lives in. Two-digit `vXX`
/// naming covers both families uniformly: the legacy RAR 1.5–4.x unpack
/// versions (`15`/`20`/`26`/`29`/`36`) and the RAR5 member codec versions
/// (`50`/`70`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ArchiveVersion {
    /// RAR 1.5 unpack version: the original adaptive-Huffman + plain LZ
    /// codec.
    V15,
    /// RAR 2.x unpack version: LZSS + Huffman.
    V20,
    /// RAR 3.x-era unpack version 26: LZSS + Huffman (RAR20 codec), used
    /// by early RAR3 files declaring the older value.
    V26,
    /// RAR 3.x/4.x unpack version 29: LZSS + Huffman + PPMd. The legacy
    /// RAR4 write pipeline emits this version.
    V29,
    /// RAR 4.x-era unpack version 36: the same RAR29 codec as `v29`, no
    /// behavioural difference. Read-only; writers emit `v29`.
    V36,
    /// RAR5 v50 compression (64-entry distance table).
    V50,
    /// RAR7 (v70) compression with the 80-entry extended distance table
    /// and a byte-size dictionary.
    V70,
}

/// The default when no version is named is the RAR5 v50 codec.
impl Default for ArchiveVersion {
    fn default() -> Self {
        ArchiveVersion::V50
    }
}

impl ArchiveVersion {
    /// All member compression versions, in order.
    pub const ALL: [ArchiveVersion; 7] = [
        ArchiveVersion::V15,
        ArchiveVersion::V20,
        ArchiveVersion::V26,
        ArchiveVersion::V29,
        ArchiveVersion::V36,
        ArchiveVersion::V50,
        ArchiveVersion::V70,
    ];

    /// Stable machine-readable two-digit name (`"v15"` … `"v70"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            ArchiveVersion::V15 => "v15",
            ArchiveVersion::V20 => "v20",
            ArchiveVersion::V26 => "v26",
            ArchiveVersion::V29 => "v29",
            ArchiveVersion::V36 => "v36",
            ArchiveVersion::V50 => "v50",
            ArchiveVersion::V70 => "v70",
        }
    }

    /// Whether this version lives in the legacy RAR 1.5–4.x container
    /// family (`v15`–`v36`), rather than the RAR5 container (`v50`/`v70`).
    pub const fn is_legacy(self) -> bool {
        matches!(
            self,
            Self::V15 | Self::V20 | Self::V26 | Self::V29 | Self::V36
        )
    }

    /// Whether the version is writable with the current writers. Only
    /// `v29` (legacy RAR4 pipeline), `v50` and `v70` (RAR5 pipeline) can
    /// be produced; the v15/v20/v26/v36 readers exist for interop only.
    pub const fn is_writable(self) -> bool {
        matches!(self, Self::V29 | Self::V50 | Self::V70)
    }

    /// Whether this version selects the RAR7 (v70) extended distance code
    /// table (80 entries instead of the RAR5 64-entry table): v50 → 64
    /// codes, v70 → 80.
    pub const fn uses_extra_dist(self) -> bool {
        matches!(self, ArchiveVersion::V70)
    }

    /// Map a legacy member `unp_ver` field (`15`/`20`/`26`/`29`/`36`) onto
    /// the version table, or `None` for an unknown value.
    pub const fn from_unp_ver(unp_ver: u8) -> Option<Self> {
        match unp_ver {
            15 => Some(ArchiveVersion::V15),
            20 => Some(ArchiveVersion::V20),
            26 => Some(ArchiveVersion::V26),
            29 => Some(ArchiveVersion::V29),
            36 => Some(ArchiveVersion::V36),
            _ => None,
        }
    }

    /// The codec version for a member whose header flags the RAR7 (v70)
    /// algorithm: on read that is `comp_version == 1`, on write the
    /// presence of a byte-size dictionary (RAR7 carries the byte count,
    /// RAR5 a log2 field).
    pub const fn from_v70(v70: bool) -> Self {
        if v70 {
            ArchiveVersion::V70
        } else {
            ArchiveVersion::V50
        }
    }
}

impl std::fmt::Display for ArchiveVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::ArchiveVersion;

    #[test]
    fn the_table_is_exhaustive_and_two_digit_named() {
        assert_eq!(ArchiveVersion::ALL.len(), 7);
        for version in ArchiveVersion::ALL {
            let name = version.as_str();
            assert_eq!(name.len(), 3, "{version} name should be like \"v15\"");
            assert_eq!(format!("{version}"), name);
        }
    }

    #[test]
    fn legacy_and_container_derived_isolation() {
        // v15-v36 live in the legacy RAR 1.5-4.x container; v50/v70 in RAR5.
        for version in [
            ArchiveVersion::V15,
            ArchiveVersion::V20,
            ArchiveVersion::V26,
            ArchiveVersion::V29,
            ArchiveVersion::V36,
        ] {
            assert!(version.is_legacy(), "{version}");
        }
        for version in [ArchiveVersion::V50, ArchiveVersion::V70] {
            assert!(!version.is_legacy(), "{version}");
        }
    }

    #[test]
    fn only_v29_v50_v70_are_writable() {
        for version in [
            ArchiveVersion::V29,
            ArchiveVersion::V50,
            ArchiveVersion::V70,
        ] {
            assert!(version.is_writable(), "{version}");
        }
        for version in [
            ArchiveVersion::V15,
            ArchiveVersion::V20,
            ArchiveVersion::V26,
            ArchiveVersion::V36,
        ] {
            assert!(!version.is_writable(), "{version}");
        }
    }

    #[test]
    fn only_v70_uses_the_extra_distance_table() {
        assert!(ArchiveVersion::V70.uses_extra_dist());
        for version in ArchiveVersion::ALL {
            if version != ArchiveVersion::V70 {
                assert!(!version.uses_extra_dist(), "{version}");
            }
        }
    }

    #[test]
    fn unp_ver_maps_onto_the_legacy_versions() {
        for (unp_ver, version) in [
            (15, ArchiveVersion::V15),
            (20, ArchiveVersion::V20),
            (26, ArchiveVersion::V26),
            (29, ArchiveVersion::V29),
            (36, ArchiveVersion::V36),
        ] {
            assert_eq!(ArchiveVersion::from_unp_ver(unp_ver), Some(version));
        }
        assert_eq!(ArchiveVersion::from_unp_ver(0), None);
        assert_eq!(ArchiveVersion::from_unp_ver(50), None);
    }

    #[test]
    fn from_v70_maps_comp_version_onto_v50_v70() {
        assert_eq!(ArchiveVersion::from_v70(false), ArchiveVersion::V50);
        assert_eq!(ArchiveVersion::from_v70(true), ArchiveVersion::V70);
    }

    #[test]
    fn default_is_v50() {
        assert_eq!(ArchiveVersion::default(), ArchiveVersion::V50);
    }
}
