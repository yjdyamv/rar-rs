//! Packed-data assembly and member decoding.
//!
//! `read_packed_data` gathers (and decrypts) the member payload across
//! volumes; `decode_file_at`/`decode_file_to` drive the RAR5 decoder and
//! `decode_rar4_*` the legacy codecs. `IntegritySink` is the streaming
//! output wrapper that computes CRC32/BLAKE2sp on the fly.

use super::*;

use std::io::{self, Write};

use crate::archive::{DecryptedPayload, RarArchive};
use crate::codec::DecoderState;
use crate::error::{RarError, RarResult};
use crate::format::shared::stream_mut;
use crate::model::FileHeader;
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
    record: &crate::archive::StreamRecord,
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

impl RarArchive {
    /// Read, decrypt and decode every "STM" stream record owned by member
    /// `idx`, returning `(name, bytes)` pairs in archive order.
    ///
    /// Both the packed and the unpacked size are capped before they can
    /// drive an allocation (a crafted "STM" header could otherwise request
    /// a multi-TiB decode window), and the packed size is narrowed with
    /// `try_from` so 32-bit targets report an error instead of truncating.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn read_member_streams(&mut self, idx: usize) -> RarResult<Vec<(String, Vec<u8>)>> {
        use std::io::{Read, Seek, SeekFrom};

