//! The interface the per-family container code is written against.
//!
//! `format::rar5` and `format::rar4` hold the family half of every engine
//! operation — the RAR5 add pipeline, the legacy write path, the extract
//! orchestrators. Those were `impl RarArchive` blocks, which forced `format`
//! to name `archive`'s type and made the two modules mutually dependent.
//!
//! This trait is the seam that removes it: a family function takes
//! `cx: &mut dyn Engine`, and [`RarArchive`](crate::archive::RarArchive)
//! implements it. The dependency then runs one way —
//! `archive` → `format` → `engine` — and the families can be read, and
//! eventually compiled, without the engine's type in view.
//!
//! The member set is deliberately close to what the blocks already used:
//! `ReadState` / `WriteState` were already reached through
//! `read_ctx()` / `write_ctx()` accessors, and the dozen services below are
//! the complete list of engine *behaviour* (as opposed to data) that the
//! family code calls.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::crypto;
use crate::detect::ArchiveFamily;
use crate::error::RarResult;
use crate::write_progress::ProgressTracker;

use super::{ArchiveEntry, ArchiveStream, Mode, ReadState, WriteState, cancel_requested};

/// Everything a family reader or writer needs from the engine.
///
/// Implemented by [`RarArchive`](crate::archive::RarArchive); passed to the
/// family functions as `&mut dyn Engine`.
//
// MIGRATION: the family blocks in `format` are converted one at a time, so
// part of this surface has no caller yet. Remove this `allow` together with
// the last `impl RarArchive` block in `format`.
#[allow(dead_code)]
pub(crate) trait Engine {
    // ── State ─────────────────────────────────────────────────────────────
    //
    // The bulk of the interface: the family code drives `ReadState` /
    // `WriteState` directly rather than through one accessor per field.

    /// Read-side state. Panics if the archive was not opened for reading.
    fn read_ctx(&self) -> &ReadState;
    /// Mutable read-side state. Panics if not opened for reading.
    fn read_ctx_mut(&mut self) -> &mut ReadState;
    /// Write-side state. Panics if not opened for writing.
    fn write_ctx(&self) -> &WriteState;
    /// Mutable write-side state. Panics if not opened for writing.
    fn write_ctx_mut(&mut self) -> &mut WriteState;
    /// Ensure the write-side state exists, for read-mode operations that
    /// rewrite the archive (`delete`, `rename`, `set_comment`, …).
    fn ensure_write_ctx(&mut self);

    // ── Container identity ────────────────────────────────────────────────

    /// The container family this archive belongs to.
    fn family(&self) -> ArchiveFamily;
    /// Note the detected family while opening.
    fn set_family(&mut self, family: ArchiveFamily);
    /// The archive is open for reading, writing or appending.
    fn mode(&self) -> Mode;

    /// Whether the container is the RAR 1.5–4.x family.
    fn is_rar4(&self) -> bool;
    /// Whether the container is the RAR 1.3/1.4 family.
    fn is_rar13(&self) -> bool;
    /// Whether the container is any legacy family (RAR 1.3–4.x).
    fn is_legacy(&self) -> bool {
        self.is_rar4() || self.is_rar13()
    }

    // ── Catalog ───────────────────────────────────────────────────────────

    /// The member catalog built so far, in archive order.
    fn entries(&self) -> &[ArchiveEntry];
    /// Mutable member catalog.
    fn entries_mut(&mut self) -> &mut Vec<ArchiveEntry>;
    /// Rotate the catalog identity token so previously issued [`EntryId`]s
    /// are rejected (see [`ReadState::catalog_token`]).
    ///
    /// [`EntryId`]: crate::EntryId
    fn reset_catalog_token(&mut self) -> RarResult<()>;

    // ── Archive stream ────────────────────────────────────────────────────

    /// The underlying stream, or an error when there is none.
    fn stream_mut(&mut self) -> RarResult<&mut Box<dyn ArchiveStream>>;
    /// Replace the underlying stream (used while opening).
    fn set_stream(&mut self, stream: Box<dyn ArchiveStream>);

    // ── Archive-level encryption ──────────────────────────────────────────

    /// The archive password, when one was supplied.
    fn password(&self) -> Option<&str>;
    /// Whether archive headers are encrypted (`-hp`).
    fn header_encryption(&self) -> bool;
    /// Parsed archive-level encryption parameters, when headers are encrypted.
    fn archive_encr(&self) -> Option<&crypto::EncryptionParams>;
    /// The derived header-encryption keys, when header encryption is active.
    fn archive_keys(&self) -> Option<&crypto::DerivedKeys>;
    /// Adopt the archive-level encryption header read (or written) by the
    /// family code, verifying the password.
    fn handle_archive_encrypt_header(&mut self, params: crypto::EncryptionParams) -> RarResult<()>;
    /// Length a plaintext header occupies on disk once header encryption is
    /// applied (IV block + padded ciphertext, or unchanged).
    fn on_disk_header_len(&self, plain_len: u64) -> u64;
    /// Write one header block, encrypting it first when header encryption is
    /// active.
    fn write_block_header(&mut self, header_bytes: &[u8]) -> RarResult<()>;

    // ── Volumes ───────────────────────────────────────────────────────────

    /// Path of the volume the archive was opened as.
    fn path(&self) -> &Path;
    /// Every volume of the set, in order.
    fn volume_paths(&self) -> &[PathBuf];
    /// Record the volume set found on disk (during opening).
    fn set_volume_paths(&mut self, paths: Vec<PathBuf>);
    /// Byte offset where the archive signature starts (0 for plain archives,
    /// >0 when an SFX stub precedes it).
    fn sfx_offset(&self) -> u64;
    /// Record where the signature was found (during opening).
    fn set_sfx_offset(&mut self, offset: u64);
    /// Whether the container is solid at archive level.
    fn archive_solid(&self) -> bool;
    /// Record the container-level solid flag (during opening).
    fn set_archive_solid(&mut self, solid: bool);
    /// Close the current volume and open the next one.
    fn start_next_volume(&mut self) -> RarResult<()>;
    /// Close the current volume and open the next `.rNN` legacy volume.
    fn start_next_volume_rar13(&mut self) -> RarResult<()>;

    // ── Parallelism, progress, cancellation ───────────────────────────────

    /// Compression worker count for this archive (`-mt`).
    fn effective_threads(&self) -> usize;
    /// The shared progress tracker and the index of the member currently
    /// being written, for callers that drive the tracker themselves (the
    /// parallel waves). `None` when no callback is installed.
    fn progress_slot(&self) -> Option<(Arc<Mutex<ProgressTracker>>, usize)>;
    /// Point progress reporting at member `index` of the current batch.
    fn set_progress_member(&mut self, index: usize);
    /// Report `done` of `member_total` bytes for the current member.
    fn report_progress(&mut self, done: u64, member_total: u64);
    /// The caller's cancellation flag, for passing to a spawned worker.
    fn cancel_token(&self) -> Option<Arc<AtomicBool>>;
    /// The caller's cancellation flag as a plain view.
    fn cancel_flag(&self) -> Option<&AtomicBool>;
    /// Fail with [`RarError::Cancelled`](crate::RarError::Cancelled) when the
    /// caller's flag has been raised.
    fn check_cancel(&self) -> RarResult<()> {
        if cancel_requested(self.cancel_flag()) {
            return Err(crate::error::RarError::Cancelled);
        }
        Ok(())
    }
}
