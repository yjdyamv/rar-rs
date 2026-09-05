//! RAR 1.5–4.x member read + decode.
//!
//! Reads a member's packed payload (single volume), decrypts it with the
//! cipher selected by `unp_ver` (15 → RAR15 stream, 20/26 → RAR20 block, and
//! 29+ → RAR30 AES-CBC block), then decodes it. STORE members pass through
//! raw; compressed members decode through the appropriate codec. Solid chains
//! share one decoder instance passed in by the caller via [`super::LegacyDecoder`].

use crate::codec::legacy::rar15::Rar15Decoder;
use crate::codec::legacy::rar20::Rar20Decoder;
use crate::codec::legacy::rar29::Rar29Decoder;
use crate::crc32;
use crate::crypto::{Rar15Cipher, Rar20Cipher, Rar30Cipher};
use crate::error::{RarError, RarResult};
use crate::model::{DataChunk, FileHeader};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use super::LegacyDecoder;

pub(crate) struct MemberDecodeOptions<'a> {
    pub password: Option<&'a str>,
    pub decoder: Option<&'a mut LegacyDecoder>,
    /// Guard for paths that aggregate the packed payload in a `Vec`.
    pub max_alloc_packed_bytes: u64,
    /// Guard for unencrypted STORE payloads copied directly to a writer.
    pub max_stream_packed_bytes: u64,
}

/// Decode a RAR4 member into memory, including split multi-volume members.
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
    limit: u64,
}

impl<W: ?Sized + std::io::Write> std::io::Write for CrcWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let requested = u64::try_from(buf.len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "RAR4 output length overflows u64",
            )
        })?;
        let remaining = self.limit.saturating_sub(self.count);
        if requested > remaining {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "RAR4 decoder attempted to write beyond declared unpacked size {}",
                    self.limit
                ),
            ));
        }
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.count = self.count.checked_add(n as u64).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "RAR4 output size overflow")
        })?;
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
    options: MemberDecodeOptions<'_>,
    writer: &mut dyn std::io::Write,
) -> RarResult<(u64, u32)> {
    let mut crc_writer = CrcWriter {
        inner: writer,
        hasher: crc32fast::Hasher::new(),
        count: 0,
        limit: hdr.unpacked_size,
    };
    {
        let writer: &mut dyn std::io::Write = &mut crc_writer;
        decode_member_bytes_to_inner(stream, volume_paths, chunks, hdr, options, writer)?;
    }
    validate_output_size(hdr, crc_writer.count)?;
    Ok((crc_writer.count, crc_writer.hasher.clone().finalize()))
}

