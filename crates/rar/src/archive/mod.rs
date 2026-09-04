//! The [RarArchive] facade: shared archive state, entry-point constructors,
//! configuration setters and drop-time cleanup.
//!
//! This module is intentionally a thin seam. Read/decode lives in
//! [`crate::rar50::extract`], the write pipeline in [`crate::rar50::write`],
//! create/append finalization in `crate::archive::create`, and surgical
//! rewrite (delete/rename/comment) in `crate::archive::rewrite`.

mod create;
mod discover;
mod entry;
mod rewrite;

#[cfg(test)]
mod tests;

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::codec::DecoderState;
use crate::crypto;
use crate::crypto::parse_archive_encrypt_header;
use crate::error::{RarError, RarResult};
use crate::io_util::{copy_prefix, read_write_create, replace_file, temp_sibling_path};
use crate::rar50::headers::*;
use crate::rar50::vint;
use crate::rar50::*;
use crate::version::ArchiveVersion;
use crate::write_progress::ProgressTracker;

pub use discover::discover_volumes;
pub(crate) use discover::{
    volume_base_of, volume_part_width, volume_path, volume_path_padded, volume_path_rar4,
};
pub use entry::{ArchiveEntry, BatchEntry};
#[cfg(feature = "parallel")]
pub(crate) use entry::{BatchPrepareCtx, PreparedEntry};

/// Maximum archive prefix buffered for inline recovery-record parity.
/// Streamed recovery records are not implemented yet; larger archives must
/// create recovery records without `recovery_percent`.
pub(crate) const MAX_RECOVERY_PREFIX_BYTES: u64 = 32 * 1024 * 1024 * 1024;

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
/// [`RarArchive::add_file`]: input is compressed in bounded chunks into a
/// temporary spill file and then streamed into the archive, so memory
/// stays bounded for any file size (P4: >4 GiB single-file creation).
pub(crate) const STREAM_COMPRESS_THRESHOLD: u64 = 64 * 1024 * 1024;
/// Total input bytes buffered per parallel compression wave (feature
/// `parallel`).
#[cfg(feature = "parallel")]
pub(crate) const PARALLEL_COMPRESS_WAVE_BUDGET: u64 = 256 * 1024 * 1024;

/// Decrypted member payload plus the key material needed for integrity
/// verification.
pub(crate) struct DecryptedPayload {
    pub(crate) data: Vec<u8>,
    pub(crate) params: Option<crypto::EncryptionParams>,
    pub(crate) keys: Option<crypto::DerivedKeys>,
}

/// Read-side state for extraction and listing.
///
/// Groups fields exclusively used by read/extract paths (extract.rs).
/// Owned as `Option<ReadState>` inside [`RarArchive`]; `None` when the
/// archive is opened for writing only.
pub(crate) struct ReadState {
    /// Persistent decoder state for RAR5 solid archive chains.
    pub solid_state: Option<DecoderState>,
    /// Index of the last file decoded in the solid chain (-1 = none).
    pub solid_decoded_through: isize,
    /// Persistent legacy decoder for solid chains (RAR 1.5/2.x/3.x).
    pub rar4_decoder: Option<crate::rar40::LegacyDecoder>,
    /// Index of the last legacy-solid member decoded (-1 = none).
    pub rar4_decoded_through: isize,
    /// Options for the current read/extract operation (set per call).
    pub extract_options: crate::options::ExtractOptions,
    /// NTFS alternate data streams ("STM" service records) attached to
    /// members, in archive order.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub streams: Vec<StreamRecord>,
}

impl Default for ReadState {
    fn default() -> Self {
        Self {
            solid_state: None,
            solid_decoded_through: -1,
            rar4_decoder: None,
            rar4_decoded_through: -1,
            extract_options: crate::options::ExtractOptions::default(),
            streams: Vec::new(),
        }
    }
}

