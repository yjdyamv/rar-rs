//! RAR 1.5–4.x member read + decode.
//!
//! Reads a member's packed payload (single volume), decrypts it with the
//! cipher selected by `unp_ver` (15 → RAR15 stream, 20/26 → RAR20 block, and
//! 29+ → RAR30 AES-CBC block), then decodes it. STORE members pass through
//! raw; compressed members decode through the appropriate codec. Solid chains
//! share one decoder instance passed in by the caller via [`super::LegacyDecoder`].

use crate::codec::rar15::Rar15Decoder;
use crate::codec::rar20::Rar20Decoder;
use crate::codec::rar29::Rar29Decoder;
use crate::crc32;
use crate::crypto::{Rar15Cipher, Rar20Cipher, Rar30Cipher};
use crate::error::{RarError, RarResult};
use crate::rar50::headers::{DataChunk, FileHeader};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use super::LegacyDecoder;

/// Decode a single-volume RAR4 member into memory.
///
/// `chunks` names the member's volume segments (one per volume for split
/// members); the packed payload is the concatenation of those segments,
/// read from `volume_paths[chunk.volume_index]` (volume 0 = `stream`).
/// `decoder` carries persistent solid-chain state (`None` for a standalone
/// member). The returned Vec holds exactly this member's unpacked bytes.
/// Counts bytes and hashes them (standard CRC-32) as they stream past, so
/// a member's integrity can be verified without buffering its output.
struct CrcWriter<'a, W: ?Sized + std::io::Write> {
    inner: &'a mut W,
    hasher: crc32fast::Hasher,
    count: u64,
}

impl<W: ?Sized + std::io::Write> std::io::Write for CrcWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Streaming variant of [`decode_member_bytes`]: the decoded member is
/// written to `writer` with bounded memory instead of being accumulated.
/// Returns `(bytes written, CRC-32 of the output)` so the caller can verify
/// integrity without re-reading.
///
/// STORE members stream their chunks straight out (no copy of the payload);
/// compressed members read the (small) packed stream whole, then decode
/// through the shared decoder incrementally. Encrypted compressed members
/// decrypt the packed stream in place first, as before.
pub(crate) fn decode_member_bytes_to(
    stream: &mut (impl Read + Seek),
    volume_paths: &[PathBuf],
    chunks: &[DataChunk],
    hdr: &FileHeader,
    password: Option<&str>,
    decoder: Option<&mut LegacyDecoder>,
    writer: &mut dyn std::io::Write,
) -> RarResult<(u64, u32)> {
    let mut crc_writer = CrcWriter {
        inner: writer,
        hasher: crc32fast::Hasher::new(),
        count: 0,
    };
    {
        let writer: &mut dyn std::io::Write = &mut crc_writer;
        decode_member_bytes_to_inner(stream, volume_paths, chunks, hdr, password, decoder, writer)?;
    }
    Ok((crc_writer.count, crc_writer.hasher.clone().finalize()))
}

