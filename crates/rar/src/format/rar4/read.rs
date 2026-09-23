//! RAR 1.5–4.x member read + decode.
//!
//! Reads a member's packed payload (single volume), decrypts it with the
//! cipher the member's codec selects (RAR15 stream, RAR20 block, or the
//! RAR30 AES-CBC block), then decodes it. STORE members pass through raw;
//! compressed members decode through the codec selected by
//! [`crate::version::LegacyCodec`] (one alias fold for `unp_ver` 15/20/26/
//! 29/36). Solid chains share one decoder instance passed in by the caller
//! via [`super::LegacyDecoder`].

use crate::codec::legacy::rar15::Rar15Decoder;
use crate::codec::legacy::rar20::Rar20Decoder;
use crate::codec::legacy::rar29::Rar29Decoder;
use crate::crc32;
use crate::crypto::{Rar15Cipher, Rar20Cipher, Rar30Cipher};
use crate::error::{RarError, RarResult};
use crate::model::{DataChunk, FileHeader};
use crate::version::LegacyCodec;
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
    /// RAR 1.3/1.4 rolling checksum, maintained alongside the CRC-32 so the
    /// streaming path can verify that family without a second pass.
    rar13_checksum: u16,
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
        for &byte in &buf[..n] {
            self.rar13_checksum = self
                .rar13_checksum
                .wrapping_add(u16::from(byte))
                .rotate_left(1);
        }
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
) -> RarResult<(u64, u32, u16)> {
    let mut crc_writer = CrcWriter {
        inner: writer,
        hasher: crc32fast::Hasher::new(),
        rar13_checksum: 0,
        count: 0,
        limit: hdr.unpacked_size,
    };
    {
        let writer: &mut dyn std::io::Write = &mut crc_writer;
        decode_member_bytes_to_inner(stream, volume_paths, chunks, hdr, options, writer)?;
    }
    validate_output_size(hdr, crc_writer.count)?;
    Ok((
        crc_writer.count,
        crc_writer.hasher.clone().finalize(),
        crc_writer.rar13_checksum,
    ))
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
        // RAR 1.3/1.4 encrypts the member's whole packed stream with one
        // RAR13 cipher stream (volume fragments continue it); the other
        // families use their own cipher over the assembled payload.
        if hdr.format_version == 3 {
            crate::crypto::Rar13Cipher::new(password).decrypt_in_place(&mut packed);
        } else {
            decrypt_in_place(hdr, password, &mut packed)?;
        }
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

    let Some(codec) = LegacyCodec::from_unp_ver(hdr.unp_ver) else {
        return Err(unsupported_unp_ver(hdr.unp_ver));
    };
    match codec {
        LegacyCodec::Rar29 => match decoder {
            Some(LegacyDecoder::Rar29(dec)) => dec
                .decode_member_streaming_to(&packed, hdr.unpacked_size, writer)
                .map_err(|e| map_codec_error(hdr, e)),
            Some(other) => Err(wrong_decoder(codec, other.codec())),
            None => crate::codec::legacy::rar29::Rar29Decoder::new()
                .decode_member_streaming_to(&packed, hdr.unpacked_size, writer)
                .map_err(|e| map_codec_error(hdr, e)),
        },
        LegacyCodec::Rar20 => match decoder {
            Some(LegacyDecoder::Rar20(dec)) => dec
                .decode_member_streaming_to(&packed, hdr.unpacked_size, writer)
                .map_err(|e| map_codec_error(hdr, e)),
            Some(other) => Err(wrong_decoder(codec, other.codec())),
            None => crate::codec::legacy::rar20::Rar20Decoder::new()
                .decode_member_streaming_to(&packed, hdr.unpacked_size, writer)
                .map_err(|e| map_codec_error(hdr, e)),
        },
        LegacyCodec::Rar15 => {
            let solid = decoder.is_some();
            let dec: &mut Rar15Decoder = match decoder {
                Some(LegacyDecoder::Rar15(dec)) => dec,
                Some(other) => return Err(wrong_decoder(codec, other.codec())),
                None => {
                    return Rar15Decoder::new()
                        .decode_member_to(&packed, unp_size, false, writer)
                        .map_err(|error| map_rar15_error(hdr, error));
                }
            };
            dec.decode_member_to(&packed, unp_size, solid, writer)
                .map_err(|error| map_rar15_error(hdr, error))
        }
    }
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
        if hdr.format_version == 3 {
            // One RAR13 cipher stream over the whole packed member.
            crate::crypto::Rar13Cipher::new(password).decrypt_in_place(&mut packed);
        } else {
            decrypt_in_place(hdr, password, &mut packed)?;
        }
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

    let Some(codec) = LegacyCodec::from_unp_ver(hdr.unp_ver) else {
        return Err(unsupported_unp_ver(hdr.unp_ver));
    };
    match codec {
        LegacyCodec::Rar29 => {
            let out = match decoder {
                Some(LegacyDecoder::Rar29(dec)) => dec.decode_member(&packed, hdr.unpacked_size),
                Some(other) => return Err(wrong_decoder(codec, other.codec())),
                None => Rar29Decoder::new().decode_member(&packed, hdr.unpacked_size),
            }
            .map_err(|e| map_codec_error(hdr, e))?;
            validate_output_size(hdr, out.len() as u64)?;
            Ok(out)
        }
        LegacyCodec::Rar20 => {
            let out = match decoder {
                Some(LegacyDecoder::Rar20(dec)) => dec.decode_member(&packed, hdr.unpacked_size),
                Some(other) => return Err(wrong_decoder(codec, other.codec())),
                None => Rar20Decoder::new().decode_member(&packed, hdr.unpacked_size),
            }
            .map_err(|e| map_codec_error(hdr, e))?;
            validate_output_size(hdr, out.len() as u64)?;
            Ok(out)
        }
        LegacyCodec::Rar15 => {
            let out = match decoder {
                Some(LegacyDecoder::Rar15(dec)) => {
                    dec.decode_member(&packed, hdr.unpacked_size, true)
                }
                Some(other) => return Err(wrong_decoder(codec, other.codec())),
                None => Rar15Decoder::new().decode_member(&packed, hdr.unpacked_size, false),
            }
            .map_err(|error| map_codec_error(hdr, error))?;
            validate_output_size(hdr, out.len() as u64)?;
            Ok(out)
        }
    }
}