/// Write-side state for creation, append, and rewrite.
///
/// Groups fields exclusively used by write/create/append paths
/// (write/mod.rs + rewrite.rs). Owned as `Option<WriteState>` inside
/// [`RarArchive`]; `None` when the archive is opened for reading only.
pub(crate) struct WriteState {
    /// Create a solid archive (shared LZ window across compressed members).
    pub solid_mode: bool,
    /// How the solid chain is split (WinRAR `-s` modifiers `-sd`/`-sv`/`-se`).
    pub solid_reset: crate::options::SolidReset,
    /// File extension of the last member added to the solid chain; used by
    /// `SolidReset::PerExtension` to detect when to reset the statistics.
    pub last_solid_ext: Option<String>,
    /// Persistent RAR5 encoder state for solid archives.
    pub encoder_state: Option<crate::codec::EncoderState>,
    /// Persistent RAR4 LZSS encoder for solid archives; the sliding window
    /// and Huffman table state carry across the members of a solid run.
    pub rar4_solid_encoder: Option<crate::codec::rar29_encoder::Unpack29Encoder>,
    /// True once the current RAR4 solid run has emitted a member, so the next
    /// compressed member is flagged as a chain continuation (`FHD_SOLID`).
    pub rar4_solid_run_has_member: bool,
    /// Per-archive compression thread count (`-mt`); `None` = process-global
    /// default. The compression pool is selected per thread count, so
    /// concurrent archives with different values never interfere.
    pub compression_threads: Option<usize>,
    /// Requested dictionary log for compression (WinRAR `-md`);
    /// `None` = default selection.
    pub dict_size_log: Option<u8>,
    /// Requested dictionary size in bytes for RAR7 (v70) members
    /// (WinRAR `-md` above 4 GiB, any value > 4 GiB accepted).
    pub dict_size_bytes: Option<u64>,
    /// Force RAR7 (v70) member headers even below the 4 GiB threshold
    /// (test seam; see `CreateOptions::force_v70`).
    pub force_v70: bool,
    /// Save creation/change time in the FILE_TIME extra record (`-tsc`).
    pub save_ctime: bool,
    /// Save last access time in the FILE_TIME extra record (`-tsa`).
    pub save_atime: bool,
    /// Save the modification time (`-tsm`; false with `-tsm-`/`-ts-`).
    pub save_mtime: bool,
    /// Save owner/group on Unix (`-ow`).
    pub save_owner: bool,
    /// Save NTFS alternate data streams (`-os`; Windows only).
    pub save_streams: bool,
    /// Store timestamps at 1-second precision (`-ts...1`).
    pub time_precision_seconds: bool,
    /// Write BLAKE2sp hash records for members.
    pub blake2: bool,
    /// Write a quick-open ("QO") service record at close time.
    pub quick_open: bool,
    /// Cached (offset, full header bytes) of file headers for quick-open.
    pub quick_open_entries: Vec<(u64, Vec<u8>)>,
    /// File offset of the quick-open offset vint inside the main header's
    /// locator record (preallocated, patched at close time).
    pub qo_offset_field_pos: Option<u64>,
    /// File offset of the main archive header (for the recovery-record
    /// locator patch written at close time).
    pub main_header_start: Option<u64>,
    /// File offset of the recovery-record offset vint inside the main
    /// header's locator record (preallocated, patched at close time).
    pub rr_offset_field_pos: Option<u64>,
    /// Staged write target during an uncommitted create/append: the data
    /// goes to temporary sibling files and is moved over the final paths
    /// only after [`Self::close`] succeeds, so a failed or interrupted
    /// operation never leaves a partial archive at the final path.
    pub pending: Option<PendingCommit>,
    /// Volume size limit for multi-volume creation (None = single volume).
    pub volume_size: Option<u64>,
    /// Current volume number during creation (1-indexed).
    pub current_volume: usize,
    /// Bytes written in the current volume during creation.
    pub volume_bytes_written: u64,
}

impl Default for WriteState {
    fn default() -> Self {
        Self {
            solid_mode: false,
            solid_reset: crate::options::SolidReset::Continuous,
            last_solid_ext: None,
            encoder_state: None,
            rar4_solid_encoder: None,
            rar4_solid_run_has_member: false,
            compression_threads: None,
            dict_size_log: None,
            dict_size_bytes: None,
            force_v70: false,
            save_ctime: false,
            save_atime: false,
            save_mtime: true,
            save_owner: false,
            save_streams: false,
            time_precision_seconds: false,
            blake2: false,
            quick_open: false,
            quick_open_entries: Vec::new(),
            qo_offset_field_pos: None,
            main_header_start: None,
            rr_offset_field_pos: None,
            pending: None,
            volume_size: None,
            current_volume: 0,
            volume_bytes_written: 0,
        }
    }
}