fn decode_member_bytes_to_inner(
    stream: &mut (impl Read + Seek),
    volume_paths: &[PathBuf],
    chunks: &[DataChunk],
    hdr: &FileHeader,
    options: MemberDecodeOptions<'_>,
    writer: &mut dyn std::io::Write,
) -> RarResult<()> {
    let MemberDecodeOptions {
        password,
        decoder,
        max_alloc_packed_bytes,
        max_stream_packed_bytes,
    } = options;
    let encrypted = hdr.flags & super::FHD_PASSWORD as u64 != 0;
    let streams_store = super::is_stored(hdr.comp_method) && !encrypted;
    let packed_limit = if streams_store {
        max_stream_packed_bytes
    } else {
        max_alloc_packed_bytes
    };
    let packed_size = checked_packed_size(chunks, hdr, packed_limit)?;
    if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
        return Ok(());
    }

    if streams_store {
        if hdr.packed_size != hdr.unpacked_size {
            return Err(RarError::Format(format!(
                "RAR4: {}: STORE packed size {} does not match unpacked size {}",
                hdr.name, hdr.packed_size, hdr.unpacked_size
            )));
        }
        for chunk in chunks {
            let mut remaining = chunk.packed_size;
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
                let take = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
                    RarError::Format("RAR4: packed chunk size overflows usize".into())
                })?;
                source
                    .read_exact(&mut buffer[..take])
                    .map_err(RarError::Io)?;
                writer.write_all(&buffer[..take]).map_err(RarError::Io)?;
                remaining -= take as u64;
            }
        }
        return Ok(());
    }

    let unp_size = usize::try_from(hdr.unpacked_size).map_err(|_| RarError::LimitExceeded {
        limit: hdr.unpacked_size,
        context: format!("{}: unpacked size overflows host address space", hdr.name),
    })?;
    let packed_len = packed_len_for_allocation(hdr, packed_size, max_alloc_packed_bytes)?;
    let mut packed = read_packed_payload(
        stream,
        volume_paths,
        chunks,
        hdr,
        packed_len,
        max_alloc_packed_bytes,
    )?;

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
            if packed.len() < unp_size {
                return Err(RarError::Format(format!(
                    "RAR4: {}: encrypted STORE payload is shorter than declared unpacked size {}",
                    hdr.name, hdr.unpacked_size
                )));
            }
            packed.truncate(unp_size);
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
            None => crate::codec::legacy::rar29::Rar29Decoder::new()
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
            None => crate::codec::legacy::rar20::Rar20Decoder::new()
                .decode_member_streaming_to(&packed, hdr.unpacked_size, writer)
                .map_err(|e| map_codec_error(hdr, e)),
        };
    }
    if hdr.unp_ver == 15 {
        let solid = decoder.is_some();
        let dec: &mut crate::codec::legacy::rar15::Rar15Decoder = match decoder {
            Some(LegacyDecoder::Rar15(dec)) => dec,
            Some(_) => {
                return Err(RarError::Format(
                    "RAR4: unp_ver 15 but wrong decoder type in solid chain".into(),
                ));
            }
            None => {
                return crate::codec::legacy::rar15::Rar15Decoder::new()
                    .decode_member_to(&packed, unp_size, false, writer)
                    .map_err(|error| {
                        let message = match error {
                            crate::codec::legacy::rar15::Error::NeedMoreInput => {
                                "RAR 1.5 stream is truncated"
                            }
                            crate::codec::legacy::rar15::Error::InvalidData(message) => message,
                        };
                        map_codec_error(hdr, RarError::Format(format!("RAR 1.5 stream: {message}")))
                    });
            }
        };
        return dec
            .decode_member_to(&packed, unp_size, solid, writer)
            .map_err(|error| {
                let message = match error {
                    crate::codec::legacy::rar15::Error::NeedMoreInput => "RAR 1.5 stream is truncated",
                    crate::codec::legacy::rar15::Error::InvalidData(message) => message,
                };
                map_codec_error(hdr, RarError::Format(format!("RAR 1.5 stream: {message}")))
            });
    }
    Err(RarError::Unsupported(format!(
        "RAR 1.3/1.4-era compressed members (unpack version {}) are not yet supported",
        hdr.unp_ver
    )))
}

fn checked_packed_size(
    chunks: &[DataChunk],
    hdr: &FileHeader,
    max_packed_bytes: u64,
) -> RarResult<u64> {
    let total = chunks.iter().try_fold(0u64, |total, chunk| {
        total
            .checked_add(chunk.packed_size)
            .ok_or_else(|| RarError::Format(format!("RAR4: {}: packed size overflow", hdr.name)))
    })?;
    if total != hdr.packed_size {
        return Err(RarError::Format(format!(
            "RAR4: {}: chunk packed size {total} does not match header packed size {}",
            hdr.name, hdr.packed_size
        )));
    }
    if total > max_packed_bytes {
        return Err(RarError::LimitExceeded {
            limit: max_packed_bytes,
            context: format!(
                "{}: packed size {total} exceeds the extraction limit",
                hdr.name
            ),
        });
    }
    Ok(total)
}

