//! Packed-data assembly and member decoding.
//!
//! `read_packed_data` gathers (and decrypts) the member payload across
//! volumes; `decode_file_at`/`decode_file_to` drive the RAR5 decoder.
//! `IntegritySink` is the streaming output wrapper that computes
//! CRC32/BLAKE2sp on the fly.

use super::*;

use std::io::{self, Write};

use crate::codec::DecoderState;
use crate::engine::DecryptedPayload;
use crate::engine::Engine;
use crate::error::{RarError, RarResult};
use crate::format::shared::stream_mut;
/// Write sink that computes CRC32 and optional BLAKE2sp over streamed
/// output.
struct IntegritySink<'a> {
    inner: &'a mut dyn Write,
    crc: crc32fast::Hasher,
    blake: Option<crate::format::rar5::blake2sp::Hasher>,
}

impl<'a> IntegritySink<'a> {
    fn new(inner: &'a mut dyn Write, want_blake: bool) -> Self {
        Self {
            inner,
            crc: crc32fast::Hasher::new(),
            blake: want_blake.then(crate::format::rar5::blake2sp::Hasher::new),
        }
    }

    fn finish(self) -> (u32, Option<[u8; 32]>) {
        (self.crc.finalize(), self.blake.map(|h| h.finalize()))
    }
}

impl Write for IntegritySink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        if let Some(h) = self.blake.as_mut() {
            h.update(&buf[..n]);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Decrypt (when the stream block carries an ENCR record) and decode one
/// "STM" alternate-data-stream payload, verifying the stored CRC32 over the
/// decoded bytes.
#[cfg_attr(not(windows), allow(dead_code))]
fn decode_stream_payload(
    record: &crate::engine::StreamRecord,
    packed: &[u8],
    keys: Option<&crate::crypto::DerivedKeys>,
) -> RarResult<Vec<u8>> {
    let mut packed = match (record.params.as_ref(), keys) {
        (Some(params), Some(keys)) => crate::crypto::decrypt_data(packed, &keys.key, &params.iv)?,
        _ => packed.to_vec(),
    };
    let data = if record.method == crate::format::rar5::COMP_METHOD_STORE {
        // Encrypted STORE payloads carry AES zero-fill padding; the
        // unpacked size trims it back to the real stream bytes.
        let len = usize::try_from(record.unpacked_size)
            .unwrap_or(usize::MAX)
            .min(packed.len());
        packed.truncate(len);
        packed
    } else {
        crate::codec::decode_standalone(
            &packed,
            record.unpacked_size,
            record.dict_size_log,
            None,
            crate::version::ArchiveVersion::V50,
        )
        .map_err(|e| RarError::Format(format!("stream decode: {e}")))?
    };
    if let Some(expected) = record.crc32 {
        let actual = crc32fast::hash(&data);
        if actual != expected {
            return Err(RarError::Crc {
                expected,
                actual,
                context: format!("NTFS stream {}", record.name),
            });
        }
    }
    Ok(data)
}

/// Read, decrypt and decode every record in `records`, fetching the packed
/// bytes through `reader`. Returns `(name, bytes, was_encrypted)` in archive
/// order.
///
/// Both the packed and the unpacked size are capped before they can drive an
/// allocation (a crafted "STM" header could otherwise request a multi-TiB
/// decode window), and the packed size is narrowed with `try_from` so 32-bit
/// targets report an error instead of truncating.
pub(crate) fn read_streams_with<R: crate::format::rar5::payload::ChunkReader + ?Sized>(
    records: &[crate::engine::StreamRecord],
    reader: &mut R,
    password: Option<&str>,
    limit: u64,
    max_dict_size: Option<u64>,
) -> RarResult<Vec<(String, Vec<u8>, bool)>> {
    let mut out = Vec::with_capacity(records.len());
    for s in records {
        if s.data_size > limit {
            return Err(RarError::LimitExceeded {
                limit,
                context: format!(
                    "NTFS stream {:?} declares {} packed bytes",
                    s.name, s.data_size
                ),
            });
        }
        let declared = usize::try_from(s.data_size).map_err(|_| RarError::LimitExceeded {
            limit,
            context: format!("NTFS stream {:?} packed size does not fit in usize", s.name),
        })?;
        if s.unpacked_size > limit {
            return Err(RarError::LimitExceeded {
                limit,
                context: format!(
                    "NTFS stream {:?} declares {} unpacked bytes",
                    s.name, s.unpacked_size
                ),
            });
        }
        // A compressed stream allocates the same LZ window a member
        // would, and its 4-bit dictionary field can declare up to 4 GiB.
        // Enforce the extraction dictionary cap (`-mdx`) exactly like
        // `member_dict_window` does for members.
        let dict_bytes = (128u64 * 1024) << s.dict_size_log;
        if let Some(cap) = max_dict_size
            && dict_bytes > cap
        {
            return Err(RarError::LimitExceeded {
                limit: cap,
                context: format!(
                    "NTFS stream {:?} dictionary size {dict_bytes} bytes exceeds the extraction cap (use -mdx to raise it)",
                    s.name
                ),
            });
        }
        let packed = reader.read_chunk(s.volume_index, s.data_offset, s.data_size)?;
        if packed.len() != declared {
            return Err(RarError::Format(format!(
                "NTFS stream {:?} is truncated: {} of {declared} bytes",
                s.name,
                packed.len()
            )));
        }
        // Encrypted streams carry a per-stream ENCR record (own salt),
        // so the password is checked and the keys are derived here
        // rather than at open time. Service records accept the all-zero
        // `PswCheck` RAR <= 5.21 wrote for "STM" items.
        let keys = match s.params.as_ref() {
            Some(params) => {
                let password = password.ok_or_else(|| {
                    RarError::Encrypted(format!(
                        "{}: encrypted NTFS stream, no password set",
                        s.name
                    ))
                })?;
                Some(
                    params
                        .derive_and_verify_service(password)?
                        .ok_or(RarError::WrongPassword)?,
                )
            }
            None => None,
        };
        out.push((
            s.name.clone(),
            decode_stream_payload(s, &packed, keys.as_ref())?,
            s.params.is_some(),
        ));
    }
    Ok(out)
}

/// Read, decrypt and decode every "STM" stream record owned by member
/// `idx`, returning `(name, bytes)` pairs in archive order.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn read_member_streams(
    cx: &mut dyn Engine,
    idx: usize,
) -> RarResult<Vec<(String, Vec<u8>)>> {
    // Gather everything owned first: the reader below borrows the engine
    // mutably, so the options and the record list must not come from it.
    let (records, limit, max_dict_size, password) = {
        let p = cx.parts();
        let options = &p.read_ctx().extract_options;
        let records: Vec<crate::engine::StreamRecord> = p
            .read_ctx()
            .streams
            .iter()
            .filter(|s| s.owner_index == idx)
            .cloned()
            .collect();
        (
            records,
            options.metadata_limit(),
            options.max_dict_size,
            p.password.map(str::to_owned),
        )
    };
    let p = cx.parts();
    let mut reader = crate::format::rar5::payload::StreamReader {
        stream: stream_mut(p.stream)?,
        volume_paths: p.volume_paths,
    };
    let streams = read_streams_with(
        &records,
        &mut reader,
        password.as_deref(),
        limit,
        max_dict_size,
    )?;
    Ok(streams
        .into_iter()
        .map(|(name, data, _encrypted)| (name, data))
        .collect())
}