/// RAR5 archive reader/writer.
pub struct RarArchive {
    pub(crate) path: PathBuf,
    pub(crate) mode: Mode,
    pub(crate) entries: Vec<ArchiveEntry>,
    /// The archive stream: a file, or any caller-provided seekable
    /// read/write sink (in-memory `Cursor` in tests, stdin/stdout for
    /// future `-si` support).
    pub(crate) stream: Option<Box<dyn ArchiveStream>>,
    /// Byte offset where the RAR5 signature begins (0 for plain archives,
    /// >0 for SFX archives whose stub precedes the archive).
    pub(crate) sfx_offset: u64,
    /// Whether the archive uses the legacy RAR 1.5–4.x container (vs RAR5).
    pub(crate) rar4: bool,
    /// Password for encrypted archives.
    pub(crate) password: Option<String>,
    /// Encrypt archive headers (file names/structure hidden) — RAR5
    /// archive-level encryption header + AES-256-CBC per-block headers.
    pub(crate) header_encryption: bool,
    /// Archive-level encryption parameters when header encryption is on.
    pub(crate) archive_encr: Option<crypto::EncryptionParams>,
    /// Recovery record: recovery percent (0-100) when the archive is created
    /// with an inline RAR5 recovery record ("RR" service header).
    pub(crate) recovery_percent: Option<u8>,
    /// Recovery volumes: percent (0-100) of `.rev` files created alongside
    /// a multi-volume archive (WinRAR `-rv`).
    pub(crate) recovery_volumes_percent: Option<u8>,
    /// Recovery volumes: exact `.rev` file count (auto-capped at the data
    /// volume count).
    pub(crate) recovery_volumes_count: Option<u32>,
    /// All volume file paths (multi-volume archives).
    pub(crate) volume_paths: Vec<PathBuf>,
    /// Optional progress callback invoked during compression. Since the
    /// `parallel` feature compresses waves of members concurrently, the
    /// tracker behind this field aggregates every member's deltas into one
    /// monotonic stream; the callback observes `(committed, total)` where
    /// `total` is the whole write operation's input byte count. See
    /// [`write_progress::ProgressTracker`].
    pub(crate) progress:
        Option<std::sync::Arc<std::sync::Mutex<crate::write_progress::ProgressTracker>>>,
    /// Index (into the current batch) of the member being written. Set by
    /// `add_batch` before each sequential member so its per-member progress
    /// is routed to the right tracker slot.
    pub(crate) progress_member: usize,
    /// Caller-owned cancellation flag: when set to true, long-running
    /// create/extract/repair operations abort at their next check point
    /// with [`crate::RarError::Cancelled`]. Installed with
    /// [`Self::set_cancel_flag`]; `None` = never cancelled.
    pub(crate) cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Read-side state (solid decode, extract options, NTFS streams).
    /// Present when the archive is opened for reading or appending.
    pub(crate) read: Option<ReadState>,
    /// Write-side state (solid mode, compression, quick-open, recovery).
    /// Present when the archive is opened for writing or appending.
    pub(crate) write: Option<WriteState>,
}

/// An NTFS alternate data stream ("STM" service record) attached to an
/// archive member: the member index, the stream name (with the leading
/// colon, e.g. `:Zone.Identifier`), the stream payload location and its
/// compression parameters (the payload may be RAR5-compressed).
///
/// Fields are only read back on Windows (extraction restores streams via
/// `file:name`); on other platforms they are parsed and stored but never
/// consumed, so dead-code linting is relaxed there.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone)]
pub(crate) struct StreamRecord {
    pub owner_index: usize,
    pub name: String,
    pub data_offset: u64,
    pub data_size: u64,
    pub unpacked_size: u64,
    pub method: u8,
    pub dict_size_log: u8,
}

/// A seekable read/write sink for archive streams: `File` in production,
/// `Cursor<Vec<u8>>` for in-memory archives (tests, future `-si` support).
pub trait ArchiveStream: Read + Write + Seek {}
impl<T: Read + Write + Seek> ArchiveStream for T {}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    Read,
    Write,
    Append,
}

/// Staged write target: new archive data is written to temporary sibling
/// files first and moved over the final paths on successful close.
pub(crate) enum PendingCommit {
    /// Single-volume write: the temporary file staged for the final path.
    Single(PathBuf),
    /// Multi-volume write: volumes are staged as `{tmp_base}.partN.rar`
    /// (in `parent`) and moved to `{final_base}.partN.rar` on close.
    Volumes {
        parent: PathBuf,
        tmp_base: String,
        final_base: String,
    },
}

impl PendingCommit {
    /// Remove staged files that were never committed.
    fn cleanup(&self, volume_count: usize) {
        match self {
            PendingCommit::Single(tmp) => {
                let _ = fs::remove_file(tmp);
            }
            PendingCommit::Volumes {
                parent, tmp_base, ..
            } => {
                for n in 1..=volume_count {
                    let _ = fs::remove_file(volume_path(parent, tmp_base, n));
                }
            }
        }
    }
}

impl RarArchive {
    // ── Constructors ───────────────────────────────────────────────────────

    /// Build a default `RarArchive` shell for a given mode and password.
    /// Every field is at its zero/default; the caller must call the
    /// appropriate open/prepare method afterward.
    fn new_for_mode(path: PathBuf, mode: Mode, password: Option<String>) -> Self {
        let (read, write) = match mode {
            Mode::Read => (Some(ReadState::default()), None),
            Mode::Write => (None, Some(WriteState::default())),
            Mode::Append => (Some(ReadState::default()), Some(WriteState::default())),
        };
        RarArchive {
            path,
            mode,
            entries: Vec::new(),
            sfx_offset: 0,
            rar4: false,
            stream: None,
            password,
            header_encryption: false,
            archive_encr: None,
            recovery_percent: None,
            recovery_volumes_percent: None,
            recovery_volumes_count: None,
            volume_paths: Vec::new(),
            progress: None,
            progress_member: 0,
            cancel: None,
            read,
            write,
        }
    }

