//! Archive signature detection and SFX stub scanning.
//!
//! Mirrors the reference layout's `detect` module: locating the archive
//! start (plain or self-extracting) lives here, not in the format modules.

use crate::format::rar5::RAR5_SIGNATURE;

/// The RAR 1.5–4.x container family signature (7 bytes, distinct from
/// RAR5's 8-byte `Rar!\x1a\x07\x01\x00`).
pub const RAR4_SIGNATURE: &[u8; 7] = b"Rar!\x1a\x07\x00";

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