/// Read packed data for an entry, potentially across multiple volumes.
///
/// The returned payload is decrypted (when applicable) together with
/// the derived keys needed for integrity verification.
pub(crate) fn read_packed_data(cx: &mut dyn Engine, idx: usize) -> RarResult<DecryptedPayload> {
    let max_packed = crate::format::shared::extract::max_packed_bytes(cx);
    let p = cx.parts();
    let entry = &p.entries[idx];
    let hdr = &entry.header;
    let mut reader = crate::format::rar5::payload::StreamReader {
        stream: stream_mut(p.stream)?,
        volume_paths: p.volume_paths,
    };
    crate::format::rar5::payload::read_packed(
        &mut reader,
        hdr,
        &entry.chunks,
        &hdr.name,
        p.password,
        max_packed,
        || {
            if p.cancel
                .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
            {
                return Err(RarError::Cancelled);
            }
            Ok(())
        },
    )
}

/// Verify the stored checksums of a zero-size member without decoding
/// a payload: CRC32 of empty, BLAKE2sp of empty when a hash record
/// exists, and their hash-key MAC equivalents when the member is
/// encrypted. The parallel extraction path verifies empty members the
/// same way; returning early without this check would let a crafted
/// zero-size header bypass integrity verification.
fn verify_empty_member(cx: &mut dyn Engine, idx: usize) -> RarResult<()> {
    let crc = crc32fast::hash(&[]);
    let blake = cx.entries()[idx]
        .header
        .hash_value
        .map(|_| crate::format::rar5::blake2sp::hash(&[]));
    let payload = read_packed_data(cx, idx)?;
    crate::format::rar5::extract::verify::verify_integrity(
        cx,
        idx,
        crc,
        blake,
        payload.params.as_ref(),
        payload.keys.as_ref(),
    )
}

