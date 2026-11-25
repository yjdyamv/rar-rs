//! Archive signature detection and SFX stub scanning.
//!
//! Mirrors the reference layout's `detect` module: locating the archive
//! start (plain or self-extracting) lives here, not in the format modules.

/// The RAR5/RAR7 container signature (8 bytes, `Rar!\x1a\x07\x01\x00`).
///
/// The signature table's single owner: every other layer (including the
/// RAR5 family modules and the recovery scanners) imports it from here, so
/// the leaf never has to reach up into `format` for one constant.
pub const RAR5_SIGNATURE: &[u8; 8] = b"Rar!\x1a\x07\x01\x00";

/// The RAR 1.5–4.x container family signature (7 bytes, distinct from
/// RAR5's 8-byte `Rar!\x1a\x07\x01\x00`).
pub const RAR4_SIGNATURE: &[u8; 7] = b"Rar!\x1a\x07\x00";

/// The RAR 1.3/1.4 container signature (`RE~^`, 4 bytes). Weak enough that
/// the scanner only trusts it when no RAR4/RAR5 signature exists nearby.
pub const RAR13_SIGNATURE: &[u8; 4] = b"RE~^";

/// Which container family a detected signature belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFamily {
    /// The RAR 1.3/1.4 container (`RE~^`).
    Rar13,
    /// The RAR 1.5–4.x container (7-byte `Rar!\x1a\x07\x00`).
    Rar15To40,
    /// The RAR5/RAR7 container (8-byte `Rar!\x1a\x07\x01\x00`).
    Rar50Plus,
}

/// Locate the archive start (plain or after an SFX stub).
///
/// The strong RAR4/RAR5 signatures win over an earlier `RE~^`, and a
/// signature must actually begin an archive: a stub that embeds the signature
/// bytes (our own `rar-sfx` links this crate, so its rodata contains them) is
/// skipped for the next candidate. Four-byte `RE~^` is only trusted when no
/// strong signature begins a valid archive.
///
/// This is the single owner of "which archive, and where": the readers open
/// through it, and callers that only need the family (for example to pick a
/// repair path) call it rather than re-scanning for the magic bytes.
pub fn find_archive_start(input: &[u8], max_scan: usize) -> Option<(ArchiveFamily, usize)> {
    if let Some(found) = first_plausible_strong(input, max_scan) {
        return Some(found);
    }
    // A strong signature with no parseable header (a truncated or
    // signature-only file) still points the family parser at the right offset,
    // so its specific "missing the main header" error surfaces.
    if let Some(found) = earliest_strong(input, max_scan) {
        return Some(found);
    }
    let limit = input
        .len()
        .min(max_scan.saturating_add(RAR5_SIGNATURE.len()));
    find_bytes(input, RAR13_SIGNATURE, limit)
        .filter(|offset| *offset <= max_scan)
        .map(|offset| (ArchiveFamily::Rar13, offset))
}

/// Earliest strong signature in the scan window, regardless of what follows.
fn earliest_strong(input: &[u8], max_scan: usize) -> Option<(ArchiveFamily, usize)> {
    let limit = input
        .len()
        .min(max_scan.saturating_add(RAR5_SIGNATURE.len()));
    [
        (ArchiveFamily::Rar50Plus, &RAR5_SIGNATURE[..]),
        (ArchiveFamily::Rar15To40, &RAR4_SIGNATURE[..]),
    ]
    .into_iter()
    .filter_map(|(family, signature)| {
        find_bytes(input, signature, limit)
            .filter(|offset| *offset <= max_scan)
            .map(|offset| (family, offset))
    })
    .min_by_key(|(_, offset)| *offset)
}

/// Earliest strong signature whose following bytes begin a valid main header.
fn first_plausible_strong(input: &[u8], max_scan: usize) -> Option<(ArchiveFamily, usize)> {
    let limit = input
        .len()
        .min(max_scan.saturating_add(RAR5_SIGNATURE.len()));
    let window = &input[..limit];
    let mut offset = 0;
    while offset <= max_scan && offset + RAR4_SIGNATURE.len() <= window.len() {
        let family = if window[offset..].starts_with(RAR5_SIGNATURE) {
            Some(ArchiveFamily::Rar50Plus)
        } else if window[offset..].starts_with(RAR4_SIGNATURE) {
            Some(ArchiveFamily::Rar15To40)
        } else {
            None
        };
        if let Some(family) = family
            && plausible_archive_start(input, family, offset)
        {
            return Some((family, offset));
        }
        offset += 1;
    }
    None
}