fn packed_len_for_allocation(
    hdr: &FileHeader,
    packed_size: u64,
    max_packed_bytes: u64,
) -> RarResult<usize> {
    usize::try_from(packed_size).map_err(|_| RarError::LimitExceeded {
        limit: max_packed_bytes,
        context: format!("{}: packed size overflows host address space", hdr.name),
    })
}

fn read_packed_payload(
    stream: &mut (impl Read + Seek),
    volume_paths: &[PathBuf],
    chunks: &[DataChunk],
    hdr: &FileHeader,
    packed_len: usize,
    max_packed_bytes: u64,
) -> RarResult<Vec<u8>> {
    let mut packed = Vec::new();
    packed
        .try_reserve_exact(packed_len)
        .map_err(|_| RarError::LimitExceeded {
            limit: max_packed_bytes,
            context: format!(
                "{}: unable to reserve {packed_len} bytes for RAR4 packed payload",
                hdr.name
            ),
        })?;
    for chunk in chunks {
        let segment_len =
            usize::try_from(chunk.packed_size).map_err(|_| RarError::LimitExceeded {
                limit: max_packed_bytes,
                context: format!("{}: packed chunk overflows host address space", hdr.name),
            })?;
        let start = packed.len();
        let end = start
            .checked_add(segment_len)
            .ok_or_else(|| RarError::Format("RAR4: packed buffer size overflow".into()))?;
        if end > packed_len {
            return Err(RarError::Format(
                "RAR4: packed chunks exceed reserved size".into(),
            ));
        }
        packed.resize(end, 0);
        if chunk.volume_index == 0 {
            stream.seek(SeekFrom::Start(chunk.data_offset))?;
            stream
                .read_exact(&mut packed[start..end])
                .map_err(RarError::Io)?;
        } else {
            let mut file = std::fs::File::open(
                volume_paths
                    .get(chunk.volume_index)
                    .ok_or_else(|| RarError::Format("RAR4: chunk volume out of range".into()))?,
            )?;
            file.seek(SeekFrom::Start(chunk.data_offset))?;
            file.read_exact(&mut packed[start..end])
                .map_err(RarError::Io)?;
        }
    }
    Ok(packed)
}

fn validate_output_size(hdr: &FileHeader, actual: u64) -> RarResult<()> {
    if actual != hdr.unpacked_size {
        return Err(RarError::Format(format!(
            "RAR4: {}: decoded size {actual} does not match declared unpacked size {}",
            hdr.name, hdr.unpacked_size
        )));
    }
    Ok(())
}