/// Decode a single file into memory, optionally with a shared
/// DecoderState (solid archives), verifying CRC32/BLAKE2sp.
pub(crate) fn decode_file_at(
    cx: &mut dyn Engine,
    idx: usize,
    state: Option<&mut DecoderState>,
) -> RarResult<Vec<u8>> {
    crate::format::shared::extract::members::validate_entry_limits(cx, idx)?;
    let _ = member_dict_window(cx, idx)?; // enforces the -mdx cap

    // Empty files / directories
    if cx.entries()[idx].header.packed_size == 0 && cx.entries()[idx].header.unpacked_size == 0 {
        if cx.entries()[idx].is_dir() {
            return Ok(Vec::new());
        }
        verify_empty_member(cx, idx)?;
        return Ok(Vec::new());
    }

    let payload = read_packed_data(cx, idx)?;
    let mut raw_data = Vec::new();
    crate::format::rar5::payload::decode_member(
        &cx.entries()[idx].header,
        &payload,
        state,
        &mut raw_data,
    )?;

    let crc = crc32fast::hash(&raw_data);
    let blake = cx.entries()[idx]
        .header
        .hash_value
        .map(|_| crate::format::rar5::blake2sp::hash(&raw_data));
    crate::format::rar5::extract::verify::verify_integrity(
        cx,
        idx,
        crc,
        blake,
        payload.params.as_ref(),
        payload.keys.as_ref(),
    )?;
    Ok(raw_data)
}

/// Actual dictionary size of a member in bytes: RAR5 uses
/// `128 KiB << comp_dict_size`, RAR7 carries the byte count directly
/// (possibly non-power-of-two). The sliding window rounds up to a
/// power of two. Enforces the extraction dictionary cap
/// (`ExtractOptions::max_dict_size`, WinRAR's `-mdx`).
pub(crate) fn member_dict_window(cx: &dyn Engine, idx: usize) -> RarResult<usize> {
    let hdr = &cx.entries()[idx].header;
    let bytes = capped_dict_bytes(hdr, cx.read_ctx().extract_options.max_dict_size)?;
    let bytes = usize::try_from(bytes)
        .map_err(|_| RarError::Format("dictionary size overflows host address space".into()))?;
    bytes
        .checked_next_power_of_two()
        .ok_or_else(|| RarError::Format("dictionary size overflows host address space".into()))
}

