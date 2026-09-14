//! Shared solid-chain rewrite: read → decode → verify → recompress →
//! emit, behind the [`ChunkReader`] seam.
//!
//! The single-volume (plan-driven) and multi-volume (re-split) rewrites
//! differ only in where the original bytes come from and what name the
//! rebuilt member carries, so the pipeline lives here once and each caller
//! supplies its reader adapter (a single archive file or a lazy set of
//! volume files).

use super::super::{DecryptedPayload, RarArchive};
use crate::codec::{DecoderState, EncoderState, lzss_huff as compression};
use crate::error::{RarError, RarResult};
use crate::format::rar5::COMP_METHOD_STORE;
use crate::format::rar5::payload::ChunkReader;

/// Shared window/encoder state of the solid chain being rewritten.
///
/// Created at the chain's first member from its dictionary size; the
/// STORE fallback resets the encoder and clears the solid flag exactly like
/// the sequential writer, so a rewritten member's solid bit always matches
/// the state the next member will decode against.
pub(super) struct SolidChainState {
    dec: DecoderState,
    enc: EncoderState,
    enc_active: bool,
}

impl SolidChainState {
    /// Start a chain whose first member declares `dict_log` (128 KiB << n).
    pub(super) fn start(dict_log: u8) -> RarResult<Self> {
        let dict_size = (128usize * 1024)
            .checked_shl(dict_log as u32)
            .ok_or_else(|| {
                RarError::Format("dictionary size overflows host address space".into())
            })?;
        Ok(Self {
            dec: DecoderState::new(dict_size),
            enc: compression::EncoderState::default(),
            enc_active: false,
        })
    }

    /// Decode member `idx` with the shared decoder window, verifying its
    /// integrity and advancing the chain without emitting anything. Deleted
    /// members use this so the following members still decode against the
    /// right window.
    pub(super) fn decode_member<R: ChunkReader + ?Sized>(
        &mut self,
        archive: &mut RarArchive,
        reader: &mut R,
        idx: usize,
    ) -> RarResult<Vec<u8>> {
        let hdr = &archive.entries[idx].header;
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(Vec::new());
        }
        let payload = archive.read_member_packed(reader, idx)?;
        let hdr = &archive.entries[idx].header;
        let mut raw_data = Vec::new();
        crate::format::rar5::payload::decode_member(
            hdr,
            &payload,
            Some(&mut self.dec),
            &mut raw_data,
        )?;
        let crc = crc32fast::hash(&raw_data);
        let blake = hdr
            .hash_value
            .map(|_| crate::format::rar5::blake2sp::hash(&raw_data));
        archive.verify_integrity(
            idx,
            crc,
            blake,
            payload.params.as_ref(),
            payload.keys.as_ref(),
        )?;
        Ok(raw_data)
    }

    /// Decode and recompress member `idx` (a kept chain member), emitting
    /// the rebuilt file entry under `name` (the original name or a rename).
    pub(super) fn recompress_member<R: ChunkReader + ?Sized>(
        &mut self,
        archive: &mut RarArchive,
        reader: &mut R,
        idx: usize,
        name: &str,
    ) -> RarResult<()> {
        let data = self.decode_member(archive, reader, idx)?;
        // Cloned so the header borrow does not outlive the encoding below.
        let hdr = archive.entries[idx].header.clone();

        let plain_crc = crc32fast::hash(&data);
        let plain_blake = hdr
            .hash_value
            .map(|_| crate::format::rar5::blake2sp::hash(&data));
        let variant = crate::version::ArchiveVersion::from_v70(hdr.dict_size_bytes.is_some());
        let packed = compression::encode_chunked(
            &data,
            compression::EncodeOptions {
                chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                state: Some(&mut self.enc),
                is_final: true,
                variant,
                ..compression::EncodeOptions::new(hdr.comp_method, hdr.comp_dict_size)
            },
        )?;

        let (method, dict_size_log, dict_size_bytes, payload) = if packed.len() >= data.len() {
            // Compression is a net loss: STORE resets the chain, matching
            // the sequential add_file path.
            self.enc.reset();
            self.enc_active = false;
            (COMP_METHOD_STORE, 0u8, None, data.clone())
        } else {
            self.enc_active = true;
            (
                hdr.comp_method,
                hdr.comp_dict_size,
                hdr.dict_size_bytes,
                packed,
            )
        };
        let (header_crc, extra_data, stored_hash, encr_params) =
            RarArchive::payload_extra_and_crc(archive.password.as_deref(), plain_crc, plain_blake)?;
        let payload = RarArchive::encrypt_payload_with(
            archive.password.as_deref(),
            encr_params.as_ref(),
            &payload,
        )?;
        archive.write_file_entry(
            name,
            data.len() as u64,
            &payload,
            header_crc,
            method,
            dict_size_log,
            dict_size_bytes,
            &extra_data,
            hdr.attributes,
            hdr.mtime,
            self.enc_active,
            stored_hash,
        )?;
        Ok(())
    }
}

impl RarArchive {
    /// Read the full packed (and decrypted, when applicable) payload of a
    /// member through any [`ChunkReader`], with the archive's password and
    /// packed-size bound. Shared by the chain rewrite and the verbatim
    /// re-split path.
    pub(super) fn read_member_packed<R: ChunkReader + ?Sized>(
        &self,
        reader: &mut R,
        idx: usize,
    ) -> RarResult<DecryptedPayload> {
        let entry = &self.entries[idx];
        let hdr = &entry.header;
        crate::format::rar5::payload::read_packed(
            reader,
            hdr,
            &entry.chunks,
            &hdr.name,
            self.password.as_deref(),
            self.max_packed_bytes(),
            || Ok(()),
        )
    }
}
