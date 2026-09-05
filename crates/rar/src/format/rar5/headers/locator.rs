//! The main-header locator record (type 0x01), owned here once so the byte
//! rules (record layout, fixed-5-byte preallocated offset fields, relative
//! offset patching, CRC recompute) live in a single place. Both the
//! create/close path (`archive.rs`) and the surgical rewrite path
//! (`rewrite.rs`) consume this module.

use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::vint_fixed5;
use crate::format::rar5::vint;

/// Locator record type (extra record type 0x01).
pub(crate) const LOCATOR_TYPE: u64 = 0x01;
pub(crate) const LOCATOR_FLAG_QUICK_OPEN: u64 = 0x0001;
pub(crate) const LOCATOR_FLAG_RECOVERY: u64 = 0x0002;

/// Build the locator record body `[flags vint][qo offset vint][rr offset
/// vint]`, returning the body bytes and the positions (relative to the body
/// start) of the preallocated QO and RR offset fields. Only the offsets
/// whose flags are set are present; absent fields are `None`.
pub(crate) fn build_locator_body(
    quick_open: bool,
    recovery: bool,
) -> (Vec<u8>, Option<usize>, Option<usize>) {
    let mut flags = 0u64;
    if quick_open {
        flags |= LOCATOR_FLAG_QUICK_OPEN;
    }
    if recovery {
        flags |= LOCATOR_FLAG_RECOVERY;
    }
    let mut body = Vec::new();
    body.extend(vint::encode(flags));
    let qo = if quick_open {
        let p = body.len();
        body.extend_from_slice(&vint_fixed5(0));
        Some(p)
    } else {
        None
    };
    let rr = if recovery {
        let p = body.len();
        body.extend_from_slice(&vint_fixed5(0));
        Some(p)
    } else {
        None
    };
    (body, qo, rr)
}

/// Frame a locator record for the header extra area:
/// `[record size vint][record type vint][body]`.
pub(crate) fn frame_locator_record(body: &[u8]) -> Vec<u8> {
    let record_type = vint::encode(LOCATOR_TYPE);
    let mut record = Vec::new();
    record.extend(vint::encode((record_type.len() + body.len()) as u64));
    record.extend(record_type);
    record.extend(body);
    record
}

/// Patch the preallocated locator offset fields in a plaintext main archive
/// header in place and recompute the header CRC. Offsets are stored relative
/// to `base` (the archive start after the signature, plus any SFX stub).
/// Returns whether any field was patched (and thus the CRC rewritten).
pub(crate) fn patch_locator_fields(
    hdr: &mut [u8],
    qo_offset: Option<u64>,
    rr_offset: Option<u64>,
    qo_field: Option<usize>,
    rr_field: Option<usize>,
    base: u64,
) -> RarResult<bool> {
    let mut patched = false;
    if let (Some(qo), Some(field)) = (qo_offset, qo_field) {
        let field_bytes = vint_fixed5(qo.saturating_sub(base));
        if field + field_bytes.len() > hdr.len() {
            return Err(RarError::Format("locator field out of bounds".into()));
        }
        hdr[field..field + field_bytes.len()].copy_from_slice(&field_bytes);
        patched = true;
    }
    if let (Some(rr), Some(field)) = (rr_offset, rr_field) {
        let field_bytes = vint_fixed5(rr.saturating_sub(base));
        if field + field_bytes.len() > hdr.len() {
            return Err(RarError::Format("locator field out of bounds".into()));
        }
        hdr[field..field + field_bytes.len()].copy_from_slice(&field_bytes);
        patched = true;
    }
    if patched {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&hdr[4..]);
        let crc = hasher.finalize();
        hdr[..4].copy_from_slice(&crc.to_le_bytes());
    }
    Ok(patched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locator_record_size_includes_type_vint() {
        let body = [LOCATOR_FLAG_QUICK_OPEN as u8, 0, 0, 0, 0, 0];
        let record = frame_locator_record(&body);
        let (size, size_len) = vint::decode_from_slice(&record, 0).unwrap();
        let (_, type_len) = vint::decode_from_slice(&record, size_len).unwrap();

        assert_eq!(size as usize, type_len + body.len());
        assert_eq!(record.len(), size_len + size as usize);
    }
}