    /// Install a cancellation flag. The flag is an `Arc<AtomicBool>` the
    /// caller owns: set it to `true` from any thread and the current
    /// create / append / extract / read / repair operation returns
    /// [`crate::RarError::Cancelled`] at its next per-member or per-chunk
    /// check point (at most one chunk or member later). Pass `None` to
    /// disable cancellation.
    ///
    /// The binding layer uses this to honor an `AbortSignal`: wrap the
    /// signal in a shared flag before starting the `AsyncTask`.
    pub fn set_cancel_flag(&mut self, flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>) {
        self.cancel = flag;
    }

    /// Set the compression thread count for this archive (like `-mt<N>`),
    /// overriding the process-global default. `None` restores the global
    /// default. The pool is chosen per thread count, so concurrent archives
    /// with different settings each run on their own pool and never
    /// interfere. Requires the `parallel` feature; without it compression
    /// stays sequential.
    pub fn set_compression_threads(&mut self, threads: Option<usize>) {
        self.write_ctx_mut().compression_threads = threads;
    }

    /// Effective compression worker count for this archive: the per-archive
    /// override when set, otherwise the process-global default.
    pub(crate) fn effective_threads(&self) -> usize {
        #[cfg(feature = "parallel")]
        {
            self.write_ctx()
                .compression_threads
                .unwrap_or_else(crate::parallel::default_compression_threads)
        }
        #[cfg(not(feature = "parallel"))]
        {
            1
        }
    }