/// Whether the bytes at `offset` (a strong signature) begin a parseable main
/// header: a CRC-valid RAR5 block of type main/encryption, or a RAR4 main
/// header (`HEAD_TYPE 0x73`) whose declared size fits.
fn plausible_archive_start(input: &[u8], family: ArchiveFamily, offset: usize) -> bool {
    match family {
        ArchiveFamily::Rar50Plus => rar5_main_header_starts(input, offset),
        ArchiveFamily::Rar15To40 => rar4_main_header_starts(input, offset),
        ArchiveFamily::Rar13 => true,
    }
}

fn rar5_main_header_starts(input: &[u8], offset: usize) -> bool {
    let Some(rest) = input.get(offset + RAR5_SIGNATURE.len()..) else {
        return false;
    };
    if rest.len() < 5 {
        return false;
    }
    let stored_crc = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
    let Some((header_size, vint_len)) = read_vint(rest, 4) else {
        return false;
    };
    let Ok(header_size) = usize::try_from(header_size) else {
        return false;
    };
    let body_start = 4 + vint_len;
    let Some(body_end) = body_start.checked_add(header_size) else {
        return false;
    };
    if body_end > rest.len() {
        return false;
    }
    // The stored CRC covers the size vint and the body (see `read_block`).
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&rest[4..body_start]);
    hasher.update(&rest[body_start..body_end]);
    if hasher.finalize() != stored_crc {
        return false;
    }
    // Block type 1 is the main header; 4 is a plaintext encryption header.
    matches!(
        read_vint(&rest[body_start..body_end], 0),
        Some((1, _)) | Some((4, _))
    )
}

/// Decode a RAR5 varint at `offset`; `None` when truncated or over 10 bytes.
/// Kept local (rather than `crate::vint`) so `detect` adds no cross-layer edge.
fn read_vint(data: &[u8], offset: usize) -> Option<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    for index in 0..10 {
        let byte = *data.get(offset + index)?;
        if shift < 64 {
            result |= u64::from(byte & 0x7f) << shift;
        }
        shift += 7;
        if byte & 0x80 == 0 {
            return Some((result, index + 1));
        }
    }
    None
}

/// The fixed RAR4 main header: `HEAD_CRC(2) HEAD_TYPE(1) HEAD_FLAGS(2)
/// HEAD_SIZE(2)` plus the six reserved bytes. Also the only bytes `HEAD_CRC`
/// covers when `MHD_COMMENT` is set — the coverage rule owned by
/// `format::rar4::envelope::header_crc_end`.
const RAR4_MAIN_HEADER_SIZE: usize = 13;

/// `MHD_COMMENT`: a main header whose embedded comment follows the fixed
/// header, so its `HEAD_CRC` stops at [`RAR4_MAIN_HEADER_SIZE`] bytes.
const RAR4_MHD_COMMENT: u16 = 0x0002;

fn rar4_main_header_starts(input: &[u8], offset: usize) -> bool {
    let Some(rest) = input.get(offset + RAR4_SIGNATURE.len()..) else {
        return false;
    };
    // HEAD_CRC(2) HEAD_TYPE(1)=0x73 HEAD_FLAGS(2) HEAD_SIZE(2).
    if rest.len() < RAR4_MAIN_HEADER_SIZE || rest[2] != 0x73 {
        return false;
    }
    let head_size = u16::from_le_bytes([rest[5], rest[6]]) as usize;
    if !(RAR4_MAIN_HEADER_SIZE..=rest.len()).contains(&head_size) {
        return false;
    }
    // A type + size match is not enough inside an SFX stub: the release
    // `rar-sfx` links this crate, so its rodata embeds the RAR4 signature,
    // and the ppc64le build had one followed by bytes that looked like a
    // 13-byte main header — the reader then parsed that bogus offset and
    // failed its HEAD_CRC. Validate the 16-bit CRC like the RAR5 probe above
    // and the envelope parser (`read_envelope`'s `check_header_crc`), so an
    // embedded signature is skipped for the real archive.
    let head_crc = u16::from_le_bytes([rest[0], rest[1]]);
    if head_crc == 0xFFFF {
        // RAR 1.5.4-era "no CRC" sentinel: `read_envelope` skips the check.
        return true;
    }
    let flags = u16::from_le_bytes([rest[3], rest[4]]);
    // A main header with an embedded comment covers only the fixed header.
    let crc_end = if flags & RAR4_MHD_COMMENT != 0 {
        RAR4_MAIN_HEADER_SIZE.min(head_size)
    } else {
        head_size
    };
    let actual = (crc32fast::hash(&rest[2..crc_end]) & 0xffff) as u16;
    actual == head_crc
}

