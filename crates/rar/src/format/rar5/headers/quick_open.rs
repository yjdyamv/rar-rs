//! The quick-open ("QO") catalog codec: the payload layout is owned here
//! once, so the writer (`archive/create.rs`) and the reader
//! (`format/rar5/extract/open.rs`) cannot drift apart.
//!
//! Payload layout (repeated per member):
//! ```text
//! [entry CRC32] 4 bytes LE, over [body]
//! [body size] vint
//! [body] = [flags vint] [relative offset vint] [header size vint]
//!          [complete file-header block bytes]
//! ```
//! The relative offset points back from the QO record to the cached file
//! header; the flags field is always 0 (a file-header entry).

use crate::error::{RarError, RarResult};
use crate::format::rar5::vint;
use crate::format::shared::extract::check_entry_cap;

/// Convert a declared quick-open size to `usize`, rejecting lengths that do
/// not fit the host address space: on 32-bit targets `as usize` would
/// truncate `2^32 + N` to `N`, so the entry CRC would be verified over — and
/// the embedded header parsed from — a range other than the declared one.
fn size_to_usize(size: u64, what: &str) -> RarResult<usize> {
    usize::try_from(size).map_err(|_| RarError::LimitExceeded {
        limit: size,
        context: format!("quick-open: {what} overflows host address space"),
    })
}

/// Encode one catalog entry from the cached file-header block bytes and
/// their distance back from the QO record.
pub(crate) fn encode_entry(rel: u64, header: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(2 + header.len());
    body.extend(vint::encode(0u64)); // entry flags: file header
    body.extend(vint::encode(rel));
    body.extend(vint::encode(header.len() as u64));
    body.extend_from_slice(header);

    let mut entry = Vec::with_capacity(8 + body.len());
    entry.extend(crc32fast::hash(&body).to_le_bytes());
    entry.extend(vint::encode(body.len() as u64));
    entry.extend(body);
    entry
}

/// Decode a quick-open payload into `(relative offset, header block bytes)`
/// pairs. Every entry CRC is verified and every declared size must fit the
/// host address space; structural violations are errors (the caller falls
/// back to a full scan). `max_entries` bounds the decode: the payload limit
/// alone still admits millions of tiny entries, so the ceiling is enforced
/// before each entry is materialized.
pub(crate) fn decode_payload(payload: &[u8], max_entries: usize) -> RarResult<Vec<(u64, Vec<u8>)>> {
    let mut entries = Vec::new();
    let mut off = 0usize;
    while off < payload.len() {
        check_entry_cap(entries.len(), max_entries)?;
        if off + 4 > payload.len() {
            return Err(RarError::Format("quick-open: truncated entry CRC".into()));
        }
        let stored_crc = u32::from_le_bytes(payload[off..off + 4].try_into().unwrap());
        off += 4;
        let (body_size, n) = vint::decode_from_slice(payload, off)
            .map_err(|e| RarError::Format(format!("quick-open: {e}")))?;
        off += n;
        let body_len = size_to_usize(body_size, "entry body size")?;
        let body_end = off
            .checked_add(body_len)
            .ok_or_else(|| RarError::Format("quick-open: body size overflow".into()))?;
        if body_end > payload.len() {
            return Err(RarError::Format("quick-open: truncated entry body".into()));
        }
        let actual = crc32fast::hash(&payload[off..body_end]);
        if actual != stored_crc {
            return Err(RarError::Crc {
                expected: stored_crc,
                actual,
                context: "quick-open entry".into(),
            });
        }

        let mut p = off;
        // flags vint (writer always emits 0 = file header)
        let (_, fn_) = vint::decode_from_slice(payload, p)
            .map_err(|e| RarError::Format(format!("quick-open: {e}")))?;
        p += fn_;
        let (rel, rn) = vint::decode_from_slice(payload, p)
            .map_err(|e| RarError::Format(format!("quick-open: {e}")))?;
        p += rn;
        let (hdr_size, hn) = vint::decode_from_slice(payload, p)
            .map_err(|e| RarError::Format(format!("quick-open: {e}")))?;
        p += hn;
        let hdr_len = size_to_usize(hdr_size, "file header size")?;
        let hdr_end = p
            .checked_add(hdr_len)
            .ok_or_else(|| RarError::Format("quick-open: header size overflow".into()))?;
        if hdr_end > body_end {
            return Err(RarError::Format("quick-open: truncated file header".into()));
        }

        entries.push((rel, payload[p..hdr_end].to_vec()));
        off = body_end;
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::rar5::headers::FileHeader;

    fn header(name: &str) -> Vec<u8> {
        FileHeader {
            name: name.into(),
            ..Default::default()
        }
        .to_bytes()
    }

    #[test]
    fn entries_round_trip_through_encode_and_decode() {
        let a = header("a.txt");
        let b = header("nested/b.bin");
        let mut payload = encode_entry(0, &a);
        payload.extend(encode_entry(4096, &b));

        let decoded = decode_payload(&payload, usize::MAX).unwrap();
        assert_eq!(decoded, vec![(0, a), (4096, b)]);
    }

    #[test]
    fn entry_cap_bounds_the_decode() {
        let a = header("a.txt");
        let mut payload = encode_entry(0, &a);
        payload.extend(encode_entry(1, &a));

        assert_eq!(decode_payload(&payload, 2).unwrap().len(), 2);
        let err = decode_payload(&payload, 1).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "unexpected: {err:?}");
    }

    #[test]
    fn corrupt_entry_crc_is_rejected() {
        let mut payload = encode_entry(7, &header("a.txt"));
        payload[0] ^= 0xFF;
        let err = decode_payload(&payload, usize::MAX).unwrap_err();
        assert!(matches!(err, RarError::Crc { .. }), "unexpected: {err:?}");
    }

    #[test]
    fn oversized_declared_sizes_are_rejected() {
        assert_eq!(size_to_usize(4096, "entry body size").unwrap(), 4096);
        let over_32_bit = u64::from(u32::MAX) + 1;
        if cfg!(target_pointer_width = "64") {
            // 64-bit hosts can represent the value; only 32-bit targets can
            // execute the rejection arm below.
            assert_eq!(
                size_to_usize(over_32_bit, "entry body size").unwrap(),
                usize::try_from(over_32_bit).unwrap()
            );
        } else {
            let err = size_to_usize(over_32_bit, "entry body size").unwrap_err();
            assert!(matches!(err, RarError::LimitExceeded { .. }), "got {err}");
        }
    }
}