        let owned: Vec<crate::archive::StreamRecord> = self
            .read_ctx()
            .streams
            .iter()
            .filter(|s| s.owner_index == idx)
            .cloned()
            .collect();
        let mut out = Vec::with_capacity(owned.len());
        for s in owned {
            let limit = self.read_ctx().extract_options.metadata_limit();
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
            let mut packed = vec![0u8; declared];
            {
                let stream = stream_mut(&mut self.stream)?;
                stream.seek(SeekFrom::Start(s.data_offset))?;
                stream.read_exact(&mut packed)?;
            }
            // Encrypted streams carry a per-stream ENCR record (own salt),
            // so the password is checked and the keys are derived here
            // rather than at open time.
            let keys = match s.params.as_ref() {
                Some(params) => {
                    let password = self.password.as_deref().ok_or_else(|| {
                        RarError::Encrypted(format!(
                            "{}: encrypted NTFS stream, no password set",
                            s.name
                        ))
                    })?;
                    if !params.verify_password(password) {
                        return Err(RarError::WrongPassword);
                    }
                    Some(params.derive_keys(password)?)
                }
                None => None,
            };
            out.push((
                s.name.clone(),
                decode_stream_payload(&s, &packed, keys.as_ref())?,
            ));
        }
        Ok(out)
    }
    /// Read packed data for an entry, potentially across multiple volumes.
    ///
    /// The returned payload is decrypted (when applicable) together with
    /// the derived keys needed for integrity verification.
    pub(super) fn read_packed_data(&mut self, idx: usize) -> RarResult<DecryptedPayload> {
        let entry = &self.entries[idx];
        let hdr = &entry.header;
        let max_packed = self.max_packed_bytes();
        let password = self.password.as_deref();
        let cancel = &self.cancel;
        let mut reader = crate::format::rar5::payload::StreamReader {
            stream: stream_mut(&mut self.stream)?,
            volume_paths: &self.volume_paths,
        };
        crate::format::rar5::payload::read_packed(
            &mut reader,
            hdr,
            &entry.chunks,
            &hdr.name,
            password,
            max_packed,
            || {
                if cancel
                    .as_ref()
                    .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
                {
                    return Err(RarError::Cancelled);
                }
                Ok(())
            },
        )
    }

    /// Maximum packed bytes accepted when the payload must be aggregated in
    /// memory. Bounded by the configured unpacked limit plus a small overhead,
    /// or a hard 8 GiB allocation guard when output is otherwise unlimited.
    pub(crate) fn max_packed_bytes(&self) -> u64 {
        self.read_ctx()
            .extract_options
            .max_unpacked_bytes
            .map(|u| u.saturating_add(1 << 20))
            .unwrap_or(8 * 1024 * 1024 * 1024)
    }

    /// Maximum packed bytes accepted by a truly streaming STORE path. With no
    /// unpacked limit there is no allocation-driven packed-size ceiling.
    fn max_stream_packed_bytes(&self) -> u64 {
        self.read_ctx()
            .extract_options
            .max_unpacked_bytes
            .map(|u| u.saturating_add(1 << 20))
            .unwrap_or(u64::MAX)
    }

    /// Decode a single file into memory, optionally with a shared
    /// DecoderState (solid archives), verifying CRC32/BLAKE2sp.
    pub(super) fn decode_file_at(
        &mut self,
        idx: usize,
        state: Option<&mut DecoderState>,
    ) -> RarResult<Vec<u8>> {
        self.validate_entry_limits(idx)?;
        let _ = self.member_dict_window(idx)?; // enforces the -mdx cap
        let hdr = &self.entries[idx].header;

        // Empty files / directories
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(Vec::new());
        }

        let payload = self.read_packed_data(idx)?;
        let mut raw_data = Vec::new();
        crate::format::rar5::payload::decode_member(
            &self.entries[idx].header,
            &payload,
            state,
            &mut raw_data,
        )?;

        let crc = crc32fast::hash(&raw_data);
        let blake = self.entries[idx]
            .header
            .hash_value
            .map(|_| crate::format::rar5::blake2sp::hash(&raw_data));
        self.verify_integrity(
            idx,
            crc,
            blake,
            payload.params.as_ref(),
            payload.keys.as_ref(),
        )?;
        Ok(raw_data)
    }

    /// Decode a single file, streaming output to `writer` (bounded memory),
    /// verifying CRC32/BLAKE2sp over the written bytes.
    /// Actual dictionary size of a member in bytes: RAR5 uses
    /// `128 KiB << comp_dict_size`, RAR7 carries the byte count directly
    /// (possibly non-power-of-two). The sliding window rounds up to a
    /// power of two. Enforces the extraction dictionary cap
    /// (`ExtractOptions::max_dict_size`, WinRAR's `-mdx`).
    pub(super) fn member_dict_window(&self, idx: usize) -> RarResult<usize> {
        let hdr = &self.entries[idx].header;
        let bytes = capped_dict_bytes(hdr, self.read_ctx().extract_options.max_dict_size)?;
        let bytes = usize::try_from(bytes)
            .map_err(|_| RarError::Format("dictionary size overflows host address space".into()))?;
        bytes
            .checked_next_power_of_two()
            .ok_or_else(|| RarError::Format("dictionary size overflows host address space".into()))
    }

    pub(super) fn decode_file_to(
        &mut self,
        idx: usize,
        writer: &mut dyn Write,
        state: Option<&mut DecoderState>,
    ) -> RarResult<u64> {
        self.validate_entry_limits(idx)?;
        let hdr = &self.entries[idx].header;
        let _ = self.member_dict_window(idx)?; // enforces the -mdx cap
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(0);
        }

        let payload = self.read_packed_data(idx)?;
        let mut sink = IntegritySink::new(writer, self.entries[idx].header.hash_value.is_some());

        let written = crate::format::rar5::payload::decode_member(
            &self.entries[idx].header,
            &payload,
            state,
            &mut sink,
        )?;

        let (crc, blake) = sink.finish();
        self.verify_integrity(
            idx,
            crc,
            blake,
            payload.params.as_ref(),
            payload.keys.as_ref(),
        )?;
        Ok(written)
    }
    /// Decode a single RAR4 member in memory, verifying its CRC32. Solid
    /// chain members decode through their chain prefix (shared window).
    pub(crate) fn decode_rar4_at(&mut self, idx: usize) -> RarResult<Vec<u8>> {
        self.validate_entry_limits(idx)?;
        let hdr = self.entries[idx].header.clone();
        if self.is_rar4_solid_member(idx) {
            let chain_start = self.rar4_find_chain_start(idx);
            let result = self
                .rar4_decode_solid_through(idx)
                .and_then(|out| self.rar4_verify_crc(&hdr, &out).map(|()| out));
            if result.is_err() {
                self.reset_rar4_solid_decoder(chain_start);
            }
            return result;
        }
        let out = self.rar4_decode_member(idx)?;
        self.rar4_verify_crc(&hdr, &out)?;
        Ok(out)
    }

    /// Decode a single RAR4 member, streaming output to `writer`, verifying
    /// its CRC32 over the written bytes. Non-chain members stream through
    /// the bounded-memory path (STORE chunks copied straight out; compressed
    /// members decode incrementally); solid-chain members keep the shared
    /// window semantics and decode in one pass.
    pub(super) fn decode_rar4_to(&mut self, idx: usize, writer: &mut dyn Write) -> RarResult<u64> {
        self.validate_entry_limits(idx)?;
        let hdr = self.entries[idx].header.clone();
        if self.is_rar4_solid_member(idx) {
            let chain_start = self.rar4_find_chain_start(idx);
            let result = self.rar4_decode_solid_through(idx).and_then(|out| {
                self.rar4_verify_crc(&hdr, &out)?;
                writer.write_all(&out).map_err(RarError::Io)?;
                Ok(out.len() as u64)
            });
            if result.is_err() {
                self.reset_rar4_solid_decoder(chain_start);
            }
            return result;
        }
        let entry = self.entries[idx].clone();
        let max_alloc_packed_bytes = self.max_packed_bytes();
        let max_stream_packed_bytes = self.max_stream_packed_bytes();
        let (written, crc) = crate::format::rar4::decode_member_bytes_to(
            stream_mut(&mut self.stream)?,
            &self.volume_paths,
            &entry.chunks,
            &entry.header,
            crate::format::rar4::MemberDecodeOptions {
                password: self.password.as_deref(),
                decoder: None,
                max_alloc_packed_bytes,
                max_stream_packed_bytes,
            },
            writer,
        )?;
        // The streamed CRC is authoritative; compare with the header.
        if let Some(expected) = hdr.crc32_val
            && crc != expected
        {
            return Err(RarError::Crc {
                expected,
                actual: crc,
                context: format!("{}: CRC32 mismatch", hdr.name),
            });
        }
        Ok(written)
    }

    /// Decode RAR4 member `idx`, routing solid-chain members through the
    /// persistent legacy decoder so their look-behind window covers the
    /// chain prefix.
    fn rar4_decode_member(&mut self, idx: usize) -> RarResult<Vec<u8>> {
        if self.is_rar4_solid_member(idx) {
            return self.rar4_decode_solid_through(idx);
        }
        let entry = self.entries[idx].clone();
        let max_packed_bytes = self.max_packed_bytes();
        crate::format::rar4::decode_member_bytes(
            stream_mut(&mut self.stream)?,
            &self.volume_paths,
            &entry.chunks,
            &entry.header,
            crate::format::rar4::MemberDecodeOptions {
                password: self.password.as_deref(),
                decoder: None,
                max_alloc_packed_bytes: max_packed_bytes,
                max_stream_packed_bytes: max_packed_bytes,
            },
        )
    }
    fn rar4_verify_crc(&self, hdr: &FileHeader, data: &[u8]) -> RarResult<()> {
        if let Some(expected) = hdr.crc32_val {
            let actual = crate::format::rar4::member_crc(data);
            if actual != expected {
                return Err(RarError::Crc {
                    expected,
                    actual,
                    context: format!("{}: CRC32 mismatch", hdr.name),
                });
            }
        }
        Ok(())
    }
}
