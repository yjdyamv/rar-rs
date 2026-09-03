//! Write lifecycle: opening the write stream, the archive-header envelope,
//! finalization (quick-open / recovery records / end block) and volume
//! rolling. Methods on [RarArchive] in a sibling impl block (see
//! `crate::archive::mod` for the shared state).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::{Mode, PendingCommit, RarArchive, volume_base_of, volume_path, volume_path_padded};
use crate::crypto;
use crate::error::{RarError, RarResult};
use crate::io_util::{read_write_create, replace_file, temp_sibling_path, temp_suffix};
use crate::rar50::headers::*;
use crate::rar50::vint;
use crate::rar50::{
    ARCHIVE_FLAG_RECOVERY, ARCHIVE_FLAG_SOLID, ARCHIVE_FLAG_VOLUME, BLOCK_FLAG_EXTRA_DATA,
    BLOCK_TYPE_ARCHIVE_HEADER, ENCR_IV_SIZE, ENCR_PBKDF2_ITER_LOG, END_FLAG_NEXT_VOLUME,
    RAR5_SIGNATURE,
};

impl RarArchive {
    // ── Lifecycle ──────────────────────────────────────────────────────────

    pub(super) fn open_write(&mut self) -> RarResult<()> {
        if let Some(volume_size) = self.write_ctx().volume_size {
            if volume_size == 0 {
                return Err(RarError::Format(
                    "volume size must be greater than zero".into(),
                ));
            }
            let base = volume_base_of(&self.path);
            let parent = self.path.parent().unwrap_or(Path::new(".")).to_path_buf();
            // Stage the volumes under a temporary volume base; they are
            // moved over the final `{base}.partN.rar` names on close.
            let tmp_base = format!(".{base}.rar5tmp-{}", temp_suffix());
            self.volume_paths = vec![volume_path(&parent, &base, 1)];
            self.write_ctx_mut().current_volume = 1;
            self.write_ctx_mut().pending = Some(PendingCommit::Volumes {
                parent: parent.clone(),
                tmp_base: tmp_base.clone(),
                final_base: base,
            });
            let f = read_write_create(&volume_path(&parent, &tmp_base, 1))?;
            self.stream = Some(Box::new(f));
            self.write_signature()?;
            self.write_archive_encryption_header_if_needed()?;
            self.write_archive_header_vol(None)?;
            self.write_ctx_mut().volume_bytes_written =
                self.stream.as_mut().unwrap().stream_position()?;
            return Ok(());
        }

        // Stage the archive under a temporary sibling name; it is moved
        // over the final path on close, so a failed or interrupted
        // creation never leaves a partial archive at the target path.
        let tmp_path = temp_sibling_path(&self.path);
        self.write_ctx_mut().pending = Some(PendingCommit::Single(tmp_path.clone()));
        let f = read_write_create(&tmp_path)?;
        self.stream = Some(Box::new(f));
        self.write_signature()?;
        self.write_archive_encryption_header_if_needed()?;
        self.write_archive_header()?;
        Ok(())
    }

    /// Write the plaintext archive-level encryption header block (type 0x04)
    /// when header encryption is on, generating the archive params once.
    ///
    /// WinRAR writes this header at the start of EVERY volume of a
    /// header-encrypted multi-volume set (same salt/check on each volume);
    /// every block after it is `[16-byte IV][AES-256-CBC encrypted header]`.
    pub(super) fn write_archive_encryption_header_if_needed(&mut self) -> RarResult<()> {
        if !self.header_encryption {
            return Ok(());
        }
        if self.archive_encr.is_none() {
            let password = self.password.as_ref().ok_or_else(|| {
                RarError::Encrypted("header encryption requires a password".into())
            })?;
            let encr =
                crypto::EncryptionParams::generate_for_password(password, ENCR_PBKDF2_ITER_LOG);
            self.archive_encr = Some(encr);
        }
        let block = self
            .archive_encr
            .as_ref()
            .unwrap()
            .to_archive_header_block();
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(&block)?;
        Ok(())
    }

