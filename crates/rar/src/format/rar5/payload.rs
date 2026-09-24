//! Decoding members: the read-a-chunk / verify / decrypt / decode / verify
//! core shared by the single-volume, multi-volume and rewrite read paths.
//!
//! The only thing that differs between those paths is *where* the bytes come
//! from, so the chunk-reading source is pushed behind a [`ChunkReader`] seam
//! and the read+decrypt and decode+verify cores live here once.

use crate::codec::DecoderState;
use crate::crypto;
use crate::engine::DecryptedPayload;
use crate::error::{RarError, RarResult};
use crate::format::rar5::COMP_METHOD_STORE;
use crate::model::{DataChunk, FileHeader};

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// Read one chunk of a member from a volume.
pub(crate) trait ChunkReader {
    fn read_chunk(&mut self, vol: usize, offset: u64, len: u64) -> RarResult<Vec<u8>>;
}

/// Chunk source over an archive's primary stream (volume 0) plus the
/// sibling volume files — the extract path's reader.
pub(crate) struct StreamReader<'a> {
    pub stream: &'a mut Box<dyn crate::engine::ArchiveStream>,
    pub volume_paths: &'a [PathBuf],
}

impl ChunkReader for StreamReader<'_> {
    fn read_chunk(&mut self, vol: usize, offset: u64, len: u64) -> RarResult<Vec<u8>> {
        let mut buf = Vec::new();
        if vol == 0 {
            let stream = self.stream.as_mut();
            stream.seek(SeekFrom::Start(offset))?;
            stream.take(len).read_to_end(&mut buf)?;
        } else {
            let path = self.volume_paths.get(vol).ok_or_else(|| {
                RarError::format(format!("member data references missing volume {vol}"))
            })?;
            let mut f = std::fs::File::open(path)?;
            f.seek(SeekFrom::Start(offset))?;
            f.take(len).read_to_end(&mut buf)?;
        }
        Ok(buf)
    }
}

/// Chunk source over a single seekable file — the rewrite path's reader for
/// single-volume members.
pub(crate) struct SingleFileReader<'a> {
    pub reader: &'a mut std::fs::File,
}

impl ChunkReader for SingleFileReader<'_> {
    fn read_chunk(&mut self, vol: usize, offset: u64, len: u64) -> RarResult<Vec<u8>> {
        debug_assert_eq!(vol, 0);
        let _ = vol;
        self.reader.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::new();
        self.reader.take(len).read_to_end(&mut buf)?;
        Ok(buf)
    }
}

/// Convert a declared payload size to `usize`, rejecting lengths that do not
/// fit the host address space: on 32-bit targets `as usize` would silently
/// truncate them and defeat the surrounding size limits.
fn size_to_usize(size: u64, name: &str, what: &str) -> RarResult<usize> {
    usize::try_from(size).map_err(|_| {
        RarError::limit_exceeded(size, format!("{name}: {what} overflows host address space"))
    })
}

/// Read a member's full packed payload from the reader with per-chunk CRC
/// verification, decrypting when the header carries an encryption extra
/// record (keys derived once, reused for integrity verification).
pub(crate) fn read_packed<R: ChunkReader + ?Sized>(
    reader: &mut R,
    hdr: &FileHeader,
    chunks: &[DataChunk],
    name: &str,
    password: Option<&str>,
    max_packed: u64,
    cancel: impl Fn() -> RarResult<()>,
) -> RarResult<DecryptedPayload> {
    let mut total_packed = 0u64;
    for c in chunks {
        total_packed = total_packed.checked_add(c.packed_size).ok_or_else(|| {
            RarError::limit_exceeded(max_packed, format!("{name}: packed size overflow"))
        })?;
        if total_packed > max_packed {
            return Err(RarError::limit_exceeded(
                max_packed,
                format!("{name}: packed data {total_packed} bytes exceeds limit"),
            ));
        }
    }

    let packed_len = size_to_usize(total_packed, name, "packed size")?;
    let mut packed = Vec::new();
    packed.try_reserve_exact(packed_len).map_err(|_| {
        RarError::limit_exceeded(max_packed, format!("{name}: cannot allocate packed data"))
    })?;

    for chunk in chunks {
        cancel()?;
        let chunk_start = packed.len();
        packed.extend(reader.read_chunk(
            chunk.volume_index,
            chunk.data_offset,
            chunk.packed_size,
        )?);
        if !chunk.is_final
            && let Some(expected_crc) = chunk.crc32_val
        {
            let actual_crc = crc32fast::hash(&packed[chunk_start..]);
            if actual_crc != expected_crc {
                return Err(RarError::crc(
                    expected_crc,
                    actual_crc,
                    format!("{name} vol {}", chunk.volume_index),
                ));
            }
        }
    }

    // A chunk's declared packed size may run past the end of its volume (a
    // truncated or replaced volume file). `take(len).read_to_end` stops short
    // silently, so report the truncation here instead of letting a downstream
    // decode or checksum failure describe it.
    if packed.len() != packed_len {
        return Err(RarError::format(format!(
            "{name}: packed payload is truncated (read {} of {packed_len} bytes)",
            packed.len()
        )));
    }

    let params = if !hdr.extra_data.is_empty() {
        crypto::parse_encryption_extra(&hdr.extra_data)?
    } else {
        None
    };
    let keys = if let Some(ref p) = params {
        let password = password
            .ok_or_else(|| RarError::encrypted(format!("{name}: encrypted, no password set")))?;
        let keys = p
            .derive_and_verify(password)?
            .ok_or(RarError::WrongPassword)?;
        let mut data = crypto::decrypt_data(&packed, &keys.key, &p.iv)?;
        if hdr.comp_method == COMP_METHOD_STORE {
            let unp_size = size_to_usize(hdr.unpacked_size, name, "unpacked size")?;
            data.truncate(unp_size);
        }
        packed = data;
        Some(keys)
    } else {
        None
    };

    Ok(DecryptedPayload {
        data: packed,
        params,
        keys,
    })
}