/// A RAR4 member whose `unp_ver` names no legacy codec (the RAR13 and RAR5
/// families have their own read paths).
fn unsupported_unp_ver(unp_ver: u8) -> RarError {
    RarError::Unsupported(format!(
        "RAR4 compressed member with unsupported unpack version {unp_ver}"
    ))
}

/// A solid-chain carrier whose codec differs from the member's: callers
/// rebuild the decoder on a codec change, so this is a corrupted chain.
fn wrong_decoder(expected: LegacyCodec, actual: LegacyCodec) -> RarError {
    RarError::Format(format!(
        "RAR4: {} member in a solid chain carrying the {} decoder",
        expected.name(),
        actual.name()
    ))
}

/// Map a RAR 1.5 codec error to the caller-facing error.
fn map_rar15_error(hdr: &FileHeader, error: crate::codec::legacy::rar15::Error) -> RarError {
    let error = match error {
        crate::codec::legacy::rar15::Error::NeedMoreInput => {
            RarError::Format("RAR 1.5 bitstream is truncated".into())
        }
        crate::codec::legacy::rar15::Error::InvalidData(message) => {
            RarError::Format(format!("RAR 1.5 stream: {message}"))
        }
    };
    map_codec_error(hdr, error)
}

/// Map codec-level errors to user-facing errors: encrypted members with a
/// password provided treat CRC/codec/crypto errors as
/// `WrongPassword` (mirrors rars `map_encrypted_payload_error`).
///
/// Also the mapper for the member-level CRC check in `extract.rs`: a RAR4
/// archive carries no password check value, so a wrong password and a
/// corrupt stream are indistinguishable, and both must report
/// `WrongPassword` (CLI exit 11) rather than a bare `Crc` (exit 3).
pub(super) fn map_codec_error(hdr: &FileHeader, error: RarError) -> RarError {
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

/// Decrypt `data` in place according to the member's codec cipher.
pub(crate) fn decrypt_in_place(
    hdr: &FileHeader,
    password: &[u8],
    data: &mut [u8],
) -> RarResult<()> {
    match LegacyCodec::from_unp_ver(hdr.unp_ver) {
        Some(LegacyCodec::Rar15) => {
            Rar15Cipher::new(password).crypt_in_place(data);
            Ok(())
        }
        Some(LegacyCodec::Rar20) => Rar20Cipher::new(password)
            .decrypt_in_place(data)
            .map_err(|e| RarError::Format(format!("RAR4 RAR20 decrypt: {e}"))),
        Some(LegacyCodec::Rar29) => {
            let mut cipher = Rar30Cipher::new(password, hdr.salt)
                .map_err(|e| RarError::Format(format!("RAR4 RAR30 key setup: {e}")))?;
            cipher
                .decrypt_in_place(data)
                .map_err(|e| RarError::Format(format!("RAR4 RAR30 decrypt: {e}")))
        }
        None => Err(RarError::Unsupported(format!(
            "RAR4 encryption unpack version {} not supported",
            hdr.unp_ver
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

    /// An unpack version outside the codec table is refused before any codec
    /// work, instead of being fed to the RAR29 decoder by an `unp_ver >= 29`
    /// range check.
    #[test]
    fn unknown_unpack_version_is_rejected() {
        let header = FileHeader {
            name: "mystery.bin".into(),
            packed_size: 0,
            unpacked_size: 1,
            comp_method: crate::format::rar4::RAR4_METHOD_STORE + 1,
            unp_ver: 33,
            format_version: 4,
            ..Default::default()
        };
        let error =
            decode_member_bytes(&mut Cursor::new(Vec::new()), &[], &[], &header, options(16))
                .unwrap_err();

        assert!(matches!(error, RarError::Unsupported(_)), "got {error:?}");
    }
}
