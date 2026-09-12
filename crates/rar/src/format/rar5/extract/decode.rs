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

impl RarArchive {
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
