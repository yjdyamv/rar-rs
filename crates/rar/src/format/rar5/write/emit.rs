//! RAR5 member emission: block headers, volume splitting and the
//! encryption/hash extra-record assembly shared by the add and stream
//! paths.

use crate::archive::{ArchiveEntry, RarArchive};
use crate::crypto;
use crate::error::{RarError, RarResult};
use crate::format::rar5::{
    BLOCK_FLAG_DATA_CONTINUE_TO, BLOCK_FLAG_DATA_CONTINUES, ENCR_PBKDF2_ITER_LOG, FILE_FLAG_CRC32,
    FILE_FLAG_TIME_UNIX, OS_UNIX,
};
use crate::format::shared::stream_mut;
use crate::model::{DataChunk, FileHeader};

/// Scalar member fields shared by both multi-volume split drivers; the
/// chunk headers (which repeat per volume) are built from these rather
/// than from the base header so payload-specific fields stay defaulted.
pub(super) struct SplitParams<'a> {
    pub(super) name: &'a str,
    pub(super) unpacked_size: u64,
    pub(super) attrs: u64,
    pub(super) mtime: u32,
    pub(super) method: u8,
    pub(super) solid: bool,
    pub(super) dict_size_log: u8,
    pub(super) dict_size_bytes: Option<u64>,
    pub(super) extra_data: &'a [u8],
}

/// Which half of a split chunk the per-chunk source closure is asked for.
/// The loop invokes it once in [SplitPhase::Crc] (before the block header is
/// emitted) and again in [SplitPhase::Write] (after it), so a phase-returning
/// `u64` is at once the chunk checksum or its on-disk `data_offset`.
#[derive(Clone, Copy)]
pub(super) enum SplitPhase {
    Crc,
    Write,
}