    /// Verify the password against a parsed archive-encryption (header
    /// encryption) record and enable header encryption on this archive.
    /// Shared by every path that encounters the leading `BLOCK_TYPE_ENCRYPT_HEADER`.
    pub(crate) fn handle_archive_encrypt_header(
        &mut self,
        params: crypto::EncryptionParams,
    ) -> RarResult<()> {
        let password = self.password.as_ref().ok_or_else(|| {
            RarError::Encrypted("archive has encrypted headers; provide a password".into())
        })?;
        if !params.verify_password(password) {
            return Err(RarError::WrongPassword);
        }
        self.archive_encr = Some(params);
        self.header_encryption = true;
        Ok(())
    }

    /// On-disk size of a block header: header encryption wraps every header
    /// in `[16-byte IV][PKCS7-padded ciphertext]`.
    pub(crate) fn on_disk_header_len(&self, plain_len: u64) -> u64 {
        if self.header_encryption {
            16 + ((plain_len + 15) & !15)
        } else {
            plain_len
        }
    }

    /// Write a block header, wrapping it in `[16-byte IV][AES-256-CBC
    /// encrypted header]` when header encryption is enabled.
    pub(crate) fn write_block_header(&mut self, header_bytes: &[u8]) -> RarResult<()> {
        let stream = self.stream.as_mut().unwrap();
        if let Some(ref encr) = self.archive_encr {
            let password = self
                .password
                .as_ref()
                .ok_or_else(|| RarError::Encrypted("no password set".into()))?;
            let key = encr.get_key(password);
            let mut iv = [0u8; ENCR_IV_SIZE];
            rand::fill(&mut iv);
            let ciphertext = crypto::encrypt_data(header_bytes, &key, &iv);
            stream.write_all(&iv)?;
            stream.write_all(&ciphertext)?;
        } else {
            stream.write_all(header_bytes)?;
        }
        Ok(())
    }

    /// Finalize the archive (writes end-of-archive block in write mode).
    pub fn close(&mut self) -> RarResult<()> {
        self.check_cancel()?;
        self.finish_writing()?;
        self.stream = None;
        // Move the staged files over their final paths: only now does the
        // archive become visible at the target path. On failure the staged
        // files are left for [`Drop`] to clean up.
        self.commit_pending()?;
        if self.recovery_volumes_percent.is_some() || self.recovery_volumes_count.is_some() {
            self.check_cancel()?;
            self.write_recovery_volumes()?;
        }
        Ok(())
    }

    /// Write the trailing service records (quick-open, recovery) and the
    /// end-of-archive block. The stream is left open so a caller can take
    /// it back afterwards (in-memory sink seam).
    pub(super) fn finish_writing(&mut self) -> RarResult<()> {
        if self.stream.is_some() && (self.mode == Mode::Write || self.mode == Mode::Append) {
            let qo_offset = if self.write_ctx().quick_open {
                Some(self.write_quick_open_record()?)
            } else {
                None
            };
            let rr_offset = if self.recovery_percent.is_some() {
                Some(self.stream.as_mut().unwrap().stream_position()?)
            } else {
                None
            };
            if rr_offset.is_some() {
                // The final main header (with the real QO/RR offsets) must
                // be in place before the parity is computed: the RR
                // protects the raw archive bytes including the main header.
                self.patch_main_header_locator(qo_offset, rr_offset)?;
                self.write_recovery_record()?;
            } else if self.write_ctx().quick_open {
                self.patch_main_header_locator(qo_offset, None)?;
            }
            self.write_end_block()?;
            self.mode = Mode::Read; // prevent double-write
        }
        Ok(())
    }

    /// Finish writing and hand the underlying stream back (test seam for
    /// in-memory archives; the caller owns the sink afterwards).
    #[cfg(test)]
    pub(crate) fn finish_into_sink(mut self) -> RarResult<Box<dyn super::ArchiveStream>> {
        self.finish_writing()?;
        self.stream
            .take()
            .ok_or_else(|| RarError::Format("no archive stream to take".into()))
    }

    /// Generate the `.rev` recovery-volume files for a completed
    /// multi-volume archive set (WinRAR `-rv` equivalent).
    pub(super) fn write_recovery_volumes(&mut self) -> RarResult<()> {
        // Exact count wins; the percent variant is converted at close time.
        let nd = self.volume_paths.len();
        let rec_count = if let Some(count) = self.recovery_volumes_count {
            (count as usize).min(nd)
        } else if let Some(percent) = self.recovery_volumes_percent {
            crate::recovery::rev50::plan_recovery_volume_count(nd, percent as u64)?
        } else {
            return Ok(());
        };

        let written =
            crate::recovery::rev50::build_recovery_volumes_for_set(&self.volume_paths, rec_count)?;
        let _ = written;
        self.recovery_volumes_percent = None;
        Ok(())
    }

