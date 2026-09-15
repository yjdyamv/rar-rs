//! Write lifecycle: opening the write stream, the archive-header envelope,
//! finalization (quick-open / recovery records / end block) and volume
//! rolling. Methods on [RarArchive] in a sibling impl block (see
//! `crate::archive::mod` for the shared state).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::{
    Mode, PendingCommit, RarArchive, volume_base_of, volume_path, volume_path_padded,
    volume_path_rar4,
};
use crate::crypto;
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::EndOfArchiveHeader;
use crate::format::rar5::vint;
use crate::format::rar5::{
    ARCHIVE_FLAG_RECOVERY, ARCHIVE_FLAG_SOLID, ARCHIVE_FLAG_VOLUME, ENCR_IV_SIZE,
    ENCR_PBKDF2_ITER_LOG, END_FLAG_NEXT_VOLUME, RAR5_SIGNATURE,
};
use crate::fs::atomic::{
    commit_files, install_durable, parent_dir, read_write_create, recover_interrupted_commit,
    temp_sibling_path, temp_suffix,
};

impl RarArchive {
    // ── Lifecycle ──────────────────────────────────────────────────────────

    pub(super) fn open_write(&mut self) -> RarResult<()> {
        // Finish or roll back a multi-volume commit that a previous process
        // was killed in the middle of, before staging anything new.
        let parent = parent_dir(&self.path);
        recover_interrupted_commit(&parent, &volume_base_of(&self.path))?;
        if self.is_rar13() {
            return self.open_write_rar13();
        }
        if self.is_rar4() {
            return self.open_write_rar4();
        }
        if let Some(volume_size) = self.write_ctx().output.volume_size {
            if volume_size == 0 {
                return Err(RarError::Format(
                    "volume size must be greater than zero".into(),
                ));
            }
            let base = volume_base_of(&self.path);
            let parent = parent_dir(&self.path);
            // Stage the volumes under a temporary volume base; they are
            // moved over the final `{base}.partN.rar` names on close.
            let tmp_base = format!(".{base}.rar5tmp-{}", temp_suffix());
            self.volume_paths = vec![volume_path(&parent, &base, 1)];
            self.write_ctx_mut().output.current_volume = 1;
            self.write_ctx_mut().output.pending = Some(PendingCommit::Volumes {
                parent: parent.clone(),
                tmp_base: tmp_base.clone(),
                final_base: base,
            });
            let f = read_write_create(&volume_path(&parent, &tmp_base, 1))?;
            self.stream = Some(Box::new(f));
            self.write_signature()?;
            self.write_archive_encryption_header_if_needed()?;
            self.write_archive_header_vol(None)?;
            self.write_ctx_mut().output.bytes_written =
                self.stream.as_mut().unwrap().stream_position()?;
            return Ok(());
        }

        // Stage the archive under a temporary sibling name; it is moved
        // over the final path on close, so a failed or interrupted
        // creation never leaves a partial archive at the target path.
        self.volume_paths = vec![self.path.clone()];
        let tmp_path = temp_sibling_path(&self.path);
        self.write_ctx_mut().output.pending = Some(PendingCommit::Single(tmp_path.clone()));
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
            let (encr, keys) =
                crypto::EncryptionParams::generate_with_keys(password, ENCR_PBKDF2_ITER_LOG);
            self.archive_encr = Some(encr);
            self.archive_keys = Some(keys);
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
        let keys = params
            .derive_and_verify(password)?
            .ok_or(RarError::WrongPassword)?;
        self.archive_encr = Some(params);
        self.archive_keys = Some(keys);
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
        if self.archive_encr.is_some() {
            let key = self.archive_header_key()?;
            let mut iv = [0u8; ENCR_IV_SIZE];
            rand::fill(&mut iv);
            let ciphertext = crypto::encrypt_data(header_bytes, &key, &iv);
            let stream = self.stream.as_mut().unwrap();
            stream.write_all(&iv)?;
            stream.write_all(&ciphertext)?;
        } else {
            self.stream.as_mut().unwrap().write_all(header_bytes)?;
        }
        Ok(())
    }