impl RarArchive {
    /// Write a file entry, splitting across volumes if needed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_file_entry(
        &mut self,
        name: &str,
        unpacked_size: u64,
        packed_data: &[u8],
        file_crc: u32,
        method: u8,
        dict_size_log: u8,
        dict_size_bytes: Option<u64>,
        extra_data: &[u8],
        attrs: u64,
        mtime: u32,
        solid: bool,
        hash_value: Option<[u8; 32]>,
    ) -> RarResult<()> {
        let (mtime, file_flags) =
            self.rar5_time_fields(mtime, FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32);
        let fh_base = FileHeader {
            name: name.to_string(),
            unpacked_size,
            packed_size: packed_data.len() as u64,
            attributes: attrs,
            mtime,
            crc32_val: Some(file_crc),
            hash_type: if hash_value.is_some() { 0 } else { u8::MAX },
            hash_value,
            comp_method: method,
            comp_solid: solid,
            comp_dict_size: dict_size_log,
            dict_size_bytes,
            host_os: OS_UNIX,
            file_flags,
            extra_data: extra_data.to_vec(),
            ..Default::default()
        };

        if self.write_ctx().output.volume_size.is_none() {
            // Single-volume
            let hdr_bytes = fh_base.to_bytes();
            if self.write_ctx().locator.quick_open {
                let pos = stream_mut(&mut self.stream)?.stream_position()?;
                self.write_ctx_mut()
                    .locator
                    .quick_open_entries
                    .push((pos, hdr_bytes.clone()));
            }
            self.write_block_header(&hdr_bytes)?;
            let stream = stream_mut(&mut self.stream)?;
            stream.write_all(packed_data)?;
            let data_offset = stream.stream_position()? - packed_data.len() as u64;
            let chunk = DataChunk {
                volume_index: 0,
                data_offset,
                packed_size: packed_data.len() as u64,
                crc32_val: Some(file_crc),
                is_final: true,
                extra_data: extra_data.to_vec(),
            };
            self.entries.push(ArchiveEntry {
                header: FileHeader {
                    data_offset,
                    ..fh_base
                },
                chunks: vec![chunk],
            });
            return Ok(());
        }

        // Multi-volume splitting
        let volume_size = self.write_ctx().output.volume_size.unwrap();
        // End-of-archive block: 8 plaintext bytes, or `[IV][padded]` when
        // header encryption wraps every block.
        let eoa_plain: u64 = 8;
        let eoa_size: u64 = self.on_disk_header_len(eoa_plain);
        let total_packed = packed_data.len() as u64;

        // Check if it fits in current volume
        let hdr_bytes = fh_base.to_bytes();
        let hdr_on_disk = self.on_disk_header_len(hdr_bytes.len() as u64);
        let total_needed = hdr_on_disk + total_packed + eoa_size;
        let remaining = volume_size.saturating_sub(self.write_ctx().output.bytes_written);

        if total_needed <= remaining {
            // Fits entirely
            self.write_block_header(&hdr_bytes)?;
            let stream = stream_mut(&mut self.stream)?;
            stream.write_all(packed_data)?;
            let data_offset = stream.stream_position()? - total_packed;
            self.write_ctx_mut().output.bytes_written += hdr_on_disk + total_packed;
            let chunk = DataChunk {
                volume_index: self.write_ctx().output.current_volume - 1,
                data_offset,
                packed_size: total_packed,
                crc32_val: Some(file_crc),
                is_final: true,
                extra_data: extra_data.to_vec(),
            };
            self.entries.push(ArchiveEntry {
                header: FileHeader {
                    data_offset,
                    ..fh_base
                },
                chunks: vec![chunk],
            });
            return Ok(());
        }

        // Need to split across volumes.
        let params = SplitParams {
            name,
            unpacked_size,
            attrs,
            mtime,
            method,
            solid,
            dict_size_log,
            dict_size_bytes,
            extra_data,
        };
        self.write_split_member(
            total_packed,
            params,
            volume_size,
            eoa_size,
            fh_base,
            |this, phase, offset, chunk_size, is_last| match phase {
                SplitPhase::Crc => {
                    if is_last {
                        Ok(file_crc as u64)
                    } else {
                        let chunk_packed =
                            &packed_data[offset as usize..(offset + chunk_size) as usize];
                        let mut h = crc32fast::Hasher::new();
                        h.update(chunk_packed);
                        Ok(h.finalize() as u64)
                    }
                }
                SplitPhase::Write => {
                    let chunk_packed =
                        &packed_data[offset as usize..(offset + chunk_size) as usize];
                    let stream = stream_mut(&mut this.stream)?;
                    stream.write_all(chunk_packed)?;
                    let data_offset = stream.stream_position()? - chunk_size;
                    Ok(data_offset)
                }
            },
        )
    }

    /// Drive the shared multi-volume split loop for a member whose packed
    /// payload must cross volume boundaries. The budget arithmetic, per-chunk
    /// header estimation, `chunk_extra` selection, volume transitions and the
    /// collected chunk bookkeeping live here once. Only the source-specific
    /// step is delegated: `phase` is invoked once with [SplitPhase::Crc] to
    /// compute the chunk's checksum (a probe pass for streamed payloads, a
    /// slice hash for in-memory data) before the block header is emitted, and
    /// again with [SplitPhase::Write] to write the chunk's bytes after the
    /// header and return its `data_offset` — preserving the on-disk
    /// [header][data] ordering.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn write_split_member(
        &mut self,
        total_packed: u64,
        params: SplitParams<'_>,
        volume_size: u64,
        eoa_size: u64,
        fh_base: FileHeader,
        mut phase: impl FnMut(&mut RarArchive, SplitPhase, u64, u64, bool) -> RarResult<u64>,
    ) -> RarResult<()> {
        let mut offset = 0u64;
        let mut chunks = Vec::new();
        let mut is_first = true;
        // Set when the previous iteration rolled to a new volume without
        // emitting anything: a second empty start means the volume size
        // cannot fit a header, so rolling can never make progress.
        let mut rolled = false;

        // Encrypted members: every chunk header carries the encryption
        // extra record (WinRAR repeats it on every volume). Non-final
        // chunks verify with a plain crc32 of the ciphertext chunk, so
        // their record must clear the hash-key MAC bit (flags=1); the
        // final chunk keeps the full record (flags=3, MAC'd checksum).
        let encr_params = if self.password.is_some() {
            crypto::parse_encryption_extra(params.extra_data)?
        } else {
            None
        };
        // WinRAR repeats the FILE_TIME record on every chunk so each
        // volume's own header is self-describing (middle volumes show the
        // member's nanoseconds); the other records stay on the first and
        // final chunks (the encryption record is per chunk by design).
        let file_time = file_time_record(params.extra_data);
        let chunk_extra = |is_last: bool, is_first: bool| -> Vec<u8> {
            let mut extra = if let Some(ref p) = encr_params {
                if is_last {
                    params.extra_data.to_vec()
                } else {
                    let mut np = p.clone();
                    np.flags &= !0x02;
                    np.to_extra_bytes()
                }
            } else if is_last || is_first {
                params.extra_data.to_vec()
            } else {
                Vec::new()
            };
            if !is_last
                && !is_first
                && let Some(time) = &file_time
            {
                extra.extend_from_slice(time);
            }
            extra
        };

        while offset < total_packed {
            self.check_cancel()?;
            let remaining_vol = volume_size.saturating_sub(self.write_ctx().output.bytes_written);

            // Build chunk flags
            let mut block_flags: u64 = 0;
            if !is_first {
                block_flags |= BLOCK_FLAG_DATA_CONTINUES;
            }

            // Estimate header sizes. The final chunk's extra area can be
            // larger than a mid chunk's (BLAKE2sp hash / OWNER records, the
            // full encryption record), so a chunk sized by the mid estimate
            // alone can turn out to be the last one and overflow the volume
            // by the extra-record delta. Budget against both.
            let (chunk_mtime, chunk_flags) =
                self.rar5_time_fields(params.mtime, FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32);
            let chunk_fh = FileHeader {
                name: params.name.to_string(),
                unpacked_size: params.unpacked_size,
                packed_size: remaining_vol.max(1),
                attributes: params.attrs,
                mtime: chunk_mtime,
                crc32_val: Some(0),
                comp_method: params.method,
                comp_solid: params.solid,
                comp_dict_size: params.dict_size_log,
                dict_size_bytes: params.dict_size_bytes,
                host_os: OS_UNIX,
                flags: block_flags | BLOCK_FLAG_DATA_CONTINUE_TO,
                file_flags: chunk_flags,
                extra_data: chunk_extra(false, is_first),
                ..Default::default()
            };
            let hdr_size = self.on_disk_header_len(chunk_fh.to_bytes().len() as u64);
            // The final header carries the full extra area; its other fields
            // are no longer than the estimate's (`packed_size` is at most
            // `remaining_vol`, the continue-to flag is cleared), so this
            // length bounds the header actually written for a final chunk.
            let last_hdr_size = self.on_disk_header_len(
                FileHeader {
                    extra_data: chunk_extra(true, is_first),
                    ..chunk_fh
                }
                .to_bytes()
                .len() as u64,
            );

            let bytes_for_data = remaining_vol.saturating_sub(hdr_size + eoa_size);
            let bytes_for_last = remaining_vol.saturating_sub(last_hdr_size + eoa_size);
            let remaining_member = total_packed - offset;

            // Middle chunks keep the mid estimate's budget; a chunk that
            // would swallow the member tail only does so when it also fits
            // with the final header, otherwise it stops at `bytes_for_last`
            // (rolling the volume when that budget is zero) and leaves the
            // tail for the next volume.
            let chunk_size = if remaining_member <= bytes_for_last {
                remaining_member
            } else if remaining_member <= bytes_for_data {
                bytes_for_last
            } else {
                bytes_for_data
            };
            if chunk_size == 0 {
                if rolled {
                    return Err(RarError::InvalidOption(format!(
                        "volume size {volume_size} is too small for a member header ({hdr_size} bytes) plus the end block"
                    )));
                }
                self.start_next_volume()?;
                is_first = false;
                rolled = true;
                continue;
            }
            rolled = false;

            let is_last = offset + chunk_size >= total_packed;

            // Set final flags
            if is_last {
                block_flags &= !BLOCK_FLAG_DATA_CONTINUE_TO;
            } else {
                block_flags |= BLOCK_FLAG_DATA_CONTINUE_TO;
            }

            let chunk_crc = phase(self, SplitPhase::Crc, offset, chunk_size, is_last)? as u32;

            let (final_mtime, final_flags) =
                self.rar5_time_fields(params.mtime, FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32);
            let final_fh = FileHeader {
                name: params.name.to_string(),
                unpacked_size: params.unpacked_size,
                packed_size: chunk_size,
                attributes: params.attrs,
                mtime: final_mtime,
                crc32_val: Some(chunk_crc),
                comp_method: params.method,
                comp_solid: params.solid,
                comp_dict_size: params.dict_size_log,
                dict_size_bytes: params.dict_size_bytes,
                host_os: OS_UNIX,
                flags: block_flags,
                file_flags: final_flags,
                extra_data: chunk_extra(is_last, is_first),
                ..Default::default()
            };

            let final_hdr = final_fh.to_bytes();
            let final_hdr_disk = self.on_disk_header_len(final_hdr.len() as u64);
            self.write_block_header(&final_hdr)?;
            let data_offset = phase(self, SplitPhase::Write, offset, chunk_size, is_last)?;
            self.write_ctx_mut().output.bytes_written += final_hdr_disk + chunk_size;

            chunks.push(DataChunk {
                volume_index: self.write_ctx().output.current_volume - 1,
                data_offset,
                packed_size: chunk_size,
                crc32_val: Some(chunk_crc),
                is_final: is_last,
                extra_data: chunk_extra(is_last, is_first),
            });

            offset += chunk_size;
            is_first = false;

            if !is_last {
                self.start_next_volume()?;
            }
        }

        self.entries.push(ArchiveEntry {
            header: FileHeader {
                packed_size: total_packed,
                ..fh_base
            },
            chunks,
        });

        Ok(())
    }

    /// Build the header CRC, extra-area records (encryption + BLAKE2sp)
    /// and stored hash value for a member, plus the per-member encryption
    /// session reused for the payload (one KDF/salt per member). For
    /// encrypted members the checksums are MAC'd with the hash key,
    /// matching WinRAR.
    #[allow(clippy::type_complexity)]
    pub(crate) fn payload_extra_and_crc(
        password: Option<&str>,
        plain_crc: u32,
        plain_blake: Option<[u8; 32]>,
    ) -> (
        u32,
        Vec<u8>,
        Option<[u8; 32]>,
        Option<crypto::MemberEncryption>,
    ) {
        if let Some(password) = password {
            let session = crypto::MemberEncryption::generate(password, ENCR_PBKDF2_ITER_LOG);
            let header_crc = session.mac_crc32(plain_crc);
            let stored_hash = plain_blake.map(|h| session.mac_hash32(h));
            let mut extra = session.extra_bytes();
            if let Some(h) = stored_hash {
                extra.extend(crate::format::rar5::headers::hash_extra_record(h));
            }
            (header_crc, extra, stored_hash, Some(session))
        } else {
            let mut extra = Vec::new();
            if let Some(h) = plain_blake {
                extra.extend(crate::format::rar5::headers::hash_extra_record(h));
            }
            (plain_crc, extra, plain_blake, None)
        }
    }

    /// Encrypt a member payload with the session returned by
    /// [`Self::payload_extra_and_crc`].
    pub(crate) fn encrypt_payload_with(
        session: Option<&crypto::MemberEncryption>,
        plaintext: &[u8],
    ) -> Vec<u8> {
        match session {
            Some(session) => session.encrypt(plaintext),
            None => plaintext.to_vec(),
        }
    }
}

/// The FILE_TIME (0x03) record from a RAR5 extra area, if present.
fn file_time_record(extra: &[u8]) -> Option<Vec<u8>> {
    let mut offset = 0usize;
    while offset < extra.len() {
        let (size, n) = crate::format::rar5::vint::decode_from_slice(extra, offset).ok()?;
        let record_start = offset;
        offset += n;
        let size = usize::try_from(size).ok()?;
        let end = offset.checked_add(size)?;
        if end > extra.len() {
            return None;
        }
        let (rec_type, _) = crate::format::rar5::vint::decode_from_slice(extra, offset).ok()?;
        if rec_type == crate::format::rar5::EXTRA_FILE_TIME {
            return Some(extra[record_start..end].to_vec());
        }
        offset = end;
    }
    None
}
