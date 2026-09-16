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
use crate::error::RarResult;
use crate::format::rar5::COMP_METHOD_STORE;
use crate::format::rar5::headers::retain_extra_records;
use crate::format::rar5::payload::ChunkReader;
use crate::format::rar5::write::MemberPlan;
use crate::format::rar5::{EXTRA_FILE_ENCRYPTION, EXTRA_FILE_HASH};

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
    /// Start a chain at the window the head member declares (already
    /// resolved through `member_dict_window`: RAR5's 4-bit log, RAR7's byte
    /// count, rounded up to a power of two and capped by `-mdx`).
    pub(super) fn start(window: usize) -> RarResult<Self> {
        Ok(Self {
            dec: DecoderState::new(window),
            enc: EncoderState::default(),
            enc_active: false,
        })
    }

    /// Grow the decoder window when the member about to be decoded declares
    /// a dictionary larger than the chain head's (official archives do this;
    /// the extraction path grows the same way). Without it, distances in the
    /// existing packed stream exceed the ring and the member decodes to
    /// wrong bytes.
    fn grow_to_member_dict(&mut self, archive: &RarArchive, idx: usize) -> RarResult<()> {
        let window = archive.member_dict_window(idx)?;
        if window > self.dec.window_capacity() {
            self.dec.grow_window(window);
        }
        Ok(())
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
        self.grow_to_member_dict(archive, idx)?;
        let hdr = &archive.entries[idx].header;
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(Vec::new());
        }
        let payload = archive.read_member_packed(reader, idx)?;
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
                ..compression::EncodeOptions::new(hdr.comp_method, encoder_dict_log(&hdr))
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
        // Only an actually encrypted member is re-encrypted: the archive
        // password may have been supplied for an unrelated reason (or for a
        // `-hp` archive whose payloads are plain), and encrypting a plain
        // member would silently change it.
        let member_encrypted = crate::crypto::parse_encryption_extra(&hdr.extra_data)?.is_some();
        let password = if member_encrypted {
            archive.password.as_deref()
        } else {
            None
        };
        let (header_crc, mut extra_data, stored_hash, encr) =
            RarArchive::payload_extra_and_crc(password, plain_crc, plain_blake);
        // Carry the member's own metadata records (nanosecond/ctime/atime
        // FILE_TIME, OWNER, ...) over to the rebuilt header; the ENCR/HASH
        // records just rebuilt are dropped from the old set (a fresh
        // encryption session invalidates the old ENCR).
        let kept = retain_extra_records(&hdr.extra_data, &[EXTRA_FILE_ENCRYPTION, EXTRA_FILE_HASH]);
        extra_data.extend_from_slice(&kept);
        let payload = RarArchive::encrypt_payload_with(encr.as_ref(), &payload);
        archive.write_file_entry(
            &MemberPlan {
                name: name.to_string(),
                unpacked_size: data.len() as u64,
                file_crc: header_crc,
                method,
                dict_size_log,
                dict_size_bytes,
                extra_data,
                attrs: hdr.attributes,
                mtime: hdr.mtime,
                solid: self.enc_active,
                stored_hash,
            },
            &payload,
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

/// The dictionary log the encoder window is sized from when a member is
/// recompressed. RAR7 carries the dictionary as a byte count (possibly
/// non-power-of-two) whose 5-bit log does not fit the 4-bit
/// `comp_dict_size`; use the byte count when present and cap at the
/// writer's 4 GiB window (log 15).
fn encoder_dict_log(hdr: &crate::model::FileHeader) -> u8 {
    let Some(bytes) = hdr.dict_size_bytes else {
        return hdr.comp_dict_size;
    };
    let mut log = 0u8;
    while log < 15 && (128u64 * 1024) << (log + 1) <= bytes {
        log += 1;
    }
    log
}
