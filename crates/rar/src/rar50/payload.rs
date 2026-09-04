//! Decoding members: the read-a-chunk / verify / decrypt / decode / verify
//! core shared by the single-volume, multi-volume and rewrite read paths.
//!
//! The only thing that differs between those paths is *where* the bytes come
//! from, so the chunk-reading source is pushed behind a [`ChunkReader`] seam
//! and the read+decrypt and decode+verify cores live here once.

use crate::archive::DecryptedPayload;
use crate::codec::DecoderState;
use crate::crypto;
use crate::error::{RarError, RarResult};
use crate::model::{DataChunk, FileHeader};
use crate::rar50::*;

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// Read one chunk of a member from a volume.
pub(crate) trait ChunkReader {
    fn read_chunk(&mut self, vol: usize, offset: u64, len: u64) -> RarResult<Vec<u8>>;
}

/// Chunk source over an archive's primary stream (volume 0) plus the
/// sibling volume files — the extract path's reader.
pub(crate) struct StreamReader<'a> {
    pub stream: &'a mut Box<dyn crate::archive::ArchiveStream>,
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
            let mut f = std::fs::File::open(&self.volume_paths[vol])?;
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
        total_packed =
            total_packed
                .checked_add(c.packed_size)
                .ok_or_else(|| RarError::LimitExceeded {
                    limit: max_packed,
                    context: format!("{name}: packed size overflow"),
                })?;
        if total_packed > max_packed {
            return Err(RarError::LimitExceeded {
                limit: max_packed,
                context: format!("{name}: packed data {total_packed} bytes exceeds limit"),
            });
        }
    }

    let mut packed = Vec::new();
    packed
        .try_reserve_exact(total_packed as usize)
        .map_err(|_| RarError::LimitExceeded {
            limit: max_packed,
            context: format!("{name}: cannot allocate packed data"),
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
                return Err(RarError::Crc {
                    expected: expected_crc,
                    actual: actual_crc,
                    context: format!("{name} vol {}", chunk.volume_index),
                });
            }
        }
    }

    let params = if !hdr.extra_data.is_empty() {
        crypto::parse_encryption_extra(&hdr.extra_data)?
    } else {
        None
    };
    let keys = if let Some(ref p) = params {
        let password = password
            .ok_or_else(|| RarError::Encrypted(format!("{name}: encrypted, no password set")))?;
        if !p.verify_password(password) {
            return Err(RarError::WrongPassword);
        }
        let keys = p.derive_keys(password)?;
        let mut data = crypto::decrypt_data(&packed, &keys.key, &p.iv)?;
        if hdr.comp_method == COMP_METHOD_STORE {
            data.truncate(hdr.unpacked_size as usize);
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
        out.write_all(&payload.data).map_err(RarError::Io)?;
        payload.data.len() as u64
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
    Ok(written)
}