pub(crate) fn decode_member_bytes(
    stream: &mut (impl Read + Seek),
    volume_paths: &[PathBuf],
    chunks: &[DataChunk],
    hdr: &FileHeader,
    options: MemberDecodeOptions<'_>,
) -> RarResult<Vec<u8>> {
    let MemberDecodeOptions {
        password,
        decoder,
        max_alloc_packed_bytes,
        max_stream_packed_bytes: _,
    } = options;
    let packed_size = checked_packed_size(chunks, hdr, max_alloc_packed_bytes)?;
    if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
        return Ok(Vec::new());
    }

    let packed_len = packed_len_for_allocation(hdr, packed_size, max_alloc_packed_bytes)?;
    let mut packed = read_packed_payload(
        stream,
        volume_paths,
        chunks,
        hdr,
        packed_len,
        max_alloc_packed_bytes,
    )?;

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

    let unp_size = usize::try_from(hdr.unpacked_size).map_err(|_| RarError::LimitExceeded {
        limit: hdr.unpacked_size,
        context: format!("{}: unpacked size overflows host address space", hdr.name),
    })?;
    if super::is_stored(hdr.comp_method) {
        if !encrypted && packed.len() != unp_size {
            return Err(RarError::Format(format!(
                "RAR4: {}: STORE packed size {} does not match unpacked size {}",
                hdr.name,
                packed.len(),
                hdr.unpacked_size
            )));
        }
        if packed.len() < unp_size {
            return Err(RarError::Format(format!(
                "RAR4: {}: STORE payload is shorter than declared unpacked size {}",
                hdr.name, hdr.unpacked_size
            )));
        }
        packed.truncate(unp_size);
        validate_output_size(hdr, packed.len() as u64)?;
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
        validate_output_size(hdr, out.len() as u64)?;
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
        validate_output_size(hdr, out.len() as u64)?;
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
        validate_output_size(hdr, out.len() as u64)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn store_header(packed_size: u64, unpacked_size: u64) -> FileHeader {
        FileHeader {
            name: "payload.bin".into(),
            packed_size,
            unpacked_size,
            comp_method: 0,
            format_version: 4,
            ..Default::default()
        }
    }

    fn options(max_packed_bytes: u64) -> MemberDecodeOptions<'static> {
        MemberDecodeOptions {
            password: None,
            decoder: None,
            max_alloc_packed_bytes: max_packed_bytes,
            max_stream_packed_bytes: max_packed_bytes,
        }
    }

    fn chunk(packed_size: u64) -> DataChunk {
        DataChunk {
            volume_index: 0,
            data_offset: 0,
            packed_size,
            crc32_val: None,
            is_final: true,
            extra_data: Vec::new(),
        }
    }

    #[test]
    fn store_stream_rejects_output_larger_than_declared_size() {
        let header = store_header(4, 2);
        let mut output = Vec::new();
        let error = decode_member_bytes_to(
            &mut Cursor::new(b"abcd".to_vec()),
            &[],
            &[chunk(4)],
            &header,
            options(16),
            &mut output,
        )
        .unwrap_err();

        assert!(error.to_string().contains("does not match unpacked size"));
        assert!(output.is_empty());
    }

    #[test]
    fn store_rejects_output_shorter_than_declared_size() {
        let header = store_header(2, 4);
        let error = decode_member_bytes(
            &mut Cursor::new(b"ab".to_vec()),
            &[],
            &[chunk(2)],
            &header,
            options(16),
        )
        .unwrap_err();

        assert!(error.to_string().contains("does not match unpacked size"));
    }

    #[test]
    fn packed_payload_limit_is_checked_before_allocation() {
        let header = store_header(u64::MAX, u64::MAX);
        let error = decode_member_bytes(
            &mut Cursor::new(Vec::new()),
            &[],
            &[chunk(u64::MAX)],
            &header,
            options(1024),
        )
        .unwrap_err();

        assert!(matches!(error, RarError::LimitExceeded { limit: 1024, .. }));
    }

    #[test]
    fn unlimited_store_stream_is_not_rejected_by_allocation_guard() {
        const ALLOCATION_GUARD: u64 = 8 * 1024 * 1024 * 1024;
        let size = ALLOCATION_GUARD + 1;
        let header = store_header(size, size);
        let mut output = Vec::new();
        let error = decode_member_bytes_to(
            &mut Cursor::new(Vec::new()),
            &[],
            &[chunk(size)],
            &header,
            MemberDecodeOptions {
                password: None,
                decoder: None,
                max_alloc_packed_bytes: ALLOCATION_GUARD,
                max_stream_packed_bytes: u64::MAX,
            },
            &mut output,
        )
        .unwrap_err();

        assert!(
            matches!(error, RarError::Io(_)),
            "unexpected error: {error}"
        );
        assert!(output.is_empty());
    }

    #[test]
    fn packed_chunk_sum_overflow_is_rejected() {
        let header = store_header(0, 0);
        let error =
            checked_packed_size(&[chunk(u64::MAX), chunk(1)], &header, u64::MAX).unwrap_err();

        assert!(error.to_string().contains("packed size overflow"));
    }

    #[test]
    fn store_exact_size_roundtrips() {
        let header = store_header(4, 4);
        let output = decode_member_bytes(
            &mut Cursor::new(b"abcd".to_vec()),
            &[],
            &[chunk(4)],
            &header,
            options(16),
        )
        .unwrap();

        assert_eq!(output, b"abcd");
    }
}
