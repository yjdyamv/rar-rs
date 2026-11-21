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
use crate::engine::MemberPlan;
use crate::error::RarResult;
use crate::format::rar5::COMP_METHOD_STORE;
use crate::format::rar5::headers::retain_extra_records;
use crate::format::rar5::payload::ChunkReader;
use crate::format::rar5::{EXTRA_FILE_ENCRYPTION, EXTRA_FILE_HASH, EXTRA_FILE_TIME};

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
        let window = crate::format::rar5::extract::decode::member_dict_window(archive, idx)?;
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
        crate::format::rar5::extract::verify::verify_integrity(
            archive,
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
        self.emit_encoded(archive, idx, name, &data, 0, 0, false)
    }

    /// Replace member `idx` (a chain member) with new `data`: advance the
    /// shared DECODER window with the member's ORIGINAL bytes (so the chain's
    /// later members still decode against the window they were packed with),
    /// then encode the NEW bytes into the shared ENCODER window and emit the
    /// rebuilt header at the member's own slot.
    pub(super) fn replace_solid_member<R: ChunkReader + ?Sized>(
        &mut self,
        archive: &mut RarArchive,
        reader: &mut R,
        idx: usize,
        data: &[u8],
        mtime: u32,
        mtime_ns: u64,
    ) -> RarResult<()> {
        // Decode the old member to advance `dec`; its plaintext is discarded.
        let _old = self.decode_member(archive, reader, idx)?;
        let name = archive.entries[idx].header.name.clone();
        self.emit_encoded(archive, idx, &name, data, mtime, mtime_ns, true)
    }

    /// Compress `data` into the shared encoder window and emit the rebuilt
    /// member header at `idx`. `rebuild_time` drops the member's old
    /// FILE_TIME record and stamps `mtime`/`mtime_ns` afresh (a content
    /// replace); otherwise the member's own time records are kept unchanged
    /// (a plain recompression, whose `hdr.mtime` is reused).
    #[allow(clippy::too_many_arguments)]
    fn emit_encoded(
        &mut self,
        archive: &mut RarArchive,
        idx: usize,
        name: &str,
        data: &[u8],
        mtime: u32,
        mtime_ns: u64,
        rebuild_time: bool,
    ) -> RarResult<()> {
        // Cloned so the header borrow does not outlive the encoding below.
        let hdr = archive.entries[idx].header.clone();

        let plain_crc = crc32fast::hash(data);
        let plain_blake = hdr
            .hash_value
            .map(|_| crate::format::rar5::blake2sp::hash(data));
        let variant = crate::version::ArchiveVersion::from_v70(hdr.dict_size_bytes.is_some());
        let packed = compression::encode_chunked(
            data,
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
            (COMP_METHOD_STORE, 0u8, None, data.to_vec())
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
            crate::format::rar5::write::emit::payload_extra_and_crc(
                password,
                plain_crc,
                plain_blake,
            );
        // Carry the member's own metadata records (nanosecond/ctime/atime
        // FILE_TIME, OWNER, ...) over to the rebuilt header; the ENCR/HASH
        // records just rebuilt are dropped from the old set (a fresh
        // encryption session invalidates the old ENCR), and a content replace
        // also drops the stale FILE_TIME record.
        let mut drop_records = vec![EXTRA_FILE_ENCRYPTION, EXTRA_FILE_HASH];
        if rebuild_time {
            drop_records.push(EXTRA_FILE_TIME);
        }
        let kept = retain_extra_records(&hdr.extra_data, &drop_records);
        extra_data.extend_from_slice(&kept);
        // On Windows (and for sub-second times) the time travels in a
        // FILE_TIME record, not the 4-byte Unix field, so the reader would
        // otherwise see mtime 0; rebuild the record from the new mtime.
        if rebuild_time
            && (crate::format::rar5::write::emit::time_uses_windows_record() || mtime_ns != 0)
        {
            extra_data.extend_from_slice(
                &crate::format::rar5::headers::file_time_extra_record_windows(
                    Some((u64::from(mtime), mtime_ns as u32)),
                    None,
                    None,
                ),
            );
        }
        let payload =
            crate::format::rar5::write::emit::encrypt_payload_with(encr.as_ref(), &payload);
        crate::format::rar5::write::emit::write_file_entry(
            archive,
            &MemberPlan {
                name: name.to_string(),
                unpacked_size: data.len() as u64,
                file_crc: header_crc,
                method,
                dict_size_log,
                dict_size_bytes,
                extra_data,
                attrs: hdr.attributes,
                mtime: if rebuild_time { mtime } else { hdr.mtime },
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
            crate::format::shared::extract::max_packed_bytes(self),
            || Ok(()),
        )
    }

    /// Replace member `idx`'s content in place: recompress `data` standalone
    /// (a fresh non-solid window) and emit the rebuilt header at the member's
    /// own slot. `mtime`/`mtime_ns` stamp the new header; the member's other
    /// metadata records (OWNER, ...) are carried over.
    pub(super) fn emit_replacement(
        &mut self,
        idx: usize,
        data: &[u8],
        mtime: u32,
        mtime_ns: u64,
    ) -> RarResult<()> {
        if crate::format::rar5::extract::solid::is_solid_chain_member(self, idx) {
            return Err(crate::error::RarError::unsupported(
                "cannot replace a member of a solid chain in place",
            ));
        }
        let hdr = self.entries[idx].header.clone();
        if hdr.is_directory {
            return Err(crate::error::RarError::unsupported(
                "cannot replace a directory member in place",
            ));
        }
        let window = crate::format::rar5::extract::decode::member_dict_window(self, idx)?;
        let mut state = SolidChainState::start(window)?;
        state.emit_encoded(self, idx, &hdr.name, data, mtime, mtime_ns, true)
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