/// Scan at most this many bytes of an input for the archive signature.
/// SFX stubs are small; 8 MiB covers realistic self-extracting modules
/// (the same bound the reference readers use).
pub const SFX_SCAN_LIMIT: usize = 8 * 1024 * 1024;

/// Byte offset where the RAR archive begins inside an SFX file (the end of the
/// embedded stub). Returns `None` when no signature begins a valid archive.
///
/// Both strong signatures are searched and the earliest that starts a valid
/// main header wins (a stub that embeds the signature bytes is skipped), so
/// `rar s-` can strip a RAR 1.5-4.x SFX as well as a RAR5 one (the `RE~^`
/// signature is too weak to scan for here).
pub fn sfx_offset_of(input: &[u8]) -> Option<usize> {
    first_plausible_strong(input, input.len()).map(|(_, offset)| offset)
}

/// Find the first occurrence of `needle` within the first `limit` bytes of
/// `haystack`. The single owner of the bounded byte search: a signature can
/// sit behind an SFX stub, so callers search a prefix rather than the whole
/// input, and `legacy`'s recovery scanner shares this instead of its own copy.
pub(crate) fn find_bytes(haystack: &[u8], needle: &[u8], limit: usize) -> Option<usize> {
    let window = &haystack[..haystack.len().min(limit)];
    if needle.is_empty() || window.len() < needle.len() {
        return None;
    }
    window.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::{ArchiveFamily, RAR13_SIGNATURE, find_archive_start};

    /// A minimal valid RAR4 main header (13 bytes): `HEAD_CRC(2) HEAD_TYPE(1)
    /// HEAD_FLAGS(2) HEAD_SIZE(2) …`, with a correct 16-bit HEAD_CRC so the
    /// detection probe accepts it.
    fn rar4_main_header() -> [u8; 13] {
        let mut header = [0u8; 13];
        header[2] = 0x73;
        header[5] = 0x0d;
        let crc = (crc32fast::hash(&header[2..]) & 0xffff) as u16;
        header[..2].copy_from_slice(&crc.to_le_bytes());
        header
    }

    /// A minimal valid RAR5 main-header block: CRC32, size vint, body (type 1,
    /// flags 0). The CRC covers the size vint and the body.
    fn rar5_main_header() -> Vec<u8> {
        let body = [0x01u8, 0x00];
        let size_vint = [0x02u8];
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&size_vint);
        hasher.update(&body);
        let mut out = Vec::new();
        out.extend_from_slice(&hasher.finalize().to_le_bytes());
        out.extend_from_slice(&size_vint);
        out.extend_from_slice(&body);
        out
    }

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
        let mut input = b"stub RE~^ bytes ".to_vec();
        input.extend_from_slice(super::RAR4_SIGNATURE);
        input.extend_from_slice(&rar4_main_header());
        let found = find_archive_start(&input, 128).unwrap();
        assert_eq!(found.0, ArchiveFamily::Rar15To40);
        assert_eq!(found.1, 16);
    }

    /// A stub that embeds the signature bytes — our own `rar-sfx` links this
    /// crate, so its rodata contains them — must not be mistaken for the
    /// archive start; detection skips it for the real header that follows.
    #[test]
    fn skips_an_embedded_signature_that_does_not_start_an_archive() {
        let mut input = b"stub".to_vec();
        input.extend_from_slice(super::RAR5_SIGNATURE);
        input.extend_from_slice(b"garbage, not a block header");
        let real = input.len();
        input.extend_from_slice(super::RAR5_SIGNATURE);
        input.extend_from_slice(&rar5_main_header());
        let found = find_archive_start(&input, 128).unwrap();
        assert_eq!(found.0, ArchiveFamily::Rar50Plus);
        assert_eq!(found.1, real);
    }

    /// The RAR4 counterpart of the check above: an embedded signature whose
    /// following bytes merely *look* like a main header (type 0x73, plausible
    /// size) but have a wrong HEAD_CRC must be skipped. The ppc64le `rar-sfx`
    /// release stub hit exactly this — the reader used to accept the bogus
    /// offset and `unrar t`/the stub then failed with a "RAR4 block type 0x73
    /// header" CRC mismatch.
    #[test]
    fn skips_an_embedded_rar4_signature_with_a_bogus_plausible_header() {
        let mut input = b"stub".to_vec();
        input.extend_from_slice(super::RAR4_SIGNATURE);
        // A fake "main header" that only passes a type + size heuristic: type
        // 0x73, size 13, but a HEAD_CRC that does not cover these bytes.
        let fake = [0x00u8, 0x00, 0x73, 0x00, 0x00, 0x0d, 0x00, 0, 0, 0, 0, 0, 0];
        input.extend_from_slice(&fake);
        let real = input.len();
        input.extend_from_slice(super::RAR4_SIGNATURE);
        input.extend_from_slice(&rar4_main_header());
        let found = find_archive_start(&input, 128).unwrap();
        assert_eq!(found.0, ArchiveFamily::Rar15To40);
        assert_eq!(found.1, real);
    }

    /// A stub's rodata holds both strong signatures (the crate's constants sit
    /// next to each other), so detection must skip a bogus *RAR5* one for the
    /// real RAR4 archive behind it — not only a bogus RAR4 one. This is the
    /// family a caller like the CLI's `is_rar4_file` must see.
    #[test]
    fn skips_an_embedded_rar5_signature_before_a_real_rar4_archive() {
        let mut input = b"stub".to_vec();
        input.extend_from_slice(super::RAR5_SIGNATURE);
        input.extend_from_slice(&[0x41u8; 16]);
        let real = input.len();
        input.extend_from_slice(super::RAR4_SIGNATURE);
        input.extend_from_slice(&rar4_main_header());
        let found = find_archive_start(&input, 128).unwrap();
        assert_eq!(found, (ArchiveFamily::Rar15To40, real));
    }

    /// `MHD_COMMENT` widens `HEAD_SIZE` to span the embedded comment, but the
    /// probe must still compute `HEAD_CRC` over only the fixed header — the
    /// coverage rule `envelope::header_crc_end` owns.
    #[test]
    fn accepts_a_comment_carrying_rar4_main_header_with_a_13_byte_crc() {
        let mut header = [0u8; 20];
        header[2] = 0x73;
        header[3..5].copy_from_slice(&super::RAR4_MHD_COMMENT.to_le_bytes());
        header[5..7].copy_from_slice(&20u16.to_le_bytes());
        let crc = (crc32fast::hash(&header[2..super::RAR4_MAIN_HEADER_SIZE]) & 0xffff) as u16;
        header[..2].copy_from_slice(&crc.to_le_bytes());
        let mut input = super::RAR4_SIGNATURE.to_vec();
        input.extend_from_slice(&header);
        assert!(super::rar4_main_header_starts(&input, 0));
    }

    /// The RAR 1.5.4-era `0xFFFF` "no CRC" sentinel is accepted, matching
    /// `read_envelope`, which skips the check for it.
    #[test]
    fn accepts_the_no_crc_sentinel_on_a_rar4_main_header() {
        let mut header = rar4_main_header();
        header[..2].copy_from_slice(&0xFFFFu16.to_le_bytes());
        let mut input = super::RAR4_SIGNATURE.to_vec();
        input.extend_from_slice(&header);
        assert!(super::rar4_main_header_starts(&input, 0));
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