/// Decode a single file, streaming output to `writer` (bounded memory),
/// verifying CRC32/BLAKE2sp over the written bytes.
pub(crate) fn decode_file_to(
    cx: &mut dyn Engine,
    idx: usize,
    writer: &mut dyn Write,
    state: Option<&mut DecoderState>,
) -> RarResult<u64> {
    crate::format::shared::extract::members::validate_entry_limits(cx, idx)?;
    let _ = member_dict_window(cx, idx)?; // enforces the -mdx cap
    if cx.entries()[idx].header.packed_size == 0 && cx.entries()[idx].header.unpacked_size == 0 {
        if cx.entries()[idx].is_dir() {
            return Ok(0);
        }
        verify_empty_member(cx, idx)?;
        return Ok(0);
    }

    let payload = read_packed_data(cx, idx)?;
    let want_blake = cx.entries()[idx].header.hash_value.is_some();
    let mut sink = IntegritySink::new(writer, want_blake);

    let written = crate::format::rar5::payload::decode_member(
        &cx.entries()[idx].header,
        &payload,
        state,
        &mut sink,
    )?;

    let (crc, blake) = sink.finish();
    crate::format::rar5::extract::verify::verify_integrity(
        cx,
        idx,
        crc,
        blake,
        payload.params.as_ref(),
        payload.keys.as_ref(),
    )?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::RarArchive;
    use crate::format::rar5::headers::{ArchiveHeader, EndOfArchiveHeader};
    use crate::format::rar5::{
        BLOCK_FLAG_DATA_AREA, BLOCK_FLAG_DEPENDS_PREV, BLOCK_FLAG_EXTRA_DATA,
        BLOCK_TYPE_SERVICE_HEADER, COMP_INFO_DICT_SHIFT, COMP_INFO_METHOD_SHIFT,
        COMP_METHOD_NORMAL, COMP_METHOD_STORE, EXTRA_SERVICE_SUBDATA, FILE_FLAG_CRC32, OS_WINDOWS,
        RAR5_SIGNATURE,
    };
    use crate::vint;

    /// Build a minimal single-member RAR5 archive whose member owns one
    /// "STM" service block. The stream's compression info carries `method`
    /// and `dict_log` (the low four bits WinRAR reserves for the LZ
    /// dictionary); `payload` becomes the block's data area.
    fn archive_with_stream(method: u8, dict_log: u8, payload: &[u8], unpacked: u64) -> Vec<u8> {
        let mut out = RAR5_SIGNATURE.to_vec();
        out.extend_from_slice(
            &ArchiveHeader {
                flags: 0,
                extra_data: Vec::new(),
                volume_number: None,
            }
            .to_bytes(),
        );
        let member = FileHeader {
            name: "owner.bin".into(),
            ..Default::default()
        };
        out.extend_from_slice(&member.to_bytes());

        let stream_name = b":ads";
        let mut extra = Vec::new();
        extra.extend(vint::encode((1 + stream_name.len()) as u64));
        extra.extend(vint::encode(EXTRA_SERVICE_SUBDATA));
        extra.extend_from_slice(stream_name);

        let mut body = Vec::new();
        body.extend(vint::encode(BLOCK_TYPE_SERVICE_HEADER));
        body.extend(vint::encode(
            BLOCK_FLAG_EXTRA_DATA | BLOCK_FLAG_DATA_AREA | BLOCK_FLAG_DEPENDS_PREV,
        ));
        body.extend(vint::encode(extra.len() as u64)); // extra area size
        body.extend(vint::encode(payload.len() as u64)); // data size
        body.extend(vint::encode(FILE_FLAG_CRC32));
        body.extend(vint::encode(unpacked));
        body.extend(vint::encode(0u64)); // attributes
        body.extend(crc32fast::hash(payload).to_le_bytes());
        body.extend(vint::encode(
            (u64::from(dict_log) << COMP_INFO_DICT_SHIFT)
                | (u64::from(method) << COMP_INFO_METHOD_SHIFT),
        ));
        body.extend(vint::encode(OS_WINDOWS));
        body.extend(vint::encode(3u64)); // name length
        body.extend(b"STM");
        body.extend_from_slice(&extra);

        let mut content = vint::encode(body.len() as u64);
        content.extend(body);
        out.extend_from_slice(&crc32fast::hash(&content).to_le_bytes());
        out.extend(content);
        out.extend_from_slice(payload);
        out.extend_from_slice(&EndOfArchiveHeader { flags: 0 }.to_bytes());
        out
    }

    fn open_archive(bytes: &[u8]) -> (tempfile::TempDir, RarArchive) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream-cap.rar");
        std::fs::write(&path, bytes).unwrap();
        let archive = RarArchive::open(&path).unwrap();
        (dir, archive)
    }

    /// A "STM" record with dictionary bits 0xF declares a 4 GiB window; a
    /// lowered `max_dict_size` must reject it before any allocation.
    #[test]
    fn stream_dictionary_cap_is_enforced() {
        let bytes = archive_with_stream(COMP_METHOD_NORMAL, 0x0F, &[0xFF; 4], 4);
        let (_dir, mut archive) = open_archive(&bytes);
        archive.read_ctx_mut().extract_options.max_dict_size = Some(128 * 1024);
        let err =
            crate::format::rar5::extract::decode::read_member_streams(&mut archive, 0).unwrap_err();
        assert!(
            matches!(err, RarError::LimitExceeded { .. }),
            "unexpected: {err:?}"
        );
    }

    /// Positive control: a STORE stream with dictionary bits 0xF (exactly
    /// the default 4 GiB cap) passes the check and decodes.
    #[test]
    fn stream_dictionary_at_the_default_cap_decodes() {
        let payload = b"stream bytes";
        let bytes = archive_with_stream(COMP_METHOD_STORE, 0x0F, payload, payload.len() as u64);
        let (_dir, mut archive) = open_archive(&bytes);
        assert_eq!(
            crate::format::rar5::extract::decode::read_member_streams(&mut archive, 0).unwrap(),
            vec![(":ads".to_string(), payload.to_vec())]
        );
    }
}