fn decode_member_bytes_to_inner(
    stream: &mut (impl Read + Seek),
    volume_paths: &[PathBuf],
    chunks: &[DataChunk],
    hdr: &FileHeader,
    password: Option<&str>,
    decoder: Option<&mut LegacyDecoder>,
    writer: &mut dyn std::io::Write,
) -> RarResult<()> {
    if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
        return Ok(());
    }
    let encrypted = hdr.flags & super::FHD_PASSWORD as u64 != 0;
    let unp_size = hdr.unpacked_size as usize;

    if super::is_stored(hdr.comp_method) && !encrypted {
        for chunk in chunks {
            let mut remaining = chunk.packed_size as usize;
            let mut buffer = vec![0u8; 1 << 20];
            let mut source: Box<dyn Read> = if chunk.volume_index == 0 {
                stream.seek(SeekFrom::Start(chunk.data_offset))?;
                Box::new(stream.by_ref())
            } else {
                let mut f =
                    std::fs::File::open(volume_paths.get(chunk.volume_index).ok_or_else(
                        || RarError::Format("RAR4: chunk volume out of range".into()),
                    )?)?;
                f.seek(SeekFrom::Start(chunk.data_offset))?;
                Box::new(f)
            };
            while remaining > 0 {
                let take = remaining.min(buffer.len());
                source
                    .read_exact(&mut buffer[..take])
                    .map_err(RarError::Io)?;
                writer.write_all(&buffer[..take]).map_err(RarError::Io)?;
                remaining -= take;
            }
        }
        return Ok(());
    }

    let mut packed = Vec::new();
    for chunk in chunks {
        let mut segment = vec![0u8; chunk.packed_size as usize];
        if chunk.volume_index == 0 {
            stream.seek(SeekFrom::Start(chunk.data_offset))?;
            stream.read_exact(&mut segment).map_err(RarError::Io)?;
        } else {
            let mut f = std::fs::File::open(
                volume_paths
                    .get(chunk.volume_index)
                    .ok_or_else(|| RarError::Format("RAR4: chunk volume out of range".into()))?,
            )?;
            f.seek(SeekFrom::Start(chunk.data_offset))?;
            f.read_exact(&mut segment).map_err(RarError::Io)?;
        }
        packed.extend_from_slice(&segment);
    }

    if encrypted {
        let password = password
            .ok_or_else(|| {
                RarError::Encrypted(format!(
                    "{}: encrypted member, no password provided",
                    hdr.name
                ))
            })?
            .as_bytes();
        decrypt_in_place(hdr, password, &mut packed)?;
        if super::is_stored(hdr.comp_method) {
            if packed.len() > unp_size {
                packed.truncate(unp_size);
            }
            writer.write_all(&packed).map_err(RarError::Io)?;
            return Ok(());
        }
    }

    if hdr.unp_ver >= 29 {
        return match decoder {
            Some(LegacyDecoder::Rar29(dec)) => dec
                .decode_member_streaming_to(&packed, hdr.unpacked_size, writer)
                .map_err(|e| map_codec_error(hdr, e)),
            Some(_) => Err(RarError::Format(
                "RAR4: unp_ver >= 29 but wrong decoder type in solid chain".into(),
            )),
            None => crate::codec::rar29::Rar29Decoder::new()
                .decode_member_streaming_to(&packed, hdr.unpacked_size, writer)
                .map_err(|e| map_codec_error(hdr, e)),
        };
    }
    if hdr.unp_ver == 20 || hdr.unp_ver == 26 {
        return match decoder {
            Some(LegacyDecoder::Rar20(dec)) => dec
                .decode_member_streaming_to(&packed, hdr.unpacked_size, writer)
                .map_err(|e| map_codec_error(hdr, e)),
            Some(_) => Err(RarError::Format(
                "RAR4: unp_ver 20/26 but wrong decoder type in solid chain".into(),
            )),
            None => crate::codec::rar20::Rar20Decoder::new()
                .decode_member_streaming_to(&packed, hdr.unpacked_size, writer)
                .map_err(|e| map_codec_error(hdr, e)),
        };
    }
    if hdr.unp_ver == 15 {
        let solid = decoder.is_some();
        let dec: &mut crate::codec::rar15::Rar15Decoder = match decoder {
            Some(LegacyDecoder::Rar15(dec)) => dec,
            Some(_) => {
                return Err(RarError::Format(
                    "RAR4: unp_ver 15 but wrong decoder type in solid chain".into(),
                ));
            }
            None => {
                return crate::codec::rar15::Rar15Decoder::new()
                    .decode_member_to(&packed, unp_size, false, writer)
                    .map_err(|error| {
                        let message = match error {
                            crate::codec::rar15::Error::NeedMoreInput => {
                                "RAR 1.5 stream is truncated"
                            }
                            crate::codec::rar15::Error::InvalidData(message) => message,
                        };
                        map_codec_error(hdr, RarError::Format(format!("RAR 1.5 stream: {message}")))
                    });
            }
        };
        return dec
            .decode_member_to(&packed, unp_size, solid, writer)
            .map_err(|error| {
                let message = match error {
                    crate::codec::rar15::Error::NeedMoreInput => "RAR 1.5 stream is truncated",
                    crate::codec::rar15::Error::InvalidData(message) => message,
                };
                map_codec_error(hdr, RarError::Format(format!("RAR 1.5 stream: {message}")))
            });
    }
    Err(RarError::Unsupported(format!(
        "RAR 1.3/1.4-era compressed members (unpack version {}) are not yet supported",
        hdr.unp_ver
    )))
}

