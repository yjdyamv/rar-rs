//! The archive engine's shared vocabulary.
//!
//! This module sits *below* both [`crate::archive`] (which owns `RarArchive`,
//! the object the public facades drive) and [`crate::format`] (the per-family
//! container code). The per-family readers and writers have to name the state
//! an open archive carries ([`ReadState`], [`WriteState`]), the entry types
//! ([`ArchiveEntry`], [`BatchEntry`]) and a handful of limits — so those live
//! here rather than in `archive`, and `archive` implements the per-family
//! operations on top instead of the other way round.
//!
//! Nothing in here may depend on `archive` or `format`; the dependency runs
//! `archive` → `format` → `engine` → `{codec, crypto, fs, model, options}`.

use std::io::{Read, Seek, Write};

mod ctx;
mod discovery;
mod entry;
mod plan;
mod state;

pub(crate) use ctx::{Engine, Parts};
pub use discovery::discover_volumes;
pub use entry::{ArchiveEntry, BatchEntry};
#[cfg(feature = "parallel")]
pub(crate) use entry::{BatchPrepareCtx, PreparedEntry};
pub(crate) use plan::MemberPlan;
pub(crate) use state::{
    CompressionSettings, DecryptedPayload, LegacyDecoder, LegacySolidEncoder, LocatorState,
    MetadataSettings, Mode, OutputState, PendingCommit, Rar4Append, ReadState, SolidAppendEntry,
    SolidChain, StreamRecord, WriteState,
};

/// Maximum accepted RAR5 dictionary-size log (4 GiB, the RAR5 format
/// ceiling; WinRAR 7.23 accepts the same range — larger, non-power-of-two
/// dictionaries only exist in the RAR7 format, which is out of scope).
/// Larger values are rejected at decode time to bound window allocations.
pub(crate) const MAX_DICT_SIZE_LOG: u8 = 15;

/// Parallel batch compression (feature `parallel`): members up to this
/// size are compressed whole in Rayon waves; larger non-solid files are
/// compressed in parallel chunks with bounded memory.
#[cfg(feature = "parallel")]
pub(crate) const PARALLEL_COMPRESS_MAX_MEMBER: u64 = 64 * 1024 * 1024;
/// Members at least this large take the streaming compressed path in
/// `add_file_rar5`: input is compressed in bounded chunks into a
/// temporary spill file and then streamed into the archive, so memory
/// stays bounded for any file size (P4: >4 GiB single-file creation).
pub(crate) const STREAM_COMPRESS_THRESHOLD: u64 = 64 * 1024 * 1024;
/// Total input bytes buffered per parallel compression wave (feature
/// `parallel`).
#[cfg(feature = "parallel")]
pub(crate) const PARALLEL_COMPRESS_WAVE_BUDGET: u64 = 256 * 1024 * 1024;

/// A seekable read/write sink for archive streams: `File` in production,
/// `Cursor<Vec<u8>>` for in-memory archives (tests, future `-si` support).
pub trait ArchiveStream: Read + Write + Seek {}
impl<T: Read + Write + Seek> ArchiveStream for T {}

/// True when the caller-owned cancellation flag has been raised.
pub(crate) fn cancel_requested(cancel: Option<&std::sync::atomic::AtomicBool>) -> bool {
    cancel.is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
}
