//! The main archive header (and its locator record, type 0x01), owned here
//! once so the byte rules (record layout, preallocated offset fields,
//! relative offset patching, CRC recompute) live in a single place.
//! The create/close path (`archive/create.rs`) and the surgical rewrite path
//! (`archive/transaction.rs`) both build through [`build_main_header`].

use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::{ArchiveHeader, vint_fixed};
use crate::vint;

/// Locator record type (extra record type 0x01).
pub(crate) const LOCATOR_TYPE: u64 = 0x01;
pub(crate) const LOCATOR_FLAG_QUICK_OPEN: u64 = 0x0001;
pub(crate) const LOCATOR_FLAG_RECOVERY: u64 = 0x0002;

/// Offset-field width used when the caller gives no size estimate (the
/// historical rar-rs default). A 5-byte vint names offsets below 32 GiB.
pub(crate) const DEFAULT_OFFSET_WIDTH: usize = 5;

/// The offset-field width WinRAR reserves for an archive it expects to reach
/// `estimate` bytes: 3 bytes below 512, then one more per 7-bit boundary
/// (4 / 5 / 6 for < 2^16 / < 2^23 / >= 2^23). WinRAR derives the estimate
/// from the planned member set before writing the main header and patches the
/// real offsets in at the end; its estimate is an internal heuristic (a
/// per-member total that grows with the member name), so a caller that wants
/// the same byte layout must supply a matching estimate.
pub(crate) fn locator_offset_width(estimate: u64) -> usize {
    match estimate {
        ..0x200 => 3,
        0x200..0x10000 => 4,
        0x10000..0x800000 => 5,
        _ => 6,
    }
}