pub(crate) fn decode_member_bytes(
    stream: &mut (impl Read + Seek),
    volume_paths: &[PathBuf],
    chunks: &[DataChunk],
    hdr: &FileHeader,
    password: Option<&str>,
    decoder: Option<&mut LegacyDecoder>,
) -> RarResult<Vec<u8>> {
    if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
        return Ok(Vec::new());
    }

    let mut packed = Vec::new();
    for chunk in chunks {
        let mut segment = vec![0u8; chunk.packed_size as usize];
        if chunk.volume_index == 0 {
            stream.seek(SeekFrom::Start(chunk.data_offset))?;
            stream.read_exact(&mut segment).map_err(RarError::Io)?;
        } else {
            let mut f = std::fs::File::open(
                volume_paths
                    .get(chunk.volume_index)
                    .ok_or_else(|| RarError::Format("RAR4: chunk volume out of range".into()))?,
            )?;
            f.seek(SeekFrom::Start(chunk.data_offset))?;
            f.read_exact(&mut segment).map_err(RarError::Io)?;
        }
        packed.extend_from_slice(&segment);
    }

    let encrypted = hdr.flags & super::FHD_PASSWORD as u64 != 0;
    if encrypted {
        let password = password
            .ok_or_else(|| {
                RarError::Encrypted(format!(
                    "{}: encrypted member, no password provided",
                    hdr.name
                ))
            })?
            .as_bytes();
        decrypt_in_place(hdr, password, &mut packed)?;
    }

    let unp_size = hdr.unpacked_size as usize;
    if super::is_stored(hdr.comp_method) {
        // STORE: encrypted payload includes block padding, and STORE never
        // expands, so trim to the unpacked size.
        if packed.len() > unp_size {
            packed.truncate(unp_size);
        }
        return Ok(packed);
    }

    if hdr.unp_ver >= 29 {
        let out = match decoder {
            Some(LegacyDecoder::Rar29(dec)) => dec.decode_member(&packed, hdr.unpacked_size),
            Some(_) => Err(RarError::Format(
                "RAR4: unp_ver >= 29 but wrong decoder type in solid chain".into(),
            )),
            None => Rar29Decoder::new().decode_member(&packed, hdr.unpacked_size),
        }
        .map_err(|e| map_codec_error(hdr, e))?;
        return Ok(out);
    }

    if hdr.unp_ver == 20 || hdr.unp_ver == 26 {
        let out = match decoder {
            Some(LegacyDecoder::Rar20(dec)) => dec.decode_member(&packed, hdr.unpacked_size),
            Some(_) => Err(RarError::Format(
                "RAR4: unp_ver 20/26 but wrong decoder type in solid chain".into(),
            )),
            None => Rar20Decoder::new().decode_member(&packed, hdr.unpacked_size),
        }
        .map_err(|e| map_codec_error(hdr, e))?;
        return Ok(out);
    }

    if hdr.unp_ver == 15 {
        let out = match decoder {
            Some(LegacyDecoder::Rar15(dec)) => dec.decode_member(&packed, hdr.unpacked_size, true),
            Some(_) => Err(RarError::Format(
                "RAR4: unp_ver 15 but wrong decoder type in solid chain".into(),
            )),
            None => Rar15Decoder::new().decode_member(&packed, hdr.unpacked_size, false),
        }
        .map_err(|e| map_codec_error(hdr, e))?;
        return Ok(out);
    }

    Err(RarError::Unsupported(format!(
        "RAR 1.3/1.4-era compressed members (unpack version {}) are not yet supported",
        hdr.unp_ver
    )))
}

/// Map codec-level errors to user-facing errors: encrypted members with a
/// password provided treat CRC/codec/crypto errors as
/// `WrongPassword` (mirrors rars `map_encrypted_payload_error`).
fn map_codec_error(hdr: &FileHeader, error: RarError) -> RarError {
    let encrypted = hdr.flags & super::FHD_PASSWORD as u64 != 0;
    if !encrypted {
        return error;
    }
    match error {
        RarError::Encrypted(_) => error,
        RarError::Crc { .. } | RarError::Format(_) | RarError::Unsupported(_) => {
            RarError::WrongPassword
        }
        other => other,
    }
}

/// Decrypt `data` in place according to the member's cipher.
pub(crate) fn decrypt_in_place(
    hdr: &FileHeader,
    password: &[u8],
    data: &mut [u8],
) -> RarResult<()> {
    match hdr.unp_ver {
        15 => {
            Rar15Cipher::new(password).crypt_in_place(data);
            Ok(())
        }
        20 | 26 => Rar20Cipher::new(password)
            .decrypt_in_place(data)
            .map_err(|e| RarError::Format(format!("RAR4 RAR20 decrypt: {e}"))),
        v if v >= 29 => {
            let mut cipher = Rar30Cipher::new(password, hdr.salt)
                .map_err(|e| RarError::Format(format!("RAR4 RAR30 key setup: {e}")))?;
            cipher
                .decrypt_in_place(data)
                .map_err(|e| RarError::Format(format!("RAR4 RAR30 decrypt: {e}")))
        }
        other => Err(RarError::Unsupported(format!(
            "RAR4 encryption unpack version {other} not supported"
        ))),
    }
}

/// Compute the RFC/standard CRC-32 of `data` for RAR4 integrity checking.
pub(crate) fn member_crc(data: &[u8]) -> u32 {
    crc32::crc32(data)
}