    /// Compute the RAR5 recovery record over the archive written so far
    /// and append the `"RR"` service header. The main header locator was
    /// already patched by [`Self::close`].
    pub(super) fn write_recovery_record(&mut self) -> RarResult<()> {
        let path = self.write_file_path().to_path_buf();
        self.write_recovery_record_from(&path)
    }

    /// The file currently being written: the staged temporary sibling
    /// during an uncommitted create/append, the final path otherwise.
    pub(super) fn write_file_path(&self) -> &Path {
        match &self.write_ctx().pending {
            Some(PendingCommit::Single(tmp)) => tmp,
            _ => &self.path,
        }
    }

    /// Move the staged write files over their final paths. Called on
    /// successful close only; on failure the staged files are left in
    /// place for [`Drop`] to clean up.
    pub(super) fn commit_pending(&mut self) -> RarResult<()> {
        // An archive opened purely for reading has no write context (and no
        // staged pending commit); `close` runs on every `Drop`, so this
        // must be a no-op there rather than panic in `write_ctx_mut`.
        let Some(pending) = self.write.as_mut().and_then(|w| w.pending.take()) else {
            return Ok(());
        };
        let result = match &pending {
            PendingCommit::Single(tmp) => replace_file(tmp, &self.path),
            PendingCommit::Volumes {
                parent,
                tmp_base,
                final_base,
            } => {
                // WinRAR zero-pads the part number to the digit count of
                // the total volume count (part01..part15 for 10+ volumes);
                // the final names carry the same padding, and the staged
                // `.rev` naming (write_recovery_volumes) follows it.
                let nd = self.volume_paths.len();
                let width = nd.to_string().len().max(1);
                let mut last = Ok(());
                let mut final_paths = Vec::with_capacity(nd);
                for n in 1..=nd {
                    let tmp = volume_path(parent, tmp_base, n);
                    let final_path = volume_path_padded(parent, final_base, n, width);
                    if let Err(e) = replace_file(&tmp, &final_path) {
                        last = Err(e);
                        break;
                    }
                    final_paths.push(final_path);
                }
                if last.is_ok() {
                    self.volume_paths = final_paths;
                }
                last
            }
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) => {
                // Keep the pending state so the drop guard removes any
                // staged files that were not committed.
                self.write_ctx_mut().pending = Some(pending);
                Err(e)
            }
        }
    }

    /// Append the `"RR"` service header with parity over the archive
    /// prefix read from `prefix_path` (the file being written: the archive
    /// itself in append mode, the replacement file during a rewrite).
    pub(super) fn write_recovery_record_from(&mut self, prefix_path: &Path) -> RarResult<()> {
        let percent = self.recovery_percent.unwrap_or(0) as u64;
        let stream = self.stream.as_mut().unwrap();
        let archive_size = stream.stream_position()?;
        if archive_size > super::MAX_RECOVERY_PREFIX_BYTES {
            return Err(RarError::LimitExceeded {
                limit: super::MAX_RECOVERY_PREFIX_BYTES,
                context: format!(
                    "recovery record prefix is {archive_size} bytes; streaming recovery records are not supported"
                ),
            });
        }

        // Read the archive prefix (everything written so far). The write
        // stream is write-only (File::create), so use a separate handle.
        let mut prefix = vec![0u8; archive_size as usize];
        {
            let mut reader = std::fs::File::open(prefix_path)?;
            reader.read_exact(&mut prefix)?;
        }

        let rr_data =
            crate::recovery::rar50::build_structural_inline_recovery_data(&prefix, percent)
                .map_err(|e| RarError::Format(format!("recovery record encode: {e}")))?;

        // RR service header: type 3, name "RR", SubData = percent byte.
        let subdata = {
            let rec = vec![percent as u8]; // recovery percent (single byte, <= 100)
            let mut extra = Vec::new();
            extra.extend(vint::encode((1 + rec.len()) as u64)); // record size: type + data
            extra.extend(vint::encode(0x07u64)); // service data record type
            extra.extend(rec);
            extra
        };
        let hdr = crate::rar50::headers::build_service_block(
            "RR",
            &subdata,
            rr_data.len() as u64,
            crate::rar50::BLOCK_FLAG_SKIP_IF_UNKNOWN,
        );

        self.write_block_header(&hdr)?;
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(&rr_data)?;
        Ok(())
    }

    /// Write the quick-open ("QO") service record at the end of the
    /// archive, caching a full copy of every file header. Returns the
    /// absolute offset of the record for the main-header locator.
    pub(super) fn write_quick_open_record(&mut self) -> RarResult<u64> {
        let stream = self.stream.as_mut().unwrap();
        let qo_pos = stream.stream_position()?;

        let mut payload = Vec::new();
        for (offset, header) in &self.write_ctx().quick_open_entries {
            let rel = qo_pos.checked_sub(*offset).ok_or_else(|| {
                RarError::Format("quick-open cached header is after the QO record".into())
            })?;
            let mut body = Vec::new();
            body.extend(vint::encode(0u64)); // entry flags: file header
            body.extend(vint::encode(rel));
            body.extend(vint::encode(header.len() as u64));
            body.extend_from_slice(header);
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&body);
            let crc = hasher.finalize();
            payload.extend(crc.to_le_bytes());
            payload.extend(vint::encode(body.len() as u64));
            payload.extend(body);
        }

        // Service header: type 3, name "QO", with an empty service-data
        // extra record (type 0x07) and the payload as its data area.
        let subdata = {
            let mut extra = Vec::new();
            extra.extend(vint::encode(1u64)); // record size: type only
            extra.extend(vint::encode(0x07u64)); // service data record type
            extra
        };
        let hdr = crate::rar50::headers::build_service_block(
            "QO",
            &subdata,
            payload.len() as u64,
            crate::rar50::BLOCK_FLAG_SKIP_IF_UNKNOWN,
        );

        self.write_block_header(&hdr)?;
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(&payload)?;
        Ok(qo_pos)
    }

    /// Rewrite the main archive header with the real quick-open and/or
    /// recovery-record offsets. Locator fields are stored as vints
    /// relative to the archive start (after the 8-byte signature), matching
    /// WinRAR; fields were preallocated as fixed 5-byte vints at header
    /// write time.
    pub(super) fn patch_main_header_locator(
        &mut self,
        qo_offset: Option<u64>,
        rr_offset: Option<u64>,
    ) -> RarResult<()> {
        let start = self
            .write_ctx()
            .main_header_start
            .ok_or_else(|| RarError::Format("main header position unknown".into()))?;

        // Rebuild the main header: read it back from the stream (plaintext
        // or decrypted), so the patch also works for in-memory sinks.
        let plain = if self.header_encryption {
            let encr = self
                .archive_encr
                .as_ref()
                .ok_or_else(|| RarError::Format("no archive encryption params".into()))?;
            let password = self
                .password
                .as_ref()
                .ok_or_else(|| RarError::Encrypted("no password set".into()))?;
            let key = encr.get_key(password);
            let stream = self.stream.as_mut().unwrap();
            let mut iv = [0u8; 16];
            stream.seek(SeekFrom::Start(start))?;
            stream.read_exact(&mut iv)?;
            // Decrypt the first block to learn the header size.
            let mut first = [0u8; 16];
            stream.read_exact(&mut first)?;
            let first_pt = crypto::decrypt_data(&first, &key, &iv)?;
            let (hsize, vint_len) = vint::decode_from_slice(&first_pt, 4)
                .map_err(|e| RarError::Format(format!("main header vint: {e}")))?;
            let total_raw = 4 + vint_len + hsize as usize;
            let enc_size = total_raw.div_ceil(16) * 16;
            let mut full_ct = vec![0u8; enc_size];
            full_ct[..16].copy_from_slice(&first);
            if enc_size > 16 {
                stream.read_exact(&mut full_ct[16..])?;
            }
            let full_pt = crypto::decrypt_data(&full_ct, &key, &iv)?;
            full_pt[..total_raw].to_vec()
        } else {
            let stream = self.stream.as_mut().unwrap();
            stream.seek(SeekFrom::Start(start))?;
            // Read the whole header: parse the size first.
            let mut crc_hdr = [0u8; 5];
            stream.read_exact(&mut crc_hdr)?;
            let (hsize, vint_len) = vint::decode_from_slice(&crc_hdr, 4)
                .map_err(|e| RarError::Format(format!("main header vint: {e}")))?;
            let total = 4 + vint_len + hsize as usize;
            let mut hdr = vec![0u8; total];
            hdr[..5].copy_from_slice(&crc_hdr);
            stream.read_exact(&mut hdr[5..])?;
            hdr
        };

        let mut new_header = plain;
        let base = self.sfx_offset + RAR5_SIGNATURE.len() as u64;
        crate::rar50::headers::locator::patch_locator_fields(
            &mut new_header,
            qo_offset,
            rr_offset,
            self.write_ctx().qo_offset_field_pos.map(|p| p as usize),
            self.write_ctx().rr_offset_field_pos.map(|p| p as usize),
            base,
        )?;

        let stream = self.stream.as_mut().unwrap();
        if self.header_encryption {
            let encr = self
                .archive_encr
                .as_ref()
                .ok_or_else(|| RarError::Format("no archive encryption params".into()))?;
            let password = self
                .password
                .as_ref()
                .ok_or_else(|| RarError::Encrypted("no password set".into()))?;
            let key = encr.get_key(password);
            let mut iv = [0u8; 16];
            rand::fill(&mut iv);
            let ciphertext = crypto::encrypt_data(&new_header, &key, &iv);
            stream.seek(SeekFrom::Start(start))?;
            stream.write_all(&iv)?;
            stream.write_all(&ciphertext)?;
        } else {
            stream.seek(SeekFrom::Start(start))?;
            stream.write_all(&new_header)?;
        }
        stream.seek(SeekFrom::End(0))?;
        Ok(())
    }

    // ── Signature ──────────────────────────────────────────────────────────

    pub(super) fn write_signature(&mut self) -> RarResult<()> {
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(RAR5_SIGNATURE)?;
        Ok(())
    }

    // ── Writing ────────────────────────────────────────────────────────────

    pub(super) fn write_archive_header(&mut self) -> RarResult<()> {
        if self.recovery_percent.is_some() || self.write_ctx().quick_open {
            return self.write_archive_header_with_locators();
        }
        let hdr = ArchiveHeader {
            flags: if self.write_ctx().solid_mode {
                ARCHIVE_FLAG_SOLID
            } else {
                0
            },
            extra_data: Vec::new(),
            volume_number: None,
        };
        let hdr_bytes = hdr.to_bytes();
        self.write_block_header(&hdr_bytes)
    }

    /// Write the main archive header with a locator record for the
    /// quick-open and/or recovery-record offsets, plus the archive flags
    /// (`MHFL_RECOVERY`, `MHFL_SOLID`) as needed.
    ///
    /// The offset fields are preallocated as fixed 5-byte vints so the
    /// header length never changes; the real offsets are patched in at
    /// close time.
    pub(super) fn write_archive_header_with_locators(&mut self) -> RarResult<()> {
        // Locator record body: [flags vint][qo offset vint][rr offset vint]
        // (only the offsets whose flags are set). The byte rules live once
        // in headers::locator.
        let quick_open = self.write_ctx().quick_open;
        let recovery = self.recovery_percent.is_some();
        let (locator, qo_field_pos, rr_field_pos) =
            crate::rar50::headers::locator::build_locator_body(quick_open, recovery);

        let mut extra = Vec::new();
        extra.extend(crate::rar50::headers::locator::frame_locator_record(
            &locator,
        ));

        let mut arch_flags = 0u64;
        if self.recovery_percent.is_some() {
            arch_flags |= ARCHIVE_FLAG_RECOVERY;
        }
        if self.write_ctx().solid_mode {
            arch_flags |= ARCHIVE_FLAG_SOLID;
        }

        let body = [
            vint::encode(BLOCK_TYPE_ARCHIVE_HEADER),
            vint::encode(BLOCK_FLAG_EXTRA_DATA),
            vint::encode(extra.len() as u64),
            vint::encode(arch_flags),
        ]
        .concat();
        let mut content = body;
        content.extend(&extra);

        let size_bytes = vint::encode(content.len() as u64);
        let mut header_content = Vec::with_capacity(size_bytes.len() + content.len());
        header_content.extend(&size_bytes);
        header_content.extend(&content);

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header_content);
        let crc = hasher.finalize();

        let mut out = Vec::with_capacity(4 + header_content.len());
        out.extend(crc.to_le_bytes());
        out.extend(header_content);

        let main_header_start = self.stream.as_mut().unwrap().stream_position()?;
        self.write_block_header(&out)?;
        self.write_ctx_mut().main_header_start = Some(main_header_start);
        // Plaintext-relative index of the locator body (flags vint then
        // the preallocated offset fields): crc(4) + hsize vint + block
        // type + block flags + extra size + archive flags + record size +
        // locator type.
        let field_base = 4u64
            + size_bytes.len() as u64
            + vint::encoded_size(BLOCK_TYPE_ARCHIVE_HEADER) as u64
            + vint::encoded_size(BLOCK_FLAG_EXTRA_DATA) as u64
            + vint::encoded_size(extra.len() as u64) as u64
            + vint::encoded_size(arch_flags) as u64
            + vint::encoded_size(locator.len() as u64) as u64
            + vint::encoded_size(crate::rar50::headers::locator::LOCATOR_TYPE) as u64;
        if let Some(p) = qo_field_pos {
            self.write_ctx_mut().qo_offset_field_pos = Some(field_base + p as u64);
        }
        if let Some(p) = rr_field_pos {
            self.write_ctx_mut().rr_offset_field_pos = Some(field_base + p as u64);
        }
        Ok(())
    }

    pub(super) fn write_archive_header_vol(&mut self, volume_number: Option<u64>) -> RarResult<()> {
        let hdr = ArchiveHeader {
            flags: ARCHIVE_FLAG_VOLUME,
            extra_data: Vec::new(),
            volume_number,
        };
        let hdr_bytes = hdr.to_bytes();
        self.write_block_header(&hdr_bytes)
    }

    pub(super) fn write_end_block(&mut self) -> RarResult<()> {
        self.write_end_block_flags(false)
    }

    pub(super) fn write_end_block_flags(&mut self, next_volume: bool) -> RarResult<()> {
        let flags = if next_volume { END_FLAG_NEXT_VOLUME } else { 0 };
        let eoa = EndOfArchiveHeader { flags };
        let hdr_bytes = eoa.to_bytes();
        self.write_block_header(&hdr_bytes)
    }

    pub(crate) fn start_next_volume(&mut self) -> RarResult<()> {
        // WinRAR `-sv`: always reset the solid statistics at the start of a
        // new volume so each volume is an independent solid group.
        if self.write_ctx().solid_mode
            && self.write_ctx().solid_reset == crate::options::SolidReset::PerVolume
        {
            self.write_ctx_mut().encoder_state = None;
            self.write_ctx_mut().last_solid_ext = None;
        }
        self.write_end_block_flags(true)?;
        // Close current volume
        self.stream = None;
        self.write_ctx_mut().current_volume += 1;
        let (parent, tmp_base, final_base) = match &self.write_ctx().pending {
            Some(PendingCommit::Volumes {
                parent,
                tmp_base,
                final_base,
            }) => (parent.clone(), tmp_base.clone(), final_base.clone()),
            // Volume creation only happens in multivolume mode, where
            // `open_write` (or `rewrite_multivolume`) has staged the set.
            _ => {
                return Err(RarError::Format(
                    "internal error: volume created without a staged volume set".into(),
                ));
            }
        };
        // The volume is staged under the temporary base and moved over its
        // final name on close.
        let tmp_vol = volume_path(&parent, &tmp_base, self.write_ctx().current_volume);
        let final_vol = volume_path(&parent, &final_base, self.write_ctx().current_volume);
        self.volume_paths.push(final_vol);
        let f = read_write_create(&tmp_vol)?;
        self.stream = Some(Box::new(f));
        self.write_signature()?;
        // Header-encrypted multi-volume sets repeat the plaintext encryption
        // header on every volume (WinRAR convention); the archive params are
        // generated once and shared across volumes.
        self.write_archive_encryption_header_if_needed()?;
        // Volume number: part2 → 1, part3 → 2, etc.
        let vol_num = (self.write_ctx().current_volume - 1) as u64;
        self.write_archive_header_vol(Some(vol_num))?;
        self.write_ctx_mut().volume_bytes_written =
            self.stream.as_mut().unwrap().stream_position()?;
        Ok(())
    }
}