/// Build the locator record body `[flags vint][qo offset vint][rr offset
/// vint]`, returning the body bytes and the positions (relative to the body
/// start) of the preallocated QO and RR offset fields. `offset_width` is the
/// reserved field width (the QO and RR fields always share one width, like
/// WinRAR).
///
/// WinRAR always writes the locator and always includes the QO offset field —
/// with the QO flag set and the offset 0 when the archive has no quick-open
/// record (its console `a` writes one only for larger archives) — while the
/// RR field appears only when a recovery record exists. Readers treat a 0 QO
/// offset as "no usable record" and fall back to a full scan, so the
/// placeholder is inert.
pub(crate) fn build_locator_body(
    quick_open: bool,
    recovery: bool,
    offset_width: usize,
) -> (Vec<u8>, Option<usize>, Option<usize>) {
    let _ = quick_open;
    let mut flags = LOCATOR_FLAG_QUICK_OPEN;
    if recovery {
        flags |= LOCATOR_FLAG_RECOVERY;
    }
    let mut body = Vec::new();
    body.extend(vint::encode(flags));
    // The QO field is always present; `quick_open` only says whether a real
    // offset will be patched into it.
    let qo = {
        let p = body.len();
        body.extend_from_slice(&vint_fixed(0, offset_width));
        Some(p)
    };
    let rr = if recovery {
        let p = body.len();
        body.extend_from_slice(&vint_fixed(0, offset_width));
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
/// or empty for a fresh archive); the locator record is always appended after
/// those records, like WinRAR. `quick_open` says whether a real quick-open
/// offset will be patched into the (always present) QO field; `recovery`
/// controls the RR field. `volume_number` selects the volume-header shape
/// through [`ArchiveHeader::to_bytes`]: the `MHD_VOLUME`/`VOLUME_NUM` flags
/// and the volume-number field.
///
/// Returns the framed header plus the header-relative offsets (including
/// the CRC and the size vint) of the preallocated QO and RR offset fields;
/// the RR field is `None` without a recovery record. The offsets feed
/// [`patch_locator_fields`] at close time, so no caller has to sum field
/// widths by hand.
pub(crate) fn build_main_header(
    arch_flags: u64,
    extra: &[u8],
    quick_open: bool,
    recovery: bool,
    volume_number: Option<u64>,
    offset_width: usize,
) -> (Vec<u8>, Option<usize>, Option<usize>) {
    let (locator, qo_pos, rr_pos) = build_locator_body(quick_open, recovery, offset_width);
    let mut all_extra = extra.to_vec();
    let record = frame_locator_record(&locator);
    // The record is [record size vint][type vint][body]; the body is its
    // tail, so it starts this far into the record.
    let start = all_extra.len() + record.len() - locator.len();
    all_extra.extend(record);

    let ah = ArchiveHeader {
        flags: arch_flags,
        extra_data: all_extra,
        volume_number,
    };
    let hdr = ah.to_bytes();
    // The extra area is the last body field, so it starts this many bytes
    // before the end of the framed header.
    let extra_base = hdr.len() - ah.extra_data.len();

    let qo_field = qo_pos.map(|p| extra_base + start + p);
    let rr_field = rr_pos.map(|p| extra_base + start + p);
    (hdr, qo_field, rr_field)
}

/// Patch the preallocated locator offset fields in a plaintext main archive
/// header in place and recompute the header CRC. Offsets are stored relative
/// to `base` (the archive start after the signature, plus any SFX stub).
/// Returns whether any field was patched (and thus the CRC rewritten).
///
/// Each field's width is read back from the placeholder already on disk (see
/// [`build_locator_body`]), so a narrow reservation is never overrun. An
/// offset the reserved width cannot hold would wrap and name arbitrary bytes,
/// so the sentinel 0 is written instead: quick-open then treats the record as
/// unusable and falls back to a full scan.
pub(crate) fn patch_locator_fields(
    hdr: &mut [u8],
    qo_offset: Option<u64>,
    rr_offset: Option<u64>,
    qo_field: Option<usize>,
    rr_field: Option<usize>,
    base: u64,
) -> RarResult<bool> {
    let mut patched = false;
    for (offset, field) in [(qo_offset, qo_field), (rr_offset, rr_field)] {
        let (Some(value), Some(field)) = (offset, field) else {
            continue;
        };
        let width = placeholder_width(hdr, field)
            .ok_or_else(|| RarError::format("locator field out of bounds"))?;
        let max = if width >= 10 {
            u64::MAX
        } else {
            (1u64 << (7 * width)) - 1
        };
        let rel = value.saturating_sub(base);
        let field_bytes = vint_fixed(if rel <= max { rel } else { 0 }, width);
        hdr[field..field + width].copy_from_slice(&field_bytes);
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

/// Width in bytes of the vint placeholder starting at `field`, or `None` when
/// the field runs past the header.
fn placeholder_width(hdr: &[u8], field: usize) -> Option<usize> {
    for n in 1..=10 {
        match hdr.get(field + n - 1) {
            Some(&byte) if byte & 0x80 != 0 => continue,
            Some(_) => return Some(n),
            None => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::rar5::headers::{
        ArchiveHeader, BlockMeta, locator_quick_open_offset, main_header_locator_fields,
        parse_block_bytes, split_main_extra,
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
                build_main_header(0, &extra, quick_open, recovery, None, DEFAULT_OFFSET_WIDTH);
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
        let (mut hdr, qo_field, rr_field) =
            build_main_header(0, &[], true, true, None, DEFAULT_OFFSET_WIDTH);
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

    /// An offset beyond the 5-byte vint's 35 bits must land as the 0
    /// sentinel (quick-open falls back to a full scan), not wrap.
    #[test]
    fn patched_offsets_beyond_35_bits_use_the_sentinel() {
        let (mut hdr, qo_field, _) =
            build_main_header(0, &[], true, false, None, DEFAULT_OFFSET_WIDTH);
        let huge = (1u64 << 40) + 1234;
        patch_locator_fields(&mut hdr, Some(huge), None, qo_field, None, 0).unwrap();
        let raw = parse_block_bytes(&hdr).unwrap();
        let ah = ArchiveHeader::from_raw(&raw).unwrap();
        let parsed = crate::format::rar5::headers::locator_quick_open_offset(&ah.extra_data);
        assert_eq!(
            parsed,
            Some(0),
            "an unrepresentable offset uses the sentinel"
        );
        assert_ne!(parsed, Some(huge & ((1 << 35) - 1)), "must not wrap");
    }

    #[test]
    fn build_main_header_volume_shape_matches_the_archive_header() {
        let (hdr, qo_field, rr_field) = build_main_header(
            crate::format::rar5::ARCHIVE_FLAG_VOLUME,
            &[],
            false,
            false,
            Some(2),
            DEFAULT_OFFSET_WIDTH,
        );
        // The locator is always written, so its QO field exists (left at 0);
        // the RR field only exists with a recovery record.
        assert!(qo_field.is_some());
        assert_eq!(rr_field, None);

        let raw = parse_block_bytes(&hdr).unwrap();
        let ah = ArchiveHeader::from_raw(&raw).unwrap();
        assert_eq!(ah.volume_number, Some(2));
        assert_ne!(ah.flags & crate::format::rar5::ARCHIVE_FLAG_VOLUME_NUM, 0);
    }

    /// The locator is written even for an archive with no quick-open and no
    /// recovery record, like WinRAR: the QO flag is set and its offset stays 0
    /// (the inert placeholder the reader treats as "no record").
    #[test]
    fn locator_is_always_present_with_a_zero_qo_placeholder() {
        let (hdr, qo_field, rr_field) =
            build_main_header(0, &[], false, false, None, DEFAULT_OFFSET_WIDTH);
        assert!(qo_field.is_some());
        assert!(rr_field.is_none());

        let raw = parse_block_bytes(&hdr).unwrap();
        let ah = ArchiveHeader::from_raw(&raw).unwrap();
        let (had_qo, had_rr, rest) = split_main_extra(&ah.extra_data).unwrap();
        assert!(!had_qo, "a 0 placeholder is not a quick-open record");
        assert!(!had_rr);
        assert!(rest.is_empty(), "only the locator was written");
        assert_eq!(locator_quick_open_offset(&ah.extra_data), Some(0));
    }

    #[test]
    fn offset_width_matches_winrar_buckets() {
        assert_eq!(locator_offset_width(0), 3);
        assert_eq!(locator_offset_width(511), 3);
        assert_eq!(locator_offset_width(512), 4);
        assert_eq!(locator_offset_width(65535), 4);
        assert_eq!(locator_offset_width(65536), 5);
        assert_eq!(locator_offset_width(0x7F_FFFF), 5);
        assert_eq!(locator_offset_width(0x80_0000), 6);
    }

    /// A narrow reservation (what a small size estimate selects) still patches
    /// and reads back, and a value that cannot fit its width uses the sentinel
    /// instead of wrapping.
    #[test]
    fn narrow_reservation_patches_and_reads_back() {
        for width in [3usize, 4, 6] {
            let (mut hdr, qo_field, _) = build_main_header(0, &[], true, false, None, width);
            let fits = (1u64 << (7 * width)) - 1;
            patch_locator_fields(&mut hdr, Some(fits), None, qo_field, None, 0).unwrap();
            let raw = parse_block_bytes(&hdr).unwrap();
            let ah = ArchiveHeader::from_raw(&raw).unwrap();
            assert_eq!(
                crate::format::rar5::headers::locator_quick_open_offset(&ah.extra_data),
                Some(fits),
                "a value filling width {width} must round-trip"
            );

            let (mut hdr, qo_field, _) = build_main_header(0, &[], true, false, None, width);
            patch_locator_fields(&mut hdr, Some(1u64 << (7 * width)), None, qo_field, None, 0)
                .unwrap();
            let raw = parse_block_bytes(&hdr).unwrap();
            let ah = ArchiveHeader::from_raw(&raw).unwrap();
            assert_eq!(
                crate::format::rar5::headers::locator_quick_open_offset(&ah.extra_data),
                Some(0),
                "a value past width {width} must use the sentinel"
            );
        }
    }
}