    /// Check the cancellation flag; returns [`crate::RarError::Cancelled`]
    /// when the caller requested an abort.
    pub(crate) fn check_cancel(&self) -> RarResult<()> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
        {
            return Err(RarError::Cancelled);
        }
        Ok(())
    }

    // ── Read/Write context accessors ─────────────────────────────────────

    /// Immutable access to the read-side state. Panics if not opened for reading.
    pub(crate) fn read_ctx(&self) -> &ReadState {
        self.read.as_ref().expect("read context not available")
    }

    /// Mutable access to the read-side state. Panics if not opened for reading.
    pub(crate) fn read_ctx_mut(&mut self) -> &mut ReadState {
        self.read.as_mut().expect("read context not available")
    }

    /// Immutable access to the write-side state. Panics if not opened for writing.
    pub(crate) fn write_ctx(&self) -> &WriteState {
        self.write.as_ref().expect("write context not available")
    }

    /// Mutable access to the write-side state. Panics if not opened for writing.
    pub(crate) fn write_ctx_mut(&mut self) -> &mut WriteState {
        self.write.as_mut().expect("write context not available")
    }

    /// Ensure the write-side state exists. Read-mode mutation operations
    /// (`delete`, `rename`, `set_comment`, `add_recovery_record`) rewrite
    /// the archive and so need a write context even though the archive was
    /// opened for reading.
    fn ensure_write_ctx(&mut self) {
        self.write.get_or_insert_with(WriteState::default);
    }

    /// Open an existing RAR5 archive for reading.
    pub fn open(path: impl AsRef<Path>) -> RarResult<Self> {
        let mut archive = Self::new_for_mode(path.as_ref().to_path_buf(), Mode::Read, None);
        archive.open_read()?;
        Ok(archive)
    }

    /// Open an existing RAR5 archive for reading without a full block
    /// scan, using the quick-open record when present.
    ///
    /// Archives written with `quick_open` carry a cached copy of every
    /// file header; this opener reads only the main header + the QO
    /// record, so listing (`list` / `namelist`) is O(QO size) instead of
    /// O(archive size). Archives without a usable record (multi-volume,
    /// header-encrypted, or `-qo-`) transparently fall back to the full
    /// scan, and reading/extraction work identically either way.
    pub fn open_quick(path: impl AsRef<Path>) -> RarResult<Self> {
        let mut archive = Self::new_for_mode(path.as_ref().to_path_buf(), Mode::Read, None);
        archive.open_read_quick()?;
        Ok(archive)
    }

    /// Open an existing RAR5 archive with a password for encrypted content.
    pub fn open_with_password(path: impl AsRef<Path>, password: &str) -> RarResult<Self> {
        let mut archive = Self::new_for_mode(
            path.as_ref().to_path_buf(),
            Mode::Read,
            Some(password.to_string()),
        );
        archive.open_read()?;
        Ok(archive)
    }

    /// Password variant of [`Self::open_quick`] (falls back to the full
    /// scan for header-encrypted archives, which never carry a QO record).
    pub fn open_quick_with_password(path: impl AsRef<Path>, password: &str) -> RarResult<Self> {
        let mut archive = Self::new_for_mode(
            path.as_ref().to_path_buf(),
            Mode::Read,
            Some(password.to_string()),
        );
        archive.open_read_quick()?;
        Ok(archive)
    }

    /// Set the password for decryption.
    pub fn set_password(&mut self, password: &str) {
        self.password = Some(password.to_string());
    }

    /// Open an existing single-volume RAR5 archive for appending new
    /// members (like `rar a` on an existing archive).
    ///
    /// Existing members are preserved verbatim: the trailing quick-open and
    /// recovery records are truncated, new members are written after the
    /// existing blocks, and [`Self::close`] rebuilds the quick-open record
    /// and the recovery record over the whole archive. Existing members are
    /// never recompressed.
    ///
    /// The append is staged in a temporary sibling file (the surviving
    /// prefix is copied over) and moved to the archive path only when
    /// [`Self::close`] succeeds, so a failed or interrupted append never
    /// truncates or corrupts the original archive.
    pub fn open_append(path: impl AsRef<Path>) -> RarResult<Self> {
        Self::open_append_with_password(path, "")
    }

    /// Set the dictionary used for members added to an already-open
    /// archive (WinRAR's `-md`; `None` = default selection). Applies to
    /// archives opened with `open_append*`, where the create options are
    /// not available.
    pub fn set_dictionary(&mut self, dict_size_log: Option<u8>, dict_size_bytes: Option<u64>) {
        self.write_ctx_mut().dict_size_log = dict_size_log;
        self.write_ctx_mut().dict_size_bytes = dict_size_bytes;
    }

    /// Open an existing archive for appending, with a password for
    /// encrypted content.
    pub fn open_append_with_password(path: impl AsRef<Path>, password: &str) -> RarResult<Self> {
        let pw = if password.is_empty() {
            None
        } else {
            Some(password.to_string())
        };
        let mut archive = Self::new_for_mode(path.as_ref().to_path_buf(), Mode::Append, pw);
        archive.open_read()?;
        archive.prepare_append()?;
        Ok(archive)
    }

    /// Prepare an archive for appending: verify it is modifiable, capture
    /// the main header locator state, cache the existing file headers for
    /// the rebuilt quick-open record, and truncate the trailing end /
    /// quick-open / recovery blocks.
    fn prepare_append(&mut self) -> RarResult<()> {
        if self.volume_paths.len() > 1 {
            return Err(RarError::Unsupported(
                "appending to multi-volume archives is not supported (the official rar refuses too)"
                    .into(),
            ));
        }
        let path = self.path.clone();
        let mut reader = File::open(&path)?;
        reader.seek(SeekFrom::Start(self.sfx_offset + 8))?;

        let first =
            crate::rar50::headers::read_block(&mut reader, self.archive_block_key()?.as_ref())?
                .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
        let main_meta = match first.block_type {
            BLOCK_TYPE_ENCRYPT_HEADER => {
                let params = parse_archive_encrypt_header(&first.raw)?;
                self.handle_archive_encrypt_header(params)?;
                crate::rar50::headers::read_block(&mut reader, self.archive_block_key()?.as_ref())?
                    .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?
            }
            BLOCK_TYPE_ARCHIVE_HEADER => first,
            _ => {
                return Err(RarError::Format(
                    "archive is missing the main header".into(),
                ));
            }
        };
        let ah = ArchiveHeader::from_raw(&main_meta.raw)?;
        if ah.flags & ARCHIVE_FLAG_LOCKED != 0 {
            return Err(RarError::ArchiveLocked);
        }
        let (had_qo, had_rr, _) = split_main_extra(&ah.extra_data)?;
        let (qo_field_pos, rr_field_pos) = main_header_locator_fields(&main_meta)?;
        self.write_ctx_mut().main_header_start = Some(main_meta.block_start);
        self.write_ctx_mut().qo_offset_field_pos = qo_field_pos.map(|p| p as u64);
        self.write_ctx_mut().rr_offset_field_pos = rr_field_pos.map(|p| p as u64);
        self.write_ctx_mut().quick_open = had_qo && !self.header_encryption;

        // Walk the remaining blocks: cache existing headers for the rebuilt
        // quick-open record and find the truncation point (the first
        // trailing QO/RR service block, or the end block).
        let mut truncate_pos = None;
        let mut last_file_end = 0u64;
        let mut rr_percent = None;
        while let Some(meta) =
            crate::rar50::headers::read_block(&mut reader, self.archive_block_key()?.as_ref())?
        {
            match meta.block_type {
                BLOCK_TYPE_END_ARCHIVE => {
                    if truncate_pos.is_none() {
                        truncate_pos = Some(meta.block_start);
                    }
                    break;
                }
                BLOCK_TYPE_FILE_HEADER => {
                    if self.write_ctx().quick_open {
                        self.write_ctx_mut()
                            .quick_open_entries
                            .push((meta.block_start, meta.header_bytes.clone()));
                    }
                    last_file_end = meta.data_end;
                }
                BLOCK_TYPE_SERVICE_HEADER => {
                    let name = self.service_block_name(&meta)?;
                    if name.as_deref() == Some("RR") {
                        rr_percent = self.rr_percent_from_block(&meta);
                    }
                    if (name.as_deref() == Some("QO") || name.as_deref() == Some("RR"))
                        && meta.block_start >= last_file_end
                        && truncate_pos.is_none()
                    {
                        truncate_pos = Some(meta.block_start);
                    }
                }
                _ => {}
            }
            // Advance past the data area (headers are read separately).
            reader.seek(SeekFrom::Start(meta.data_end))?;
        }
        let truncate_pos = truncate_pos.unwrap_or(last_file_end);

        self.recovery_percent = if had_rr { rr_percent } else { None };

        // Stage the append: copy the surviving prefix into a temporary
        // sibling file and append the new members there. The temp is moved
        // over the archive on close, so a failed or interrupted append
        // never truncates or corrupts the original archive.
        let tmp_path = temp_sibling_path(&path);
        let mut src = File::open(&path)?;
        let mut dst = read_write_create(&tmp_path)?;
        copy_prefix(&mut src, &mut dst, truncate_pos)?;
        self.write_ctx_mut().pending = Some(PendingCommit::Single(tmp_path));
        self.stream = Some(Box::new(dst));
        Ok(())
    }

    /// Lock the archive (like `rar k`): sets the `LOCKED` flag in the main
    /// archive header, making the archive read-only. Locking is
    /// irreversible.
    pub fn lock(&mut self) -> RarResult<()> {
        if self.mode != Mode::Read {
            return Err(RarError::Format(
                "lock requires an archive opened for reading".into(),
            ));
        }
        if self.volume_paths.len() > 1 {
            return Err(RarError::Unsupported(
                "locking multi-volume archives is not supported (lock the first volume instead)"
                    .into(),
            ));
        }
        if self.main_header_is_locked()? {
            return Ok(());
        }
        let path = self.path.clone();
        let mut reader = File::open(&path)?;
        reader.seek(SeekFrom::Start(self.sfx_offset + 8))?;
        let first =
            crate::rar50::headers::read_block(&mut reader, self.archive_block_key()?.as_ref())?
                .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
        let main_meta = match first.block_type {
            BLOCK_TYPE_ENCRYPT_HEADER => {
                let params = parse_archive_encrypt_header(&first.raw)?;
                self.handle_archive_encrypt_header(params)?;
                crate::rar50::headers::read_block(&mut reader, self.archive_block_key()?.as_ref())?
                    .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?
            }
            BLOCK_TYPE_ARCHIVE_HEADER => first,
            _ => {
                return Err(RarError::Format(
                    "archive is missing the main header".into(),
                ));
            }
        };

        // Patch the archive-level flags in the plaintext header and
        // recompute the CRC.
        let data = &main_meta.raw.header_data;
        let mut offset = 0usize;
        let (_, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("block type: {e}")))?;
        offset += n;
        let (flags, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("block flags: {e}")))?;
        offset += n;
        if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("extra size: {e}")))?;
            offset += n;
        }
        let (arch_flags, vint_len) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("archive flags: {e}")))?;
        let new_flags = arch_flags | ARCHIVE_FLAG_LOCKED;
        let new_vint = vint::encode(new_flags);
        if new_vint.len() != vint_len {
            return Err(RarError::Unsupported(
                "cannot lock: the archive flags field grows when locked".into(),
            ));
        }
        let mut hdr = main_meta.header_bytes.clone();
        // Header bytes: [crc 4][size vint][body]; the flags field lives at
        // 4 + hsize_vint_len + offset within the body.
        let body_off = 4 + main_meta.hsize_vint_len + offset;
        hdr[body_off..body_off + vint_len].copy_from_slice(&new_vint);
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&hdr[4..]);
        let crc = hasher.finalize();
        hdr[..4].copy_from_slice(&crc.to_le_bytes());

        self.stream = Some(Box::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)?,
        ));
        let main_start = main_meta.block_start;
        self.stream
            .as_mut()
            .unwrap()
            .seek(SeekFrom::Start(main_start))?;
        self.write_block_header(&hdr)?;
        self.stream = None;
        Ok(())
    }

    /// Add an inline recovery record to an existing archive (like `rar rr
    /// <percent>`), rebuilding the archive header locator. Existing
    /// members are copied verbatim; an existing recovery record is
    /// replaced.
    pub fn add_recovery_record(&mut self, percent: u8) -> RarResult<()> {
        if self.mode != Mode::Read {
            return Err(RarError::Format(
                "add_recovery_record requires an archive opened for reading".into(),
            ));
        }
        self.ensure_write_ctx();
        if self.volume_paths.len() > 1 {
            return Err(RarError::Unsupported(
                "recovery records are not supported for multi-volume archives".into(),
            ));
        }
        let deleted = vec![false; self.entries.len()];
        let src_path = self.path.clone();
        let tmp_path = temp_sibling_path(&src_path);
        let mut reader = File::open(&src_path)?;
        self.stream = Some(Box::new(read_write_create(&tmp_path)?));
        self.write_ctx_mut().quick_open_entries.clear();
        self.header_encryption = false;
        self.archive_encr = None;

        let result = self.rewrite_blocks(
            &mut reader,
            &deleted,
            None,
            Some(percent.min(100)),
            None,
            None,
            &src_path,
            &tmp_path,
        );
        self.stream = None;
        match result {
            Ok(()) => replace_file(&tmp_path, &src_path)?,
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(e);
            }
        }
        self.mode = Mode::Read;
        self.open_read()?;
        Ok(())
    }

    /// Create a new RAR5 archive with explicit options (overwrites an
    /// existing file).
    ///
    /// This is the full-featured constructor: `solid`, `quick_open` and
    /// `blake2` options can be combined with passwords, header encryption,
    /// recovery records and volume sizes.
    ///
    /// The archive data is staged in a temporary sibling file and moved to
    /// `path` only when [`Self::close`] succeeds, so an aborted or failed
    /// write never leaves a partial archive at `path`.
    pub fn create_with_options(
        path: impl AsRef<Path>,
        opts: crate::options::CreateOptions,
    ) -> RarResult<Self> {
        let path = path.as_ref().to_path_buf();
        let mut archive = Self::new_with_options(path, opts)?;
        archive.open_write()?;
        Ok(archive)
    }

    /// Create an archive writing into a caller-provided seekable sink
    /// (test seam; `Cursor<Vec<u8>>` gives an in-memory archive). The sink
    /// can be recovered with [`Self::finish_into_sink`] after closing.
    #[cfg(test)]
    pub(crate) fn create_with_sink(
        path: PathBuf,
        opts: crate::options::CreateOptions,
        sink: Box<dyn ArchiveStream>,
    ) -> RarResult<Self> {
        if opts.volume_size.is_some() {
            return Err(RarError::Unsupported(
                "in-memory sinks are single-volume only".into(),
            ));
        }
        if opts.recovery_volumes_percent.is_some() || opts.recovery_volume_count.is_some() {
            return Err(RarError::Unsupported(
                "recovery volumes require files on disk".into(),
            ));
        }
        let mut archive = Self::new_with_options(path, opts)?;
        archive.stream = Some(sink);
        archive.write_signature()?;
        archive.write_archive_encryption_header_if_needed()?;
        archive.write_archive_header()?;
        Ok(archive)
    }

    /// Validate options and build the archive state (without opening the
    /// stream).
    fn new_with_options(path: PathBuf, opts: crate::options::CreateOptions) -> RarResult<Self> {
        let is_rar4 = opts.format_version == ArchiveVersion::Rar40;
        if is_rar4 {
            // RAR4 does not support these RAR5-specific features.
            if opts.quick_open {
                return Err(RarError::Unsupported(
                    "quick-open is not supported for RAR4 archives".into(),
                ));
            }
            if opts.blake2 {
                return Err(RarError::Unsupported(
                    "BLAKE2sp hashes are not supported for RAR4 archives".into(),
                ));
            }
            // Inline recovery records are now supported on single-volume
            // RAR4 archives too (the NEWSUB 0x7a form WinRAR writes); the
            // multi-volume rejection below still applies, matching WinRAR.
            if opts.recovery_percent.is_some() && opts.volume_size.is_some() {
                return Err(RarError::Unsupported(
                    "recovery records are not supported for multi-volume archives".into(),
                ));
            }
            if opts.recovery_volumes_percent.is_some() || opts.recovery_volume_count.is_some() {
                return Err(RarError::Unsupported(
                    "recovery volumes are not supported for RAR4 archives".into(),
                ));
            }
            if opts.save_owner {
                return Err(RarError::Unsupported(
                    "owner/group records are not supported for RAR4 archives".into(),
                ));
            }
            if opts.save_streams {
                return Err(RarError::Unsupported(
                    "NTFS stream records are not supported for RAR4 archives".into(),
                ));
            }
            if opts.dict_size_bytes.is_some() {
                return Err(RarError::Unsupported(
                    "RAR4 does not support RAR7 dictionary sizes".into(),
                ));
            }
        }
        if opts.encrypt_headers && opts.password.as_deref().is_none_or(|pw| pw.is_empty()) {
            return Err(RarError::Encrypted(
                "header encryption requires a password".into(),
            ));
        }
        // Header encryption is supported for multi-volume archives: every
        // volume starts with the plaintext encryption header and all
        // subsequent blocks are `[IV][AES-256-CBC header]` (WinRAR -hp
        // convention).
        if opts.recovery_percent.is_some() && opts.volume_size.is_some() {
            return Err(RarError::Unsupported(
                "recovery records are not supported for multi-volume archives".into(),
            ));
        }
        if (opts.recovery_volumes_percent.is_some() || opts.recovery_volume_count.is_some())
            && opts.volume_size.is_none()
        {
            return Err(RarError::Unsupported(
                "recovery volumes require a volume size".into(),
            ));
        }
        // Quick-open only applies to single-volume archives without header
        // encryption; otherwise it is silently skipped (matching the
        // reference writer behavior).
        let quick_open = opts.quick_open && !opts.encrypt_headers && opts.volume_size.is_none();

        let archive = RarArchive {
            path,
            mode: Mode::Write,
            entries: Vec::new(),
            sfx_offset: 0,
            rar4: is_rar4,
            stream: None,
            password: opts.password,
            header_encryption: opts.encrypt_headers,
            archive_encr: None,
            recovery_percent: opts.recovery_percent.map(|p| p.min(100)),
            recovery_volumes_percent: opts.recovery_volumes_percent.map(|p| p.min(100)),
            recovery_volumes_count: opts.recovery_volume_count,
            cancel: None,
            volume_paths: Vec::new(),
            progress: None,
            progress_member: 0,
            read: None,
            write: Some(WriteState {
                solid_mode: opts.solid,
                solid_reset: opts.solid_reset,
                last_solid_ext: None,
                encoder_state: None,
                rar4_solid_encoder: None,
                rar4_solid_run_has_member: false,
                compression_threads: opts.threads,
                dict_size_log: opts.dict_size_log,
                dict_size_bytes: opts.dict_size_bytes,
                force_v70: opts.force_v70,
                save_ctime: opts.save_ctime,
                save_atime: opts.save_atime,
                save_mtime: opts.save_mtime,
                save_owner: opts.save_owner,
                save_streams: opts.save_streams,
                time_precision_seconds: opts.time_precision_seconds,
                blake2: opts.blake2,
                quick_open,
                quick_open_entries: Vec::new(),
                qo_offset_field_pos: None,
                main_header_start: None,
                rr_offset_field_pos: None,
                pending: None,
                volume_size: opts.volume_size,
                current_volume: 0,
                volume_bytes_written: 0,
            }),
        };
        Ok(archive)
    }

    // ── Progress ──────────────────────────────────────────────────────────

    /// Set an optional progress callback for archive creation.
    ///
    /// The callback receives `(bytes_committed, bytes_total)` where
    /// `bytes_total` is the total input byte count of the whole write
    /// operation (a single member, or the full batch passed to
    /// [`RarArchive::add_batch`]) and `bytes_committed` is the monotonic,
    /// operation-global number of input bytes processed so far. Events are
    /// delivered serially even when members compress concurrently on the
    /// Rayon pool, and `bytes_committed` never moves backwards.
    ///
    /// Callers no longer need to stitch per-file deltas together: the
    /// reported percentage is `bytes_committed / bytes_total` directly.
    pub fn set_progress_callback(&mut self, callback: Option<Box<dyn FnMut(u64, u64) + Send>>) {
        self.progress = callback
            .map(|cb| std::sync::Arc::new(std::sync::Mutex::new(ProgressTracker::new(Some(cb)))));
        self.progress_member = 0;
    }

    /// Override the progress denominator. `add_batch` sets this automatically
    /// to the sum of its members' sizes; single-member `add_*` calls fall back
    /// to the member's own size when this is left unset.
    pub fn set_progress_total(&mut self, total: u64) {
        if let Some(progress) = &self.progress {
            progress.lock().expect("progress lock").set_total(total);
        }
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

impl Drop for RarArchive {
    fn drop(&mut self) {
        let _ = self.close();
        // `close` may have failed (or never been reached); remove any
        // staged files that were not committed so a failed or interrupted
        // write leaves no garbage behind and never a partial archive at
        // the final path. A read-only archive has no write context.
        if let Some(pending) = self.write.as_mut().and_then(|w| w.pending.take()) {
            pending.cleanup(self.volume_paths.len());
        }
    }
}