/// Decode a member's decoded payload into `out` (STORE passes through,
/// compressed members go through the shared `DecoderState` window).
/// Returns the number of bytes written to `out`. Integrity (CRC32 and
/// BLAKE2sp, hash-key MAC'd when the encryption record requests it) is
/// verified by the caller over the written bytes / materialized buffer.
pub(crate) fn decode_member(
    hdr: &FileHeader,
    payload: &DecryptedPayload,
    state: Option<&mut DecoderState>,
    out: &mut dyn std::io::Write,
) -> RarResult<u64> {
    let written = if hdr.comp_method == COMP_METHOD_STORE {
        // Never emit more than the declared unpacked size: a crafted STORE
        // member whose packed area is larger than its unpacked size would
        // otherwise stream the excess to the caller before the mismatch
        // error below. A short payload still fails the size check.
        let declared = size_to_usize(hdr.unpacked_size, &hdr.name, "unpacked size")?;
        let take = payload.data.len().min(declared);
        out.write_all(&payload.data[..take]).map_err(RarError::Io)?;
        if payload.data.len() > declared {
            return Err(RarError::format(format!(
                "member {}: stored payload has {} bytes, header declares {}",
                hdr.name,
                payload.data.len(),
                hdr.unpacked_size
            )));
        }
        take as u64
    } else {
        crate::codec::decode_to_writer(
            &payload.data,
            hdr.unpacked_size,
            crate::codec::DecodeOptions {
                dict_size_log: hdr.comp_dict_size,
                dict_size_bytes: hdr.dict_size_bytes,
                variant: crate::version::ArchiveVersion::from_v70(hdr.comp_version == 1),
                state,
            },
            out,
        )?
    };
    if written != hdr.unpacked_size {
        // A short STORE payload (or a packed stream that stopped early) must
        // not surface as a silently truncated member even when the stored CRC
        // was recomputed over the truncated data.
        return Err(RarError::format(format!(
            "member {}: decoded {written} bytes, header declares {}",
            hdr.name, hdr.unpacked_size
        )));
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_payload_longer_than_declared_does_not_emit_excess() {
        let hdr = FileHeader {
            name: "long.bin".into(),
            unpacked_size: 4,
            comp_method: COMP_METHOD_STORE,
            ..Default::default()
        };
        let payload = DecryptedPayload {
            data: b"abcdefgh".to_vec(),
            params: None,
            keys: None,
        };
        let mut out = Vec::new();
        let err = decode_member(&hdr, &payload, None, &mut out).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "got {err}");
        assert!(
            out.len() <= 4,
            "excess bytes reached the writer: {} bytes",
            out.len()
        );
    }

    #[test]
    fn store_payload_padded_to_declared_size_writes_declared_bytes() {
        // The encrypted-read path trims AES zero-fill padding to the
        // declared unpacked size; the STORE branch must copy it through
        // unchanged (padding bytes included) and report the full size.
        let hdr = FileHeader {
            name: "padded.bin".into(),
            unpacked_size: 4,
            comp_method: COMP_METHOD_STORE,
            ..Default::default()
        };
        let payload = DecryptedPayload {
            data: b"ab\0\0".to_vec(),
            params: None,
            keys: None,
        };
        let mut out = Vec::new();
        let written = decode_member(&hdr, &payload, None, &mut out).unwrap();
        assert_eq!(written, 4);
        assert_eq!(out, b"ab\0\0");
    }

    #[test]
    fn store_payload_shorter_than_declared_is_rejected() {
        let hdr = FileHeader {
            name: "short.bin".into(),
            unpacked_size: 4,
            comp_method: COMP_METHOD_STORE,
            ..Default::default()
        };
        let payload = DecryptedPayload {
            data: b"ab".to_vec(),
            params: None,
            keys: None,
        };
        let mut out = Vec::new();
        let err = decode_member(&hdr, &payload, None, &mut out).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "got {err}");
    }

    #[test]
    fn declared_sizes_must_fit_the_host_address_space() {
        assert_eq!(size_to_usize(4096, "m", "packed size").unwrap(), 4096);
        let over_32_bit = u64::from(u32::MAX) + 1;
        if cfg!(target_pointer_width = "64") {
            // 64-bit hosts can represent the value; only 32-bit targets can
            // execute the rejection arm below.
            assert_eq!(
                size_to_usize(over_32_bit, "m", "packed size").unwrap(),
                usize::try_from(over_32_bit).unwrap()
            );
        } else {
            let err = size_to_usize(over_32_bit, "m", "packed size").unwrap_err();
            assert!(matches!(err, RarError::LimitExceeded { .. }), "got {err}");
        }
    }
}
