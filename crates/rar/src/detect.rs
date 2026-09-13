//! Archive signature detection and SFX stub scanning.
//!
//! Mirrors the reference layout's `detect` module: locating the archive
//! start (plain or self-extracting) lives here, not in the format modules.

use crate::format::rar5::RAR5_SIGNATURE;

/// The RAR 1.5–4.x container family signature (7 bytes, distinct from
/// RAR5's 8-byte `Rar!\x1a\x07\x01\x00`).
pub const RAR4_SIGNATURE: &[u8; 7] = b"Rar!\x1a\x07\x00";

/// The RAR 1.3/1.4 container signature (`RE~^`, 4 bytes). Weak enough that
/// the scanner only trusts it when no RAR4/RAR5 signature exists nearby.
pub const RAR13_SIGNATURE: &[u8; 4] = b"RE~^";

/// Which container family a detected signature belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveFamily {
    Rar13,
    Rar15To40,
    Rar50Plus,
}

/// Locate the archive start (plain or after an SFX stub).
///
/// The strong RAR4/RAR5 signatures win over an earlier `RE~^`: four bytes
/// are too weak to trust when a distinctive longer signature exists,
/// matching the reference scanner (`rars::detect`).
pub(crate) fn find_archive_start(input: &[u8], max_scan: usize) -> Option<(ArchiveFamily, usize)> {
    let limit = input
        .len()
        .min(max_scan.saturating_add(RAR5_SIGNATURE.len()));
    let window = &input[..limit];
    let strong = [
        find_bytes(window, RAR5_SIGNATURE).map(|offset| (ArchiveFamily::Rar50Plus, offset)),
        find_bytes(window, RAR4_SIGNATURE).map(|offset| (ArchiveFamily::Rar15To40, offset)),
    ]
    .into_iter()
    .flatten()
    .filter(|(_, offset)| *offset <= max_scan)
    .min_by_key(|(_, offset)| *offset);
    if let Some(found) = strong {
        return Some(found);
    }
    find_bytes(window, RAR13_SIGNATURE)
        .filter(|offset| *offset <= max_scan)
        .map(|offset| (ArchiveFamily::Rar13, offset))
}

/// Scan at most this many bytes of an input for the archive signature.
/// SFX stubs are small; 8 MiB covers realistic self-extracting modules
/// (the same bound the reference readers use).
pub const SFX_SCAN_LIMIT: usize = 8 * 1024 * 1024;

/// Byte offset where the RAR5 archive begins inside an SFX file (the end
/// of the embedded stub). Returns `None` when no signature is found.
pub fn sfx_offset_of(input: &[u8]) -> Option<usize> {
    find_bytes(input, RAR5_SIGNATURE)
}

/// Find the first occurrence of `needle` in `haystack`.
pub(crate) fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::{ArchiveFamily, RAR13_SIGNATURE, find_archive_start};

    #[test]
    fn finds_a_bare_rar13_signature() {
        let found = find_archive_start(b"RE~^payload", 128).unwrap();
        assert_eq!(found.0, ArchiveFamily::Rar13);
        assert_eq!(found.1, 0);
    }

    #[test]
    fn finds_sfx_prefixed_rar13_archive() {
        let found = find_archive_start(b"stub bytes RE~^payload", 128).unwrap();
        assert_eq!(found.0, ArchiveFamily::Rar13);
        assert_eq!(found.1, 11);
    }

    #[test]
    fn strong_signature_wins_over_an_earlier_rar13_match() {
        let found = find_archive_start(b"stub RE~^ bytes Rar!\x1a\x07\x00payload", 128).unwrap();
        assert_eq!(found.0, ArchiveFamily::Rar15To40);
        assert_eq!(found.1, 16);
    }

    #[test]
    fn scan_limit_bounds_sfx_detection() {
        let input = b"stub bytes RE~^payload";
        assert_eq!(find_archive_start(input, 10), None);
        let found = find_archive_start(input, 11).unwrap();
        assert_eq!(found.0, ArchiveFamily::Rar13);
        assert_eq!(found.1, RAR13_SIGNATURE.len() + 7);
    }

    #[test]
    fn rejects_unknown_data() {
        assert_eq!(find_archive_start(b"not an archive", 128), None);
        assert_eq!(find_archive_start(b"", 128), None);
    }
}