    /// Finalize the archive (writes end-of-archive block in write mode).
    pub(crate) fn close(&mut self) -> RarResult<()> {
        self.check_cancel()?;
        // Finalization runs exactly once. A failure can leave the stream
        // positioned mid-header and may already have written service
        // records; re-running it (a second `close`, or `Drop`'s automatic
        // retry) would append a second quick-open/recovery record or patch
        // the wrong offset. The flag is set before the first trailing write,
        // so neither path can re-enter it.
        if self.finalize_started {
            return Err(RarError::InvalidState(
                "archive finalization was already attempted".into(),
            ));
        }
        self.finish_writing()?;
        self.stream = None;
        // Move the staged files over their final paths: only now does the
        // archive become visible at the target path. Recovery volumes are
        // generated from the staged set and committed in the same
        // transaction, so a committed volume set always has its `.rev`
        // siblings (a failure before the commit leaves the previous set
        // untouched). On failure the staged files are left for [`Drop`] to
        // clean up.
        self.commit_pending()?;
        Ok(())
    }

    /// Write the trailing service records (quick-open, recovery) and the
    /// end-of-archive block. The stream is left open so a caller can take
    /// it back afterwards (in-memory sink seam).
    pub(super) fn finish_writing(&mut self) -> RarResult<()> {
        if self.finalize_started {
            return Err(RarError::InvalidState(
                "archive finalization was already attempted".into(),
            ));
        }
        self.finalize_started = true;
        if self.is_rar13() {
            return self.finish_writing_rar13();
        }
        if self.is_rar4() {
            return self.finish_writing_rar4();
        }
        if self.stream.is_some() && (self.mode == Mode::Write || self.mode == Mode::Append) {
            let qo_offset = if self.write_ctx().locator.quick_open {
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
            } else if self.write_ctx().locator.quick_open {
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

    /// Generate the `.rev` recovery-volume files for a multi-volume archive
    /// set (WinRAR `-rv` equivalent), reading the staged volume files that
    /// are about to be committed. Returns the staged `.rev` paths so the
    /// caller can install them in the same atomic commit as the data
    /// volumes; if the generation fails, no data volume is committed.
    ///
    /// WinRAR silently skips recovery volumes when `-v` produced a single
    /// volume (the data fit), and so do we.
    fn stage_recovery_volumes(
        &self,
        parent: &Path,
        tmp_base: &str,
        nd: usize,
    ) -> RarResult<Vec<PathBuf>> {
        if nd < 2 {
            return Ok(Vec::new());
        }
        // Exact count wins; the percent variant is converted at close time.
        let rec_count = if let Some(count) = self.recovery_volumes_count {
            (count as usize).min(nd)
        } else if let Some(percent) = self.recovery_volumes_percent {
            crate::recovery::rev50::plan_recovery_volume_count(nd, percent as u64)?
        } else {
            return Ok(Vec::new());
        };
        let staged: Vec<PathBuf> = (1..=nd).map(|n| volume_path(parent, tmp_base, n)).collect();
        crate::recovery::rev50::build_recovery_volumes_for_set(&staged, rec_count)
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
        match &self.write_ctx().output.pending {
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
        let Some(pending) = self.write.as_mut().and_then(|w| w.output.pending.take()) else {
            return Ok(());
        };
        let result = match &pending {
            PendingCommit::Single(tmp) => install_durable(tmp, &self.path),
            PendingCommit::Volumes {
                parent,
                tmp_base,
                final_base,
            } => {
                // WinRAR zero-pads the part number to the digit count of
                // the total volume count (part01..part15 for 10+ volumes);
                // the final names carry the same padding.
                let nd = self.volume_paths.len();
                let width = nd.to_string().len().max(1);
                let mut data_paths = Vec::with_capacity(nd);
                let mut install = Vec::with_capacity(nd);
                for n in 1..=nd {
                    let tmp = volume_path(parent, tmp_base, n);
                    // RAR4/RAR13 volume sets use the legacy `.rar`/`.rNN`
                    // naming; RAR5 uses the zero-padded `.partN.rar` naming.
                    let final_path = if self.is_legacy() {
                        volume_path_rar4(parent, final_base, n)
                    } else {
                        volume_path_padded(parent, final_base, n, width)
                    };
                    data_paths.push(final_path.clone());
                    install.push((tmp, final_path));
                }
                // Recovery volumes are generated from the staged data
                // volumes and installed by the same journaled commit, so a
                // committed set always carries matching `.rev` files and a
                // failure before the commit leaves the previous set intact.
                let revs = match self.stage_recovery_volumes(parent, tmp_base, nd) {
                    Ok(revs) => revs,
                    Err(error) => {
                        // The builder writes the `.rev` files one by one;
                        // drop the ones it managed to write before failing
                        // so no partial parity set is left next to the
                        // staged volumes.
                        Self::remove_staged_recovery_files(parent, tmp_base);
                        self.write_ctx_mut().output.pending = Some(pending);
                        return Err(error);
                    }
                };
                let rev_install = match recovery_install_paths(parent, final_base, &revs, width) {
                    Ok(install) => install,
                    Err(error) => {
                        Self::remove_staged_recovery_files(parent, tmp_base);
                        self.write_ctx_mut().output.pending = Some(pending);
                        return Err(error);
                    }
                };
                install.extend(rev_install);
                // A shorter overwrite must not leave parts of the previous,
                // longer set behind: retire every existing volume the new
                // set does not replace. The commit below parks them with the
                // replaced originals and restores them if it rolls back.
                // The new `.rev` files are in `keep`, so a stale-set scan
                // never retires the parity generated in this same commit.
                let keep: Vec<PathBuf> = install.iter().map(|(_, f)| f.clone()).collect();
                let retire = crate::fs::volume::stale_volume_paths(
                    parent,
                    final_base,
                    self.is_legacy(),
                    &keep,
                );
                let result = commit_files(parent, final_base, &install, &retire);
                if result.is_ok() {
                    self.volume_paths = data_paths;
                    // The .rev generation is one-shot: never re-run it on a
                    // later close (the data volumes are already installed).
                    self.recovery_volumes_percent = None;
                    self.recovery_volumes_count = None;
                }
                result
            }
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) => {
                // Keep the pending state so the drop guard removes any
                // staged files that were not committed.
                self.write_ctx_mut().output.pending = Some(pending);
                Err(e)
            }
        }
    }

    /// Remove the staged `.rev` files the recovery-volume builder may have
    /// written before failing. Called with the staged volume base; the
    /// builder names every file after it, so the sweep cannot match
    /// anything else.
    pub(super) fn remove_staged_recovery_files(parent: &Path, tmp_base: &str) {
        let Ok(entries) = std::fs::read_dir(parent) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(tmp_base) && name.ends_with(".rev") {
                let _ = std::fs::remove_file(entry.path());
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
        // Stream the recovery parity from disk instead of buffering the entire
        // prefix in memory, so arbitrarily large archives can carry a recovery
        // record without a multi-gigabyte RAM buffer.
        let rr_data = {
            let reader = std::fs::File::open(prefix_path)?;
            let mut limited = reader.take(archive_size);
            crate::recovery::rar50::build_structural_inline_recovery_data_streaming(
                &mut limited,
                archive_size,
                percent,
                None,
                1,
            )
            .map_err(|e| RarError::Format(format!("recovery record encode: {e}")))?
        };

        // RR service header: type 3, name "RR", SubData = percent byte.
        let subdata = {
            let rec = vec![percent as u8]; // recovery percent (single byte, <= 100)
            let mut extra = Vec::new();
            extra.extend(vint::encode((1 + rec.len()) as u64)); // record size: type + data
            extra.extend(vint::encode(0x07u64)); // service data record type
            extra.extend(rec);
            extra
        };
        let hdr = crate::format::rar5::headers::build_service_block(
            "RR",
            &subdata,
            rr_data.len() as u64,
            crate::format::rar5::BLOCK_FLAG_SKIP_IF_UNKNOWN,
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
        for (offset, header) in &self.write_ctx().locator.quick_open_entries {
            let rel = qo_pos.checked_sub(*offset).ok_or_else(|| {
                RarError::Format("quick-open cached header is after the QO record".into())
            })?;
            payload.extend(crate::format::rar5::headers::quick_open::encode_entry(
                rel, header,
            ));
        }

        // Service header: type 3, name "QO", with an empty service-data
        // extra record (type 0x07) and the payload as its data area.
        let subdata = {
            let mut extra = Vec::new();
            extra.extend(vint::encode(1u64)); // record size: type only
            extra.extend(vint::encode(0x07u64)); // service data record type
            extra
        };
        let hdr = crate::format::rar5::headers::build_service_block(
            "QO",
            &subdata,
            payload.len() as u64,
            crate::format::rar5::BLOCK_FLAG_SKIP_IF_UNKNOWN,
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
            .locator
            .main_header_start
            .ok_or_else(|| RarError::Format("main header position unknown".into()))?;

        // Rebuild the main header: read it back from the stream (plaintext
        // or decrypted), so the patch also works for in-memory sinks. The
        // header key is derived once (cached) and reused for the read-back
        // and the rewrite.
        let header_key = if self.header_encryption {
            Some(self.archive_header_key()?)
        } else {
            None
        };
        let plain = if let Some(key) = header_key.as_ref() {
            let stream = self.stream.as_mut().unwrap();
            let mut iv = [0u8; 16];
            stream.seek(SeekFrom::Start(start))?;
            stream.read_exact(&mut iv)?;
            // Decrypt the first block to learn the header size.
            let mut first = [0u8; 16];
            stream.read_exact(&mut first)?;
            let first_pt = crypto::decrypt_data(&first, key, &iv)?;
            let (hsize, vint_len) = vint::decode_from_slice(&first_pt, 4)
                .map_err(|e| RarError::Format(format!("main header vint: {e}")))?;
            let total_raw = 4 + vint_len + hsize as usize;
            let enc_size = total_raw.div_ceil(16) * 16;
            let mut full_ct = vec![0u8; enc_size];
            full_ct[..16].copy_from_slice(&first);
            if enc_size > 16 {
                stream.read_exact(&mut full_ct[16..])?;
            }
            let full_pt = crypto::decrypt_data(&full_ct, key, &iv)?;
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
        crate::format::rar5::headers::locator::patch_locator_fields(
            &mut new_header,
            qo_offset,
            rr_offset,
            self.write_ctx()
                .locator
                .qo_offset_field_pos
                .map(|p| p as usize),
            self.write_ctx()
                .locator
                .rr_offset_field_pos
                .map(|p| p as usize),
            base,
        )?;

        let stream = self.stream.as_mut().unwrap();
        if let Some(key) = header_key.as_ref() {
            let mut iv = [0u8; 16];
            rand::fill(&mut iv);
            let ciphertext = crypto::encrypt_data(&new_header, key, &iv);
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
        if self.recovery_percent.is_some() || self.write_ctx().locator.quick_open {
            return self.write_archive_header_with_locators();
        }
        let flags = if self.write_ctx().solid.mode {
            ARCHIVE_FLAG_SOLID
        } else {
            0
        };
        let (hdr, _, _) = crate::format::rar5::headers::locator::build_main_header(
            flags,
            &[],
            false,
            false,
            None,
        );
        self.write_block_header(&hdr)
    }

    /// Write the main archive header with a locator record for the
    /// quick-open and/or recovery-record offsets, plus the archive flags
    /// (`MHFL_RECOVERY`, `MHFL_SOLID`) as needed.
    ///
    /// The offset fields are preallocated as fixed 5-byte vints so the
    /// header length never changes; the real offsets are patched in at
    /// close time.
    pub(super) fn write_archive_header_with_locators(&mut self) -> RarResult<()> {
        let quick_open = self.write_ctx().locator.quick_open;
        let recovery = self.recovery_percent.is_some();
        let mut arch_flags = 0u64;
        if recovery {
            arch_flags |= ARCHIVE_FLAG_RECOVERY;
        }
        if self.write_ctx().solid.mode {
            arch_flags |= ARCHIVE_FLAG_SOLID;
        }
        let (hdr, qo_field, rr_field) = crate::format::rar5::headers::locator::build_main_header(
            arch_flags,
            &[],
            quick_open,
            recovery,
            None,
        );

        let main_header_start = self.stream.as_mut().unwrap().stream_position()?;
        self.write_block_header(&hdr)?;
        let ctx = self.write_ctx_mut();
        ctx.locator.main_header_start = Some(main_header_start);
        ctx.locator.qo_offset_field_pos = qo_field.map(|p| p as u64);
        ctx.locator.rr_offset_field_pos = rr_field.map(|p| p as u64);
        Ok(())
    }

    pub(super) fn write_archive_header_vol(&mut self, volume_number: Option<u64>) -> RarResult<()> {
        let (hdr, _, _) = crate::format::rar5::headers::locator::build_main_header(
            ARCHIVE_FLAG_VOLUME,
            &[],
            false,
            false,
            volume_number,
        );
        self.write_block_header(&hdr)
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
        if self.is_rar4() {
            return self.start_next_volume_rar4();
        }
        // WinRAR `-sv`: always reset the solid statistics at the start of a
        // new volume so each volume is an independent solid group.
        if self.write_ctx().solid.mode
            && self.write_ctx().solid.reset == crate::options::SolidReset::PerVolume
        {
            self.write_ctx_mut().solid.encoder_state = None;
            self.write_ctx_mut().solid.last_ext = None;
        }
        self.write_end_block_flags(true)?;
        // Close current volume
        self.stream = None;
        self.write_ctx_mut().output.current_volume += 1;
        let (parent, tmp_base, final_base) = match &self.write_ctx().output.pending {
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
        let tmp_vol = volume_path(&parent, &tmp_base, self.write_ctx().output.current_volume);
        let final_vol = volume_path(&parent, &final_base, self.write_ctx().output.current_volume);
        self.volume_paths.push(final_vol);
        let f = read_write_create(&tmp_vol)?;
        self.stream = Some(Box::new(f));
        self.write_signature()?;
        // Header-encrypted multi-volume sets repeat the plaintext encryption
        // header on every volume (WinRAR convention); the archive params are
        // generated once and shared across volumes.
        self.write_archive_encryption_header_if_needed()?;
        // Volume number: part2 → 1, part3 → 2, etc.
        let vol_num = (self.write_ctx().output.current_volume - 1) as u64;
        self.write_archive_header_vol(Some(vol_num))?;
        self.write_ctx_mut().output.bytes_written =
            self.stream.as_mut().unwrap().stream_position()?;
        Ok(())
    }

    // ── RAR4 write path ──────────────────────────────────────────────────

    // ── RAR 1.3/1.4 write path ───────────────────────────────────────────

    /// Stage a RAR 1.3/1.4 archive: the signature is written immediately,
    /// the main header is deferred until the archive comment is known (first
    /// member or close). Multi-volume sets stage under a temporary base and
    /// are installed as the legacy `.rar`/`.rNN` names on close.
    fn open_write_rar13(&mut self) -> RarResult<()> {
        if let Some(volume_size) = self.write_ctx().output.volume_size {
            if volume_size == 0 {
                return Err(RarError::Format(
                    "volume size must be greater than zero".into(),
                ));
            }
            let base = volume_base_of(&self.path);
            let parent = parent_dir(&self.path);
            let tmp_base = format!(".{base}.rar13tmp-{}", temp_suffix());
            self.volume_paths = vec![volume_path_rar4(&parent, &base, 1)];
            self.write_ctx_mut().output.current_volume = 1;
            self.write_ctx_mut().output.pending = Some(PendingCommit::Volumes {
                parent: parent.clone(),
                tmp_base: tmp_base.clone(),
                final_base: base,
            });
            let f = read_write_create(&volume_path(&parent, &tmp_base, 1))?;
            self.stream = Some(Box::new(f));
            let ctx = self.write_ctx_mut();
            ctx.output.rar13_header_pending = true;
            ctx.output.bytes_written = 0;
            return Ok(());
        }

        self.volume_paths = vec![self.path.clone()];
        let tmp_path = temp_sibling_path(&self.path);
        self.write_ctx_mut().output.pending = Some(PendingCommit::Single(tmp_path.clone()));
        let f = read_write_create(&tmp_path)?;
        self.stream = Some(Box::new(f));
        // The main header (signature included) is deferred until the archive
        // comment is known: first member or close.
        let ctx = self.write_ctx_mut();
        ctx.output.rar13_header_pending = true;
        ctx.output.bytes_written = 0;
        Ok(())
    }

    /// Roll a RAR 1.3/1.4 volume set: every volume starts with the signature
    /// and a plaintext main header carrying `MHD_VOLUME` (only the first
    /// volume holds the archive comment extension).
    pub(crate) fn start_next_volume_rar13(&mut self) -> RarResult<()> {
        if self.write_ctx().output.current_volume >= crate::fs::volume::LEGACY_VOLUME_MAX {
            return Err(RarError::InvalidOption(format!(
                "volume set exceeds the {}-volume legacy `.rNN` naming limit",
                crate::fs::volume::LEGACY_VOLUME_MAX
            )));
        }
        self.stream = None;
        self.write_ctx_mut().output.current_volume += 1;
        let (parent, tmp_base, final_base) = match &self.write_ctx().output.pending {
            Some(PendingCommit::Volumes {
                parent,
                tmp_base,
                final_base,
            }) => (parent.clone(), tmp_base.clone(), final_base.clone()),
            _ => {
                return Err(RarError::Format(
                    "internal error: volume created without a staged volume set".into(),
                ));
            }
        };
        let tmp_vol = volume_path(&parent, &tmp_base, self.write_ctx().output.current_volume);
        let final_vol =
            volume_path_rar4(&parent, &final_base, self.write_ctx().output.current_volume);
        self.volume_paths.push(final_vol);
        let f = read_write_create(&tmp_vol)?;
        self.stream = Some(Box::new(f));
        let header = crate::format::rar13::write::build_main_header(
            self.write_ctx().solid.mode,
            None,
            true,
        )?;
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(&header)?;
        self.write_ctx_mut().output.bytes_written = header.len() as u64;
        Ok(())
    }

    fn finish_writing_rar13(&mut self) -> RarResult<()> {
        if self.stream.is_some() && (self.mode == Mode::Write || self.mode == Mode::Append) {
            self.emit_rar13_main_header()?;
            self.mode = Mode::Read; // prevent double-write
        }
        Ok(())
    }

    fn open_write_rar4(&mut self) -> RarResult<()> {
        if let Some(volume_size) = self.write_ctx().output.volume_size {
            if volume_size == 0 {
                return Err(RarError::Format(
                    "volume size must be greater than zero".into(),
                ));
            }
            let base = volume_base_of(&self.path);
            let parent = parent_dir(&self.path);
            let tmp_base = format!(".{base}.rar4tmp-{}", temp_suffix());
            self.volume_paths = vec![volume_path_rar4(&parent, &base, 1)];
            self.write_ctx_mut().output.current_volume = 1;
            self.write_ctx_mut().output.pending = Some(PendingCommit::Volumes {
                parent: parent.clone(),
                tmp_base: tmp_base.clone(),
                final_base: base,
            });
            let f = read_write_create(&volume_path(&parent, &tmp_base, 1))?;
            self.stream = Some(Box::new(f));
            self.write_rar4_signature()?;
            self.write_rar4_main_header()?;
            self.write_ctx_mut().output.bytes_written =
                self.stream.as_mut().unwrap().stream_position()?;
            return Ok(());
        }

        self.volume_paths = vec![self.path.clone()];
        let tmp_path = temp_sibling_path(&self.path);
        self.write_ctx_mut().output.pending = Some(PendingCommit::Single(tmp_path.clone()));
        let f = read_write_create(&tmp_path)?;
        self.stream = Some(Box::new(f));
        self.write_rar4_signature()?;
        self.write_rar4_main_header()?;
        Ok(())
    }

    fn write_rar4_signature(&mut self) -> RarResult<()> {
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(crate::format::rar4::write::RAR4_SIGNATURE)?;
        Ok(())
    }

    fn write_rar4_main_header(&mut self) -> RarResult<()> {
        use crate::format::rar4::{MHD_FIRSTVOLUME, MHD_SOLID, MHD_VOLUME};
        let is_solid = self.write_ctx().solid.mode;
        let is_multivolume = self.write_ctx().output.volume_size.is_some();
        let mut flags: u16 = 0;
        if is_solid {
            flags |= MHD_SOLID;
        }
        if is_multivolume {
            flags |= MHD_VOLUME;
            // The first volume of a RAR4 set flags MHD_FIRSTVOLUME alongside
            // MHD_VOLUME (matches WinRAR's convention).
            if self.write_ctx().output.current_volume == 1 {
                flags |= MHD_FIRSTVOLUME;
            }
        }
        if self.header_encryption {
            // `-hp`: the main header is written in plaintext as the marker,
            // and every block after it is header-encrypted (RAR4 convention,
            // matching `scan_volume`).
            flags |= crate::format::rar4::MHD_PASSWORD;
        }
        if self.recovery_percent.is_some() {
            // MHD_RECOVERY: the archive carries a recovery record (the
            // NEWSUB `RR` block written at close). WinRAR's repair looks
            // for this bit before scanning for the record.
            flags |= crate::format::rar4::MHD_RECOVERY;
        }
        let buf = crate::format::rar4::write::build_main_header(flags);
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(&buf)?;
        Ok(())
    }

    fn finish_writing_rar4(&mut self) -> RarResult<()> {
        if self.write.as_ref().is_some_and(|w| w.rar4.solid_append) {
            // Deferred solid append: close repacks the whole archive
            // (surviving members re-encoded in chain order + the buffered
            // additions), preserving the original comment and rebuilding
            // the recovery record. The original file is replaced atomically
            // inside the repack.
            let additions = std::mem::take(&mut self.write_ctx_mut().rar4.solid_append_entries);
            self.write_ctx_mut().rar4.solid_append = false;
            let none_deleted = vec![false; self.entries.len()];
            crate::archive::rar4_edit::repack_solid_archive(
                self,
                &none_deleted,
                &std::collections::HashMap::new(),
                None,
                None,
                0,
                &additions,
                &[],
            )?;
            self.mode = Mode::Read;
            return Ok(());
        }
        if self.stream.is_some() && (self.mode == Mode::Write || self.mode == Mode::Append) {
            // `-rr`: the legacy NEWSUB (0x7a) recovery record goes between
            // the last member and the end-of-archive block (single-volume
            // only, matching WinRAR's RAR4 writer). Appending to an
            // archive that carried a record rebuilds it over the whole new
            // prefix at its original parity strength.
            if self.recovery_percent.is_some() || self.write_ctx().rar4.rr_sectors.is_some() {
                self.write_rar4_recovery_block()?;
            }
            self.write_rar4_end_block()?;
            self.mode = Mode::Read;
        }
        Ok(())
    }

    /// Build and append the RAR 3.x/4.x NEWSUB recovery block protecting
    /// everything written so far. Reads the prefix back through the staged
    /// file (the write stream is positioned at its end); the sector grid is
    /// anchored at the archive start, so the recovery block itself is the
    /// only thing left outside the protected range. The parity-sector count
    /// comes from the `-rr` percent, or — when appending to an archive that
    /// had a record — from the original record's strength.
    fn write_rar4_recovery_block(&mut self) -> RarResult<()> {
        let path = self.write_file_path().to_path_buf();
        let prefix_len = {
            let stream = self.stream.as_mut().unwrap();
            stream.stream_position()? as usize
        };
        let mut prefix = vec![0u8; prefix_len];
        {
            let mut reader = std::fs::File::open(&path)?;
            std::io::Read::read_exact(&mut reader, &mut prefix)?;
        }
        let rec_sectors = match self.write_ctx().rar4.rr_sectors {
            Some(rec) => rec,
            None => {
                let percent = self.recovery_percent.unwrap_or(0);
                crate::recovery::legacy_rr::recovery_sector_count(prefix_len, percent)
            }
        };
        let block = crate::recovery::legacy_rr::build_legacy_recovery_block(&prefix, rec_sectors)?;
        let stream = self.stream.as_mut().unwrap();
        if self.header_encryption {
            // `-hp`: only the 54-byte NEWSUB header is encrypted (the same
            // rule as FILE members); the tag table and parity sectors follow
            // as plaintext data, so readers advance past the block with the
            // decrypted head_size and the parity stays recoverable.
            let password = self.password.as_deref().ok_or_else(|| {
                RarError::Encrypted("header encryption requires a password".into())
            })?;
            let (ciphertext, on_disk) =
                crate::format::rar4::write::encrypt_block_header(&block[..54], password)?;
            stream.write_all(&ciphertext)?;
            stream.write_all(&block[54..])?;
            self.write_ctx_mut().output.bytes_written += on_disk + (block.len() - 54) as u64;
        } else {
            stream.write_all(&block)?;
            self.write_ctx_mut().output.bytes_written += block.len() as u64;
        }
        Ok(())
    }

    fn write_rar4_end_block(&mut self) -> RarResult<()> {
        let buf = crate::format::rar4::write::build_endarc(0);
        let stream = self.stream.as_mut().unwrap();
        if self.header_encryption {
            // `-hp`: the end-of-archive block is header-encrypted like every
            // other block after the main header.
            let password = self.password.as_deref().ok_or_else(|| {
                RarError::Encrypted("header encryption requires a password".into())
            })?;
            let (ciphertext, on_disk) =
                crate::format::rar4::write::encrypt_block_header(&buf, password)?;
            stream.write_all(&ciphertext)?;
            self.write_ctx_mut().output.bytes_written += on_disk;
        } else {
            stream.write_all(&buf)?;
            self.write_ctx_mut().output.bytes_written += buf.len() as u64;
        }
        Ok(())
    }

    fn start_next_volume_rar4(&mut self) -> RarResult<()> {
        if self.write_ctx().output.current_volume >= crate::fs::volume::LEGACY_VOLUME_MAX {
            return Err(RarError::InvalidOption(format!(
                "volume set exceeds the {}-volume legacy `.rNN` naming limit",
                crate::fs::volume::LEGACY_VOLUME_MAX
            )));
        }
        self.write_rar4_end_block()?;
        self.stream = None;
        self.write_ctx_mut().output.current_volume += 1;
        let (parent, tmp_base, final_base) = match &self.write_ctx().output.pending {
            Some(PendingCommit::Volumes {
                parent,
                tmp_base,
                final_base,
            }) => (parent.clone(), tmp_base.clone(), final_base.clone()),
            _ => {
                return Err(RarError::Format(
                    "internal error: volume created without a staged volume set".into(),
                ));
            }
        };
        let tmp_vol = volume_path(&parent, &tmp_base, self.write_ctx().output.current_volume);
        let final_vol =
            volume_path_rar4(&parent, &final_base, self.write_ctx().output.current_volume);
        self.volume_paths.push(final_vol);
        let f = read_write_create(&tmp_vol)?;
        self.stream = Some(Box::new(f));
        self.write_rar4_signature()?;
        self.write_rar4_main_header()?;
        self.write_ctx_mut().output.bytes_written =
            self.stream.as_mut().unwrap().stream_position()?;
        Ok(())
    }
}

/// Map the staged `.rev` files returned by the recovery-volume builder (named
/// after the staged volume base) to the canonical final names of the set.
///
/// The names are rebuilt from the generated files themselves: RAR5 uses the
/// zero-padded `<base>.partNN.rev` scheme (matching the data volumes),
/// while legacy (RAR 1.5–4.x) sets keep the rev3 name shape — including the
/// counts it embeds — for the final base.
fn recovery_install_paths(
    parent: &Path,
    final_base: &str,
    revs: &[PathBuf],
    part_width: usize,
) -> RarResult<Vec<(PathBuf, PathBuf)>> {
    if revs.is_empty() {
        return Ok(Vec::new());
    }
    let names =
        crate::recovery::rev3::canonical_recovery_names(final_base, revs, Some(part_width))?;
    Ok(revs
        .iter()
        .zip(names)
        .map(|(staged, name)| (staged.clone(), parent.join(name)))
        .collect())
}
