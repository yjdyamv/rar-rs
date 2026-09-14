//! The main archive header (and its locator record, type 0x01), owned here
//! once so the byte rules (record layout, fixed-5-byte preallocated offset
//! fields, relative offset patching, CRC recompute) live in a single place.
//! The create/close path (`archive/create.rs`) and the surgical rewrite path
//! (`archive/transaction.rs`) both build through [`build_main_header`].

use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::{ArchiveHeader, vint_fixed5};
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

/// Build a complete main archive header and frame it for the wire.
///
/// `extra` is the archive-header extra area (an existing archive's records,
/// or empty for a fresh archive); when `quick_open` or `recovery` is set,
/// the locator record is appended after those records. `volume_number`
/// selects the volume-header shape through [`ArchiveHeader::to_bytes`]: the
/// `MHD_VOLUME`/`VOLUME_NUM` flags and the volume-number field.
///
/// Returns the framed header plus the header-relative offsets (including
/// the CRC and the size vint) of the preallocated QO and RR offset fields;
/// absent fields are `None`. The offsets feed [`patch_locator_fields`] at
/// close time, so no caller has to sum field widths by hand.
pub(crate) fn build_main_header(
    arch_flags: u64,
    extra: &[u8],
    quick_open: bool,
    recovery: bool,
    volume_number: Option<u64>,
) -> (Vec<u8>, Option<usize>, Option<usize>) {
    let (locator, qo_pos, rr_pos) = build_locator_body(quick_open, recovery);
    let mut all_extra = extra.to_vec();
    let locator_body_start = if quick_open || recovery {
        // Locator record layout: [record size vint][type vint][body].
        let record_body = vint::encoded_size(LOCATOR_TYPE) + locator.len();
        let start = all_extra.len()
            + vint::encoded_size(record_body as u64)
            + vint::encoded_size(LOCATOR_TYPE);
        all_extra.extend(frame_locator_record(&locator));
        Some(start)
    } else {
        None
    };

    let ah = ArchiveHeader {
        flags: arch_flags,
        extra_data: all_extra,
        volume_number,
    };
    let hdr = ah.to_bytes();
    // The extra area is the last body field, so it starts this many bytes
    // before the end of the framed header.
    let extra_base = hdr.len() - ah.extra_data.len();

    let (qo_field, rr_field) = match locator_body_start {
        Some(start) => (
            qo_pos.map(|p| extra_base + start + p),
            rr_pos.map(|p| extra_base + start + p),
        ),
        None => (None, None),
    };
    (hdr, qo_field, rr_field)
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
    use crate::format::rar5::headers::{
        ArchiveHeader, BlockMeta, main_header_locator_fields, parse_block_bytes,
    };

    #[test]
    fn locator_record_size_includes_type_vint() {
        let body = [LOCATOR_FLAG_QUICK_OPEN as u8, 0, 0, 0, 0, 0];
        let record = frame_locator_record(&body);
        let (size, size_len) = vint::decode_from_slice(&record, 0).unwrap();
        let (_, type_len) = vint::decode_from_slice(&record, size_len).unwrap();

        assert_eq!(size as usize, type_len + body.len());
        assert_eq!(record.len(), size_len + size as usize);
    }

    /// The field offsets returned by [`build_main_header`] must be the ones
    /// the read-side locator parser computes, so patch and read agree.
    #[test]
    fn build_main_header_offsets_match_the_reader() {
        let extra = {
            let mut bytes = vint::encode(2u64); // an unrelated extra record
            bytes.extend(vint::encode(0x99u64));
            bytes
        };
        for (quick_open, recovery) in [(false, false), (true, false), (false, true), (true, true)] {
            let (hdr, qo_field, rr_field) =
                build_main_header(0, &extra, quick_open, recovery, None);
            let raw = parse_block_bytes(&hdr).unwrap();
            let (_, hsize_vint_len) = vint::decode_from_slice(&hdr, 4).unwrap();
            let meta = BlockMeta {
                block_type: raw.block_type,
                flags: raw.flags,
                block_start: 0,
                data_offset: raw.data_offset,
                data_end: raw.data_offset,
                header_bytes: hdr.clone(),
                hsize_vint_len,
                raw,
            };
            let (reader_qo, reader_rr) = main_header_locator_fields(&meta).unwrap();
            assert_eq!(reader_qo, qo_field, "QO field offset ({quick_open})");
            assert_eq!(reader_rr, rr_field, "RR field offset ({recovery})");

            let ah = ArchiveHeader::from_raw(&meta.raw).unwrap();
            assert!(ah.extra_data.starts_with(&extra));
        }
    }

    #[test]
    fn build_main_header_patch_round_trips_through_the_parser() {
        let (mut hdr, qo_field, rr_field) = build_main_header(0, &[], true, true, None);
        let patched =
            patch_locator_fields(&mut hdr, Some(1234), Some(5678), qo_field, rr_field, 0).unwrap();
        assert!(patched);

        let raw = parse_block_bytes(&hdr).unwrap();
        let ah = ArchiveHeader::from_raw(&raw).unwrap();
        assert_eq!(
            crate::format::rar5::headers::locator_quick_open_offset(&ah.extra_data),
            Some(1234)
        );
        let (_, hsize_vint_len) = vint::decode_from_slice(&hdr, 4).unwrap();
        let meta = BlockMeta {
            block_type: raw.block_type,
            flags: raw.flags,
            block_start: 0,
            data_offset: raw.data_offset,
            data_end: raw.data_offset,
            header_bytes: hdr,
            hsize_vint_len,
            raw,
        };
        let (qo, rr) = main_header_locator_fields(&meta).unwrap();
        assert_eq!(qo, qo_field);
        assert_eq!(rr, rr_field);
    }

    #[test]
    fn build_main_header_volume_shape_matches_the_archive_header() {
        let (hdr, qo_field, rr_field) = build_main_header(
            crate::format::rar5::ARCHIVE_FLAG_VOLUME,
            &[],
            false,
            false,
            Some(2),
        );
        assert_eq!((qo_field, rr_field), (None, None));

        let raw = parse_block_bytes(&hdr).unwrap();
        let ah = ArchiveHeader::from_raw(&raw).unwrap();
        assert_eq!(ah.volume_number, Some(2));
        assert_ne!(ah.flags & crate::format::rar5::ARCHIVE_FLAG_VOLUME_NUM, 0);
    }
}
