//! Inherent methods the [`Engine`] impl forwards to.
//!
//! `archive/ctx.rs` wires the family-facing [`Engine`](crate::engine::Engine)
//! trait to [`RarArchive`]. Seven of its methods are thin forwards to an
//! inherent method (`handle_archive_encrypt_header`, `on_disk_header_len`,
//! `write_block_header`, `start_next_volume`, `start_next_volume_rar13`,
//! `effective_threads`, `reset_catalog_token`), and the engine's own code
//! calls a handful more (`is_rar4`/`is_rar13`/`is_legacy`, `check_cancel`,
//! `ensure_write_ctx`, `report_progress`). Those live here, next to each
//! other, instead of being scattered between `mod.rs` (struct, lifecycle,
//! constructors) and `create.rs` (write finalization).
//!
//! Nothing else moved: the constructors, the open/append/lock lifecycle and
//! [`Drop`](super::RarArchive) stay in `mod.rs`, and the write finalization
//! (quick-open / recovery records / end block, volume staging) stays in
//! `create.rs`.

use std::io::Write;

use crate::crypto;
use crate::crypto::ENCR_IV_SIZE;
use crate::detect::ArchiveFamily;
use crate::engine::WriteState;
use crate::error::{RarError, RarResult};
use crate::fs::atomic::read_write_create;
use crate::fs::volume::{volume_path, volume_path_rar4};

use super::{PendingCommit, RarArchive, cancel_requested};

impl RarArchive {
    /// Whether the container is the legacy RAR 1.5–4.x family.
    pub(crate) fn is_rar4(&self) -> bool {
        self.family == ArchiveFamily::Rar15To40
    }

    /// Whether the container is the RAR 1.3/1.4 family.
    pub(crate) fn is_rar13(&self) -> bool {
        self.family == ArchiveFamily::Rar13
    }

    /// Whether the container is any legacy family (RAR 1.3–4.x).
    pub(crate) fn is_legacy(&self) -> bool {
        self.is_rar4() || self.is_rar13()
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

    /// Effective compression worker count for this archive: the per-archive
    /// override when set, otherwise the process-global default.
    pub(crate) fn effective_threads(&self) -> usize {
        #[cfg(feature = "parallel")]
        {
            crate::parallel::compression_threads_for(self.write_ctx().compression.threads)
        }
        #[cfg(not(feature = "parallel"))]
        {
            1
        }
    }

    /// Check the cancellation flag; returns [`crate::RarError::Cancelled`]
    /// when the caller requested an abort.
    pub(crate) fn check_cancel(&self) -> RarResult<()> {
        if cancel_requested(self.cancel.as_deref()) {
            return Err(RarError::Cancelled);
        }
        Ok(())
    }

    /// Mint a fresh catalog identity. Called after every catalog rebuild so
    /// IDs minted from the previous catalog are rejected as stale.
    pub(crate) fn reset_catalog_token(&mut self) -> RarResult<()> {
        self.read_ctx_mut().catalog_token = super::reader::allocate_catalog_token()?;
        Ok(())
    }

    /// Ensure the write-side state exists. Read-mode mutation operations
    /// (`delete`, `rename`, `set_comment`, `add_recovery_record`) rewrite
    /// the archive and so need a write context even though the archive was
    /// opened for reading.
    pub(super) fn ensure_write_ctx(&mut self) {
        self.write.get_or_insert_with(WriteState::default);
    }

    /// Report `done` bytes of the current member (identified by
    /// `progress_member`) against `member_total` through the shared tracker.
    /// Safe to call from the single-threaded write paths.
    pub(crate) fn report_progress(&mut self, done: u64, member_total: u64) {
        if let Some(progress) = self.progress.clone() {
            let member = self.progress_member;
            progress
                .lock()
                .expect("progress lock")
                .report(member, done, member_total);
        }
    }
}
