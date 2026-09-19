//! The interface the per-family container code is written against.
//!
//! `format::rar5` and `format::rar4` hold the family half of every engine
//! operation — the RAR5 add pipeline, the legacy write path, the extract
//! orchestrators. They used to be written as `impl RarArchive` blocks,
//! which forced `format` to name `archive`'s type and made the two modules
//! mutually dependent.
//!
//! The seam that removes it is a set of object-safe traits: every family
//! operation is a free function taking `cx: &mut dyn Engine` (or
//! `cx: &dyn Engine` when it only reads), and
//! [`RarArchive`](crate::archive::RarArchive) implements them in
//! `archive/ctx.rs`. The dependency runs one way —
//! `archive` → `format` → `engine` — and `format` no longer names `archive`
//! at all, which `tests/architecture_boundaries.rs` pins.
//!
//! The member set is deliberately close to what the blocks already used:
//! `ReadState` / `WriteState` were already reached through
//! `read_ctx()` / `write_ctx()` accessors, and the services below are the
//! complete list of engine *behaviour* (as opposed to data) that the family
//! code calls.
//!
//! The seam is grouped rather than monolithic. [`EngineState`] carries the
//! state block and the container identity, [`CatalogOps`] the member
//! catalog, [`StreamOps`] the underlying stream, [`HeaderCryptoOps`] the
//! archive-level header encryption, [`VolumeOps`] the volume set and its
//! byte accounting, and [`WriteServices`] the progress/cancellation hooks
//! plus the single-point write services. [`Engine`] is their umbrella:
//! family code takes `&mut dyn Engine`, so no call site has to name a group,
//! while the grouping records what each call is about and lets a narrower
//! context be spelled `&mut (dyn CatalogOps + VolumeOps)` where that is all
//! a function needs. `docs/ARCHITECTURE.md` describes the seam.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::crypto;
use crate::detect::ArchiveFamily;
use crate::error::RarResult;
use crate::write_progress::ProgressTracker;

use super::{ArchiveEntry, ArchiveStream, Mode, ReadState, WriteState, cancel_requested};

/// Disjoint borrows of the engine's state.
///
/// A family reader or writer routinely holds two pieces of the engine at
/// once — the catalog entry it is decoding and the stream it reads the
/// payload from — and `self.entries[idx]` plus `&mut self.stream` is a
/// disjoint *field* borrow, which the compiler accepts. Through a trait it
/// is two calls that each borrow the whole `&mut dyn Engine`, which it does
/// not.
///
/// [`EngineState::parts`] hands the fields out together so the family code
/// keeps the borrow structure it had. The engine implements it with plain
/// field borrows; only the fields the converted family code actually reads
/// are carried, and `format` never names `RarArchive`.
///
/// Services (`write_block_header`, `start_next_volume`, …) still take the
/// whole `&mut dyn Engine`, so a `Parts` borrow must end before one is
/// called.
pub(crate) struct Parts<'a> {
    /// Read-side state; `None` when the archive is not open for reading.
    pub read: &'a mut Option<ReadState>,
    /// The member catalog, in archive order.
    pub entries: &'a mut Vec<ArchiveEntry>,
    /// The underlying stream; `None` before one is opened.
    pub stream: &'a mut Option<Box<dyn ArchiveStream>>,
    /// Every volume of the set, in order.
    pub volume_paths: &'a [PathBuf],
    /// The archive password, when one was supplied.
    pub password: Option<&'a str>,
    /// The caller's cancellation flag, when one is installed.
    pub cancel: Option<&'a AtomicBool>,
}

impl Parts<'_> {
    /// Read-side state. Panics if the archive was not opened for reading.
    pub(crate) fn read_ctx(&self) -> &ReadState {
        self.read.as_ref().expect("read context not available")
    }
}

/// The engine's state block and the container identity.
///
/// The bulk of the seam: the family code drives `ReadState` / `WriteState`
/// directly rather than through one accessor per field, and asks the engine
/// which container it is looking at (the families differ in header layout
/// and in which operations are legal).
pub(crate) trait EngineState {
    /// Read-side state. Panics if the archive was not opened for reading.
    fn read_ctx(&self) -> &ReadState;
    /// Mutable read-side state. Panics if not opened for reading.
    fn read_ctx_mut(&mut self) -> &mut ReadState;
    /// Write-side state. Panics if not opened for writing.
    fn write_ctx(&self) -> &WriteState;
    /// Mutable write-side state. Panics if not opened for writing.
    fn write_ctx_mut(&mut self) -> &mut WriteState;
    /// Disjoint borrows of the state fields, for family code that needs two
    /// of them at once. See [`Parts`].
    fn parts(&mut self) -> Parts<'_>;

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
}

/// The member catalog: the ordered entry list and its identity token.
///
/// The writers may only append (`push_entry`) and the readers may only
/// publish a whole scan (`clear_catalog` / `replace_catalog`), so archive
/// order and the payload-offset identity behind
/// [`EntryId`](crate::EntryId) stay owned here.
pub(crate) trait CatalogOps {
    /// The member catalog built so far, in archive order.
    fn entries(&self) -> &[ArchiveEntry];
    /// Drop every catalog entry (a scan rebuilds it from scratch).
    fn clear_catalog(&mut self);
    /// Replace the whole catalog (a scan publishes its result at once).
    fn replace_catalog(&mut self, entries: Vec<ArchiveEntry>);
    /// Rotate the catalog identity token so previously issued [`EntryId`]s
    /// are rejected (see [`ReadState::catalog_token`]).
    ///
    /// [`EntryId`]: crate::EntryId
    fn reset_catalog_token(&mut self) -> RarResult<()>;
    /// Append one emitted member to the catalog.
    ///
    /// The single append point: the writer pipelines must not push into the
    /// catalog themselves, so "archive order" and the payload-offset identity
    /// used by [`EntryId`](crate::EntryId) have one owner.
    fn push_entry(&mut self, entry: ArchiveEntry);
}

/// The underlying archive stream.
pub(crate) trait StreamOps {
    /// The underlying stream, or an error when there is none.
    fn stream_mut(&mut self) -> RarResult<&mut Box<dyn ArchiveStream>>;
    /// Replace the underlying stream (used while opening).
    fn set_stream(&mut self, stream: Box<dyn ArchiveStream>);
}

/// Archive-level header encryption (`-hp`) and the headers written through
/// it.
pub(crate) trait HeaderCryptoOps {
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
}

/// The volume set: its identity on disk, the archive-level flags read with
/// it, the volume rolls, and the byte accounting the writers budget
/// against.
pub(crate) trait VolumeOps {
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
    /// Bytes already written to the current volume (the budget the family
    /// writers measure their next header/payload against).
    fn bytes_written(&self) -> u64;
    /// Account `bytes` written to the current volume.
    fn add_bytes_written(&mut self, bytes: u64);
    /// Zero-based index of the volume currently being written.
    fn current_volume_index(&self) -> usize;
}

/// The write-side services the family pipelines call: parallelism,
/// progress reporting, cancellation, and the operations whose invariant
/// would otherwise be re-implemented per family.
pub(crate) trait WriteServices {
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
    /// Record a member header in the quick-open locator, when quick-open is
    /// enabled, at the current stream position. No-op otherwise.
    fn record_quick_open_entry(&mut self, header_bytes: &[u8]) -> RarResult<()>;
    /// Seed the RAR5 solid-chain encoder state when absent and start a new
    /// member frame (`EncoderState::begin_member`). Returns whether the chain
    /// was already carry-over solid, which the member header records.
    fn begin_solid_member(&mut self) -> bool;
}

/// Everything a family reader or writer needs from the engine: the umbrella
/// of the six capability traits above.
///
/// Implemented by [`RarArchive`](crate::archive::RarArchive) through the
/// blanket impl below; passed to the family functions as `&mut dyn Engine`
/// (`&dyn Engine` when they only read). Nothing is declared here on purpose:
/// the methods belong to the group that explains them, and a receiver typed
/// `dyn Engine` still reaches every one of them.
pub(crate) trait Engine:
    EngineState + CatalogOps + StreamOps + HeaderCryptoOps + VolumeOps + WriteServices
{
}

impl<T> Engine for T where
    T: EngineState + CatalogOps + StreamOps + HeaderCryptoOps + VolumeOps + WriteServices + ?Sized
{
}
