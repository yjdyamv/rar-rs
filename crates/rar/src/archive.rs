use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::codec::DecoderState;
use crate::codec::rar50 as compression;
use crate::crypto::{self, parse_archive_encrypt_header};
use crate::error::{RarError, RarResult};
use crate::io_util::{
    copy_prefix, read_write_create, replace_file, temp_sibling_path, temp_suffix,
};
use crate::rar50::headers::*;
use crate::rar50::vint;
use crate::rar50::*;
use crate::write_progress::ProgressTracker;

/// Surgical rewrite pipeline (delete/rename/comment/recovery mutation) in
/// a sibling impl block.
#[path = "rewrite.rs"]
mod rewrite;

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

/// A single entry in the archive (public API).
#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    pub header: FileHeader,
    pub chunks: Vec<DataChunk>,
}

/// One entry to add through [`RarArchive::add_batch`].
///
/// Borrowed views only: byte payloads are copied by the library during
/// preparation, and file entries are read (up to the batch member cap)
/// before compression.
#[derive(Debug, Clone, Copy)]
pub enum BatchEntry<'a> {
    /// In-memory payload added under `name`.
    Bytes {
        /// Archive entry name.
        name: &'a str,
        /// Raw member content.
        data: &'a [u8],
        /// Compression level 0..=5.
        level: u8,
    },
    /// File from disk; `name` overrides the archive entry name when set.
    File {
        /// Path on disk.
        path: &'a Path,
        /// Optional archive name override.
        name: Option<&'a str>,
        /// Compression level 0..=5.
        level: u8,
    },
    /// Directory header only (no recursion).
    Directory {
        /// Path on disk.
        path: &'a Path,
        /// Optional archive name override (basename when `None`).
        name: Option<&'a str>,
    },
}

/// A fully prepared member: hashed, filtered/compressed (or STORE) and
/// encrypted, ready to be written in archive order.
#[cfg(feature = "parallel")]
pub(crate) struct PreparedEntry {
    pub(crate) name: String,
    pub(crate) unpacked_size: u64,
    pub(crate) attrs: u64,
    pub(crate) mtime: u32,
    pub(crate) file_crc: u32,
    pub(crate) method: u8,
    pub(crate) dict_size_log: u8,
    pub(crate) dict_size_bytes: Option<u64>,
    pub(crate) extra_data: Vec<u8>,
    pub(crate) stored_hash: Option<[u8; 32]>,
    pub(crate) payload: Vec<u8>,
}

/// Immutable snapshot of the writer settings needed to prepare a member
/// off-thread. `Sync`-safe where `&RarArchive` is not (the progress
/// callback is a `FnMut` trait object).
#[cfg(feature = "parallel")]
pub(crate) struct BatchPrepareCtx<'a> {
    pub(crate) password: Option<&'a str>,
    pub(crate) blake2: bool,
    pub(crate) dict_size_log: Option<u8>,
    pub(crate) dict_size_bytes: Option<u64>,
    pub(crate) force_v70: bool,
    pub(crate) save_ctime: bool,
    pub(crate) save_atime: bool,
    pub(crate) save_mtime: bool,
    pub(crate) save_owner: bool,
    pub(crate) time_precision_seconds: bool,
    /// Caller-owned cancellation flag, checked per chunk in the parallel
    /// prepare loop; `None` = never cancelled.
    pub(crate) cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

/// Decrypted member payload plus the key material needed for integrity
/// verification.
pub(crate) struct DecryptedPayload {
    pub(crate) data: Vec<u8>,
    pub(crate) params: Option<crypto::EncryptionParams>,
    pub(crate) keys: Option<crypto::DerivedKeys>,
}

/// One step of a surgical archive rewrite.
enum RewriteOp {
    /// Copy one block verbatim: `header_bytes` followed by `len` bytes of
    /// data starting at `src_data` in the original archive. `qo_header`
    /// holds the header bytes for the rebuilt quick-open record (copied
    /// file blocks only, plaintext archives only).
    CopyBlock {
        header_bytes: Vec<u8>,
        src_data: u64,
        len: u64,
        qo_header: Option<Vec<u8>>,
    },
    /// Decode (and recompress when kept) one member of the affected solid
    /// chain.
    Recompress { idx: usize, is_deleted: bool },
}

/// The result of planning a rewrite: the blocks to emit, in order.
struct RewritePlan {
    ops: Vec<RewriteOp>,
    /// Verbatim bytes of the archive encryption header (if any), written
    /// before the rebuilt main header.
    encrypt_header: Option<Vec<u8>>,
    /// Parsed main header block (plaintext).
    main_meta: BlockMeta,
    /// Recovery percentage from the dropped RR record; the record is
    /// rebuilt over the rewritten archive.
    rr_percent: Option<u8>,
    /// New archive comment (CMT service block), written right after the
    /// main header.
    comment: Option<Vec<u8>>,
}

/// Lazily opened readers for every volume of the original archive.
struct VolumeReaders {
    files: Vec<Option<File>>,
    paths: Vec<PathBuf>,
}

impl VolumeReaders {
    fn new(paths: &[PathBuf]) -> Self {
        VolumeReaders {
            files: (0..paths.len()).map(|_| None).collect(),
            paths: paths.to_vec(),
        }
    }

    fn read_chunk(&mut self, vol: usize, offset: u64, len: u64) -> RarResult<Vec<u8>> {
        let file = self
            .files
            .get_mut(vol)
            .ok_or_else(|| RarError::Format(format!("chunk references missing volume {vol}")))?;
        if file.is_none() {
            *file = Some(File::open(&self.paths[vol])?);
        }
        let f = file.as_mut().unwrap();
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len as usize];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// Parse the recovery parameters out of an existing `.rev` file header:
/// `(rec_count, data_count)`.
fn rev_params_from_file(path: &Path) -> RarResult<(u32, u32)> {
    let data = std::fs::read(path)?;
    if data.len() < 8 + 4 + 4 + 1 + 2 + 2 + 2 + 4
        || &data[..8] != crate::recovery::rev5::REV5_SIGNATURE
    {
        return Err(RarError::Format(format!(
            "{}: not a RAR5 recovery volume",
            path.display()
        )));
    }
    let mut off = 8 + 4 + 4; // signature + header CRC + header size
    if data[off] != 1 {
        return Err(RarError::Format(format!(
            "{}: unsupported recovery volume version",
            path.display()
        )));
    }
    off += 1;
    let data_count = u16::from_le_bytes(data[off..off + 2].try_into().unwrap()) as u32;
    off += 2;
    let rec_count = u16::from_le_bytes(data[off..off + 2].try_into().unwrap()) as u32;
    Ok((rec_count, data_count))
}

/// Read-ahead copy job: `len` bytes from `src` in the original archive.
#[cfg(feature = "parallel")]
#[derive(Clone, Copy)]
struct CopyJob {
    src: u64,
    len: u64,
}

/// Bounded producer thread that prefetches verbatim block data ahead of
/// the writer, overlapping source reads with destination writes (and, for
/// solid chains, with the CPU-bound recompression).
#[cfg(feature = "parallel")]
struct CopyPipeline {
    rx: std::sync::mpsc::Receiver<Result<Vec<u8>, RarError>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "parallel")]
impl CopyPipeline {
    const CHUNK: usize = 4 * 1024 * 1024;
    const QUEUE: usize = 4;

    fn start(src_path: &Path, jobs: &[CopyJob]) -> Self {
        let src_path = src_path.to_path_buf();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, RarError>>(Self::QUEUE);
        let jobs = jobs.to_vec();
        let handle = std::thread::spawn(move || {
            let mut f = match File::open(src_path) {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(Err(e.into()));
                    return;
                }
            };
            for job in jobs {
                if let Err(e) = f.seek(SeekFrom::Start(job.src)) {
                    let _ = tx.send(Err(e.into()));
                    return;
                }
                let mut left = job.len;
                while left > 0 {
                    let want = left.min(Self::CHUNK as u64) as usize;
                    let mut buf = vec![0u8; want];
                    if let Err(e) = f.read_exact(&mut buf) {
                        let _ = tx.send(Err(e.into()));
                        return;
                    }
                    if tx.send(Ok(buf)).is_err() {
                        return; // consumer aborted
                    }
                    left -= want as u64;
                }
            }
        });
        CopyPipeline {
            rx,
            handle: Some(handle),
        }
    }

    /// Next prefetched buffer, in job order.
    fn take(&self) -> RarResult<Option<Vec<u8>>> {
        match self.rx.recv() {
            Ok(Ok(buf)) => Ok(Some(buf)),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(None),
        }
    }

    fn finish(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl ArchiveEntry {
    /// Archive member name (forward-slash separated, UTF-8).
    pub fn name(&self) -> &str {
        &self.header.name
    }

    /// Uncompressed size in bytes.
    pub fn size(&self) -> u64 {
        self.header.unpacked_size
    }

    /// On-disk (packed) size in bytes.
    pub fn compressed_size(&self) -> u64 {
        self.header.packed_size
    }

    /// Whether this entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.header.is_directory
    }

    /// CRC32 of the uncompressed content, if present.
    pub fn crc32(&self) -> Option<u32> {
        self.header.crc32_val
    }

    /// Human-readable compression method name ("Store", "Normal", etc.).
    pub fn method_name(&self) -> &'static str {
        method_name(self.header.comp_method)
    }

    /// Numeric compression method (0 = store, 1..=5 = level).
    pub fn method(&self) -> u8 {
        self.header.comp_method
    }

    /// Modification time as a Unix timestamp (seconds since epoch).
    pub fn mtime(&self) -> u32 {
        self.header.mtime
    }

    /// Modification time nanosecond component (`None` when stored at
    /// 1-second precision or when the archive has no FILE_TIME record).
    pub fn mtime_ns(&self) -> Option<u32> {
        self.header.mtime_ns
    }

    /// Creation time (seconds, nanoseconds) from the FILE_TIME extra
    /// record (`None` when absent).
    pub fn ctime(&self) -> Option<(u64, u32)> {
        self.header.ctime
    }

    /// Last access time (seconds, nanoseconds) from the FILE_TIME extra
    /// record (`None` when absent).
    pub fn atime(&self) -> Option<(u64, u32)> {
        self.header.atime
    }

    /// Host OS identifier (0 = Windows, 1 = Unix).
    pub fn host_os(&self) -> u64 {
        self.header.host_os
    }

    /// File attributes (OS-specific).
    pub fn attributes(&self) -> u64 {
        self.header.attributes
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
    /// Persistent decoder state for RAR5 solid archive chains.
    pub(crate) solid_state: Option<DecoderState>,
    /// Index of the last file decoded in the solid chain (-1 = none).
    pub(crate) solid_decoded_through: isize,
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
    /// File offset of the main archive header (for the recovery-record
    /// locator patch written at close time).
    pub(crate) main_header_start: Option<u64>,
    /// File offset of the recovery-record offset vint inside the main
    /// header's locator record (preallocated, patched at close time).
    pub(crate) rr_offset_field_pos: Option<u64>,
    /// All volume file paths (multi-volume archives).
    pub(crate) volume_paths: Vec<PathBuf>,
    /// Volume size limit for multi-volume creation (None = single volume).
    pub(crate) volume_size: Option<u64>,
    /// Current volume number during creation (1-indexed).
    pub(crate) current_volume: usize,
    /// Bytes written in the current volume during creation.
    pub(crate) volume_bytes_written: u64,
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
    /// Create a solid archive (shared LZ window across compressed members).
    pub(crate) solid_mode: bool,
    /// Write a quick-open ("QO") service record at close time.
    pub(crate) quick_open: bool,
    /// Write BLAKE2sp hash records for members.
    pub(crate) blake2: bool,
    /// Cached (offset, full header bytes) of file headers for quick-open.
    pub(crate) quick_open_entries: Vec<(u64, Vec<u8>)>,
    /// File offset of the quick-open offset vint inside the main header's
    /// locator record (preallocated, patched at close time).
    pub(crate) qo_offset_field_pos: Option<u64>,
    /// Persistent RAR5 encoder state for solid archives.
    pub(crate) encoder_state: Option<crate::codec::EncoderState>,
    /// Per-archive compression thread count (`-mt`); `None` = process-global
    /// default. The compression pool is selected per thread count, so
    /// concurrent archives with different values never interfere.
    pub(crate) compression_threads: Option<usize>,
    /// Requested dictionary log for compression (WinRAR `-md`);
    /// `None` = default selection.
    pub(crate) dict_size_log: Option<u8>,
    /// Requested dictionary size in bytes for RAR7 (v70) members
    /// (WinRAR `-md` above 4 GiB, any value > 4 GiB accepted).
    pub(crate) dict_size_bytes: Option<u64>,
    /// Force RAR7 (v70) member headers even below the 4 GiB threshold
    /// (test seam; see `CreateOptions::force_v70`).
    pub(crate) force_v70: bool,
    /// Save creation/change time in the FILE_TIME extra record (`-tsc`).
    pub(crate) save_ctime: bool,
    /// Save last access time in the FILE_TIME extra record (`-tsa`).
    pub(crate) save_atime: bool,
    /// Save the modification time (`-tsm`; false with `-tsm-`/`-ts-`).
    pub(crate) save_mtime: bool,
    /// Save owner/group on Unix (`-ow`).
    pub(crate) save_owner: bool,
    /// Save NTFS alternate data streams (`-os`; Windows only).
    pub(crate) save_streams: bool,
    /// NTFS alternate data streams ("STM" service records) attached to
    /// members, in archive order.
    pub(crate) streams: Vec<StreamRecord>,
    /// Store timestamps at 1-second precision (`-ts...1`).
    pub(crate) time_precision_seconds: bool,
    /// Options for the current read/extract operation (set per call).
    pub(crate) extract_options: crate::options::ExtractOptions,
    /// Staged write target during an uncommitted create/append: the data
    /// goes to temporary sibling files and is moved over the final paths
    /// only after [`Self::close`] succeeds, so a failed or interrupted
    /// operation never leaves a partial archive at the final path.
    pub(crate) pending: Option<PendingCommit>,
    /// Caller-owned cancellation flag: when set to true, long-running
    /// create/extract/repair operations abort at their next check point
    /// with [`crate::RarError::Cancelled`]. Installed with
    /// [`Self::set_cancel_flag`]; `None` = never cancelled.
    pub(crate) cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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
        RarArchive {
            path,
            mode,
            entries: Vec::new(),
            sfx_offset: 0,
            stream: None,
            solid_state: None,
            solid_decoded_through: -1,
            password,
            header_encryption: false,
            archive_encr: None,
            recovery_percent: None,
            recovery_volumes_percent: None,
            recovery_volumes_count: None,
            main_header_start: None,
            rr_offset_field_pos: None,
            volume_paths: Vec::new(),
            volume_size: None,
            current_volume: 0,
            volume_bytes_written: 0,
            progress: None,
            progress_member: 0,
            solid_mode: false,
            quick_open: false,
            blake2: false,
            quick_open_entries: Vec::new(),
            qo_offset_field_pos: None,
            encoder_state: None,
            compression_threads: None,
            dict_size_log: None,
            dict_size_bytes: None,
            force_v70: false,
            save_ctime: false,
            save_atime: false,
            save_mtime: true,
            save_owner: false,
            save_streams: false,
            streams: Vec::new(),
            time_precision_seconds: false,
            extract_options: crate::options::ExtractOptions::default(),
            pending: None,
            cancel: None,
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
        self.compression_threads = threads;
    }

    /// Effective compression worker count for this archive: the per-archive
    /// override when set, otherwise the process-global default.
    pub(crate) fn effective_threads(&self) -> usize {
        #[cfg(feature = "parallel")]
        {
            self.compression_threads
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
        self.dict_size_log = dict_size_log;
        self.dict_size_bytes = dict_size_bytes;
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
            crate::rar50::headers::read_block(&mut reader, self.archive_block_key().as_ref())?
                .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
        let main_meta = match first.block_type {
            BLOCK_TYPE_ENCRYPT_HEADER => {
                let params = parse_archive_encrypt_header(&first.raw)?;
                let password = self.password.as_ref().ok_or_else(|| {
                    RarError::Encrypted("archive has encrypted headers; provide a password".into())
                })?;
                if !params.verify_password(password) {
                    return Err(RarError::WrongPassword);
                }
                self.archive_encr = Some(params);
                self.header_encryption = true;
                crate::rar50::headers::read_block(&mut reader, self.archive_block_key().as_ref())?
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
        self.main_header_start = Some(main_meta.block_start);
        self.qo_offset_field_pos = qo_field_pos.map(|p| p as u64);
        self.rr_offset_field_pos = rr_field_pos.map(|p| p as u64);
        self.quick_open = had_qo && !self.header_encryption;

        // Walk the remaining blocks: cache existing headers for the rebuilt
        // quick-open record and find the truncation point (the first
        // trailing QO/RR service block, or the end block).
        let mut truncate_pos = None;
        let mut last_file_end = 0u64;
        let mut rr_percent = None;
        while let Some(meta) =
            crate::rar50::headers::read_block(&mut reader, self.archive_block_key().as_ref())?
        {
            match meta.block_type {
                BLOCK_TYPE_END_ARCHIVE => {
                    if truncate_pos.is_none() {
                        truncate_pos = Some(meta.block_start);
                    }
                    break;
                }
                BLOCK_TYPE_FILE_HEADER => {
                    if self.quick_open {
                        self.quick_open_entries
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
        self.pending = Some(PendingCommit::Single(tmp_path));
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
            crate::rar50::headers::read_block(&mut reader, self.archive_block_key().as_ref())?
                .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
        let main_meta = match first.block_type {
            BLOCK_TYPE_ENCRYPT_HEADER => {
                let params = parse_archive_encrypt_header(&first.raw)?;
                let password = self.password.as_ref().ok_or_else(|| {
                    RarError::Encrypted("archive has encrypted headers; provide a password".into())
                })?;
                if !params.verify_password(password) {
                    return Err(RarError::WrongPassword);
                }
                self.archive_encr = Some(params);
                self.header_encryption = true;
                crate::rar50::headers::read_block(&mut reader, self.archive_block_key().as_ref())?
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
        self.quick_open_entries.clear();
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
            stream: None,
            solid_state: None,
            solid_decoded_through: -1,
            password: opts.password,
            header_encryption: opts.encrypt_headers,
            archive_encr: None,
            recovery_percent: opts.recovery_percent.map(|p| p.min(100)),
            recovery_volumes_percent: opts.recovery_volumes_percent.map(|p| p.min(100)),
            recovery_volumes_count: opts.recovery_volume_count,
            main_header_start: None,
            cancel: None,
            rr_offset_field_pos: None,
            volume_paths: Vec::new(),
            volume_size: opts.volume_size,
            current_volume: 0,
            volume_bytes_written: 0,
            progress: None,
            progress_member: 0,
            solid_mode: opts.solid,
            quick_open,
            blake2: opts.blake2,
            quick_open_entries: Vec::new(),
            qo_offset_field_pos: None,
            encoder_state: None,
            compression_threads: opts.threads,
            dict_size_log: opts.dict_size_log,
            dict_size_bytes: opts.dict_size_bytes,
            force_v70: opts.force_v70,
            save_ctime: opts.save_ctime,
            save_atime: opts.save_atime,
            save_mtime: opts.save_mtime,
            save_owner: opts.save_owner,
            save_streams: opts.save_streams,
            streams: Vec::new(),
            time_precision_seconds: opts.time_precision_seconds,
            extract_options: crate::options::ExtractOptions::default(),
            pending: None,
        };
        Ok(archive)
    }

    // ── Lifecycle ──────────────────────────────────────────────────────────

    fn open_write(&mut self) -> RarResult<()> {
        if let Some(volume_size) = self.volume_size {
            if volume_size == 0 {
                return Err(RarError::Format(
                    "volume size must be greater than zero".into(),
                ));
            }
            let base = get_volume_base(&self.path);
            let parent = self.path.parent().unwrap_or(Path::new(".")).to_path_buf();
            // Stage the volumes under a temporary volume base; they are
            // moved over the final `{base}.partN.rar` names on close.
            let tmp_base = format!(".{base}.rar5tmp-{}", temp_suffix());
            self.volume_paths = vec![volume_path(&parent, &base, 1)];
            self.current_volume = 1;
            self.pending = Some(PendingCommit::Volumes {
                parent: parent.clone(),
                tmp_base: tmp_base.clone(),
                final_base: base,
            });
            let f = read_write_create(&volume_path(&parent, &tmp_base, 1))?;
            self.stream = Some(Box::new(f));
            self.write_signature()?;
            self.write_archive_encryption_header_if_needed()?;
            self.write_archive_header_vol(None)?;
            self.volume_bytes_written = self.stream.as_mut().unwrap().stream_position()?;
            return Ok(());
        }

        // Stage the archive under a temporary sibling name; it is moved
        // over the final path on close, so a failed or interrupted
        // creation never leaves a partial archive at the target path.
        let tmp_path = temp_sibling_path(&self.path);
        self.pending = Some(PendingCommit::Single(tmp_path.clone()));
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
    fn write_archive_encryption_header_if_needed(&mut self) -> RarResult<()> {
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
    fn finish_writing(&mut self) -> RarResult<()> {
        if self.stream.is_some() && (self.mode == Mode::Write || self.mode == Mode::Append) {
            let qo_offset = if self.quick_open {
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
            } else if self.quick_open {
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
    pub(crate) fn finish_into_sink(mut self) -> RarResult<Box<dyn ArchiveStream>> {
        self.finish_writing()?;
        self.stream
            .take()
            .ok_or_else(|| RarError::Format("no archive stream to take".into()))
    }

    /// Generate the `.rev` recovery-volume files for a completed
    /// multi-volume archive set (WinRAR `-rv` equivalent).
    fn write_recovery_volumes(&mut self) -> RarResult<()> {
        // Exact count wins; the percent variant is converted at close time.
        let nd = self.volume_paths.len();
        let rec_count = if let Some(count) = self.recovery_volumes_count {
            (count as usize).min(nd)
        } else if let Some(percent) = self.recovery_volumes_percent {
            crate::recovery::rev5::plan_recovery_volume_count(nd, percent as u64)?
        } else {
            return Ok(());
        };

        let written =
            crate::recovery::rev5::build_recovery_volumes_for_set(&self.volume_paths, rec_count)?;
        let _ = written;
        self.recovery_volumes_percent = None;
        Ok(())
    }

    /// Compute the RAR5 recovery record over the archive written so far
    /// and append the `"RR"` service header. The main header locator was
    /// already patched by [`Self::close`].
    fn write_recovery_record(&mut self) -> RarResult<()> {
        let path = self.write_file_path().to_path_buf();
        self.write_recovery_record_from(&path)
    }

    /// The file currently being written: the staged temporary sibling
    /// during an uncommitted create/append, the final path otherwise.
    fn write_file_path(&self) -> &Path {
        match &self.pending {
            Some(PendingCommit::Single(tmp)) => tmp,
            _ => &self.path,
        }
    }

    /// Move the staged write files over their final paths. Called on
    /// successful close only; on failure the staged files are left in
    /// place for [`Drop`] to clean up.
    fn commit_pending(&mut self) -> RarResult<()> {
        let Some(pending) = self.pending.take() else {
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
                self.pending = Some(pending);
                Err(e)
            }
        }
    }

    /// Append the `"RR"` service header with parity over the archive
    /// prefix read from `prefix_path` (the file being written: the archive
    /// itself in append mode, the replacement file during a rewrite).
    fn write_recovery_record_from(&mut self, prefix_path: &Path) -> RarResult<()> {
        let percent = self.recovery_percent.unwrap_or(0) as u64;
        let stream = self.stream.as_mut().unwrap();
        let archive_size = stream.stream_position()?;
        if archive_size > MAX_RECOVERY_PREFIX_BYTES {
            return Err(RarError::LimitExceeded {
                limit: MAX_RECOVERY_PREFIX_BYTES,
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
            crate::recovery::rar5::build_structural_inline_recovery_data(&prefix, percent)
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
    fn write_quick_open_record(&mut self) -> RarResult<u64> {
        let stream = self.stream.as_mut().unwrap();
        let qo_pos = stream.stream_position()?;

        let mut payload = Vec::new();
        for (offset, header) in &self.quick_open_entries {
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
    fn patch_main_header_locator(
        &mut self,
        qo_offset: Option<u64>,
        rr_offset: Option<u64>,
    ) -> RarResult<()> {
        let start = self
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
        if let Some(qo) = qo_offset {
            let field = self.qo_offset_field_pos.ok_or_else(|| {
                RarError::Format("quick-open locator field position unknown".into())
            })? as usize;
            let base = self.sfx_offset + RAR5_SIGNATURE.len() as u64;
            let patched = vint_fixed5(qo.saturating_sub(base));
            if field + patched.len() > new_header.len() {
                return Err(RarError::Format("locator field out of bounds".into()));
            }
            new_header[field..field + patched.len()].copy_from_slice(&patched);
        }
        if let Some(rr) = rr_offset {
            let field = self
                .rr_offset_field_pos
                .ok_or_else(|| RarError::Format("recovery locator field position unknown".into()))?
                as usize;
            let base = self.sfx_offset + RAR5_SIGNATURE.len() as u64;
            let patched = vint_fixed5(rr.saturating_sub(base));
            if field + patched.len() > new_header.len() {
                return Err(RarError::Format("locator field out of bounds".into()));
            }
            new_header[field..field + patched.len()].copy_from_slice(&patched);
        }
        // Recompute the header CRC (covers from the size field onwards).
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&new_header[4..]);
        let crc = hasher.finalize();
        new_header[..4].copy_from_slice(&crc.to_le_bytes());

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

    // ── Signature ──────────────────────────────────────────────────────────

    fn write_signature(&mut self) -> RarResult<()> {
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(RAR5_SIGNATURE)?;
        Ok(())
    }

    // ── Writing ────────────────────────────────────────────────────────────

    fn write_archive_header(&mut self) -> RarResult<()> {
        if self.recovery_percent.is_some() || self.quick_open {
            return self.write_archive_header_with_locators();
        }
        let hdr = ArchiveHeader {
            flags: if self.solid_mode {
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
    fn write_archive_header_with_locators(&mut self) -> RarResult<()> {
        // Locator record body: [flags vint][qo offset vint][rr offset vint]
        // (only the offsets whose flags are set).
        const LOCATOR_TYPE: u64 = 0x01;
        const LOCATOR_FLAG_QUICK_OPEN: u64 = 0x0001;
        const LOCATOR_FLAG_RECOVERY: u64 = 0x0002;

        let mut locator = Vec::new();
        let mut locator_flags = 0u64;
        if self.quick_open {
            locator_flags |= LOCATOR_FLAG_QUICK_OPEN;
        }
        if self.recovery_percent.is_some() {
            locator_flags |= LOCATOR_FLAG_RECOVERY;
        }
        locator.extend(vint::encode(locator_flags));
        let qo_field_pos = if self.quick_open {
            let p = locator.len();
            locator.extend_from_slice(&vint_fixed5(0));
            Some(p)
        } else {
            None
        };
        let rr_field_pos = if self.recovery_percent.is_some() {
            let p = locator.len();
            locator.extend_from_slice(&vint_fixed5(0));
            Some(p)
        } else {
            None
        };

        let mut extra = Vec::new();
        extra.extend(vint::encode(locator.len() as u64)); // record size
        extra.extend(vint::encode(LOCATOR_TYPE));
        extra.extend(&locator);

        let mut arch_flags = 0u64;
        if self.recovery_percent.is_some() {
            arch_flags |= ARCHIVE_FLAG_RECOVERY;
        }
        if self.solid_mode {
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
        self.main_header_start = Some(main_header_start);
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
            + vint::encoded_size(LOCATOR_TYPE) as u64;
        if let Some(p) = qo_field_pos {
            self.qo_offset_field_pos = Some(field_base + p as u64);
        }
        if let Some(p) = rr_field_pos {
            self.rr_offset_field_pos = Some(field_base + p as u64);
        }
        Ok(())
    }

    fn write_archive_header_vol(&mut self, volume_number: Option<u64>) -> RarResult<()> {
        let hdr = ArchiveHeader {
            flags: ARCHIVE_FLAG_VOLUME,
            extra_data: Vec::new(),
            volume_number,
        };
        let hdr_bytes = hdr.to_bytes();
        self.write_block_header(&hdr_bytes)
    }

    fn write_end_block(&mut self) -> RarResult<()> {
        self.write_end_block_flags(false)
    }

    fn write_end_block_flags(&mut self, next_volume: bool) -> RarResult<()> {
        let flags = if next_volume { END_FLAG_NEXT_VOLUME } else { 0 };
        let eoa = EndOfArchiveHeader { flags };
        let hdr_bytes = eoa.to_bytes();
        self.write_block_header(&hdr_bytes)
    }

    pub(crate) fn start_next_volume(&mut self) -> RarResult<()> {
        self.write_end_block_flags(true)?;
        // Close current volume
        self.stream = None;
        self.current_volume += 1;
        let (parent, tmp_base, final_base) = match &self.pending {
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
        let tmp_vol = volume_path(&parent, &tmp_base, self.current_volume);
        let final_vol = volume_path(&parent, &final_base, self.current_volume);
        self.volume_paths.push(final_vol);
        let f = read_write_create(&tmp_vol)?;
        self.stream = Some(Box::new(f));
        self.write_signature()?;
        // Header-encrypted multi-volume sets repeat the plaintext encryption
        // header on every volume (WinRAR convention); the archive params are
        // generated once and shared across volumes.
        self.write_archive_encryption_header_if_needed()?;
        // Volume number: part2 → 1, part3 → 2, etc.
        let vol_num = (self.current_volume - 1) as u64;
        self.write_archive_header_vol(Some(vol_num))?;
        self.volume_bytes_written = self.stream.as_mut().unwrap().stream_position()?;
        Ok(())
    }
}

impl Drop for RarArchive {
    fn drop(&mut self) {
        let _ = self.close();
        // `close` may have failed (or never been reached); remove any
        // staged files that were not committed so a failed or interrupted
        // write leaves no garbage behind and never a partial archive at
        // the final path.
        if let Some(pending) = self.pending.take() {
            pending.cleanup(self.volume_paths.len());
        }
    }
}

/// Discover all volumes of a multi-volume RAR5 archive.
///
/// Given any volume path, returns a sorted list of all volume paths
/// starting from part1. Uses `.partN.rar` naming convention.
pub fn discover_volumes(path: &Path) -> Vec<PathBuf> {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => return vec![path.to_path_buf()],
    };

    // Match .partN.rar naming (zero-padded or not; WinRAR pads to the
    // digit count of the total volume count, e.g. part01..part15).
    if let Some((base, width)) = extract_volume_base(&name) {
        let parent = path.parent().unwrap_or(Path::new("."));
        let mut volumes = Vec::new();
        let mut n = 1u64;
        loop {
            let vol = parent.join(if width > 1 {
                format!("{base}.part{:0width$}.rar", n, width = width)
            } else {
                format!("{base}.part{n}.rar")
            });
            if vol.exists() {
                volumes.push(vol);
                n += 1;
            } else {
                break;
            }
        }
        if !volumes.is_empty() {
            return volumes;
        }
        // Fall back to the unpadded enumeration (mixed/odd sets).
        let mut n = 1u64;
        loop {
            let vol = parent.join(format!("{base}.part{n}.rar"));
            if vol.exists() {
                volumes.push(vol);
                n += 1;
            } else {
                break;
            }
        }
        if !volumes.is_empty() {
            return volumes;
        }
    }

    // Check if path itself names a single-volume file that has a .part1.rar sibling
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        let parent = path.parent().unwrap_or(Path::new("."));
        let part1 = parent.join(format!("{stem}.part1.rar"));
        if part1.exists() && part1 != path {
            return discover_volumes(&part1);
        }
        // Also probe zero-padded first volumes ({stem}.part01.rar ..
        // part0001.rar): sets written with 10+ volumes now carry the
        // padding themselves, and a caller may pass the base name.
        for width in 2..=4 {
            let probe = parent.join(format!("{stem}.part{:0width$}.rar", 1, width = width));
            if probe.exists() && probe != path {
                return discover_volumes(&probe);
            }
        }
    }

    vec![path.to_path_buf()]
}

/// Extract volume base from a filename like `archive.part3.rar` → `archive`.
/// Extract the volume base and the zero-padding width of the part number
/// from a name like `archive.part3.rar` → `("archive", 1)` or
/// `archive.part03.rar` → `("archive", 2)`. WinRAR pads the number to the
/// digit count of the total volume count (part01..part15), so both forms
/// must be discoverable.
fn extract_volume_base(name: &str) -> Option<(String, usize)> {
    // Case-insensitive match for .partN.rar
    let lower = name.to_lowercase();
    if let Some(idx) = lower.find(".part") {
        let after = &lower[idx + 5..];
        if let Some(rar_idx) = after.find(".rar") {
            let num_str = &after[..rar_idx];
            if num_str.chars().all(|c| c.is_ascii_digit()) && !num_str.is_empty() {
                return Some((name[..idx].to_string(), num_str.len()));
            }
        }
    }
    None
}

/// Volume base of an archive path, stripping `.partN.rar` or `.rar`
/// suffixes (used by the recovery-volume machinery).
pub(crate) fn volume_base_of(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive");
    if let Some((base, _)) = extract_volume_base(name) {
        return base;
    }
    if let Some(stem) = name.strip_suffix(".rar") {
        return stem.to_string();
    }
    if let Some(stem) = name.strip_suffix(".RAR") {
        return stem.to_string();
    }
    name.to_string()
}

/// Zero-padding width of the part number in a volume name
/// (`archive.part03.rar` → 2, `archive.part3.rar` → 1). Used to name
/// `.rev` files identically to their volume set.
pub(crate) fn volume_part_width(path: &Path) -> usize {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(extract_volume_base)
        .map(|(_, w)| w)
        .unwrap_or(1)
}

pub(crate) fn get_volume_base(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive");
    if let Some((base, _)) = extract_volume_base(name) {
        return base;
    }
    if let Some(stem) = name.strip_suffix(".rar") {
        return stem.to_string();
    }
    if let Some(stem) = name.strip_suffix(".RAR") {
        return stem.to_string();
    }
    name.to_string()
}

fn volume_path(parent: &Path, base: &str, part_num: usize) -> PathBuf {
    parent.join(format!("{base}.part{part_num}.rar"))
}

/// Volume path with the part number zero-padded to `width` digits
/// (`part01.rar` for width 2), matching WinRAR's naming for sets of 10
/// or more volumes.
fn volume_path_padded(parent: &Path, base: &str, part_num: usize, width: usize) -> PathBuf {
    parent.join(format!("{base}.part{part_num:0width$}.rar"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rar50::extract::sanitize_archive_path;

    #[test]
    fn encrypted_store_roundtrip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("rar");
        let data = b"Hello, encrypted world!";
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    password: Some("secret".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("test.txt", data, 0).unwrap();
            ar.close().unwrap();
        }
        {
            let mut ar = RarArchive::open_with_password(&path, "secret").unwrap();
            assert_eq!(ar.read("test.txt").unwrap(), data);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn encrypted_compressed_roundtrip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("rar");
        let data = b"Compress me! ".repeat(200);
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    password: Some("pw".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("test.txt", &data, 3).unwrap();
            ar.close().unwrap();
        }
        {
            let mut ar = RarArchive::open_with_password(&path, "pw").unwrap();
            assert_eq!(ar.read("test.txt").unwrap(), data);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn encrypted_wrong_password_fails() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("rar");
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    password: Some("right".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("test.txt", b"data", 0).unwrap();
            ar.close().unwrap();
        }
        {
            let mut ar = RarArchive::open_with_password(&path, "wrong").unwrap();
            assert!(ar.read("test.txt").is_err());
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn encrypted_multiple_files() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("rar");
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    password: Some("multi".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("a.txt", b"First", 0).unwrap();
            ar.add_bytes("b.txt", &b"Second ".repeat(50), 3).unwrap();
            ar.add_bytes("c.bin", &(0..=255u8).collect::<Vec<_>>(), 0)
                .unwrap();
            ar.close().unwrap();
        }
        {
            let mut ar = RarArchive::open_with_password(&path, "multi").unwrap();
            assert_eq!(ar.read("a.txt").unwrap(), b"First");
            assert_eq!(ar.read("b.txt").unwrap(), b"Second ".repeat(50));
            assert_eq!(ar.read("c.bin").unwrap(), (0..=255u8).collect::<Vec<_>>());
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn recovery_volume_count_is_capped_at_data_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("vol.part1.rar");
        let data = b"volume recovery payload ".repeat(4000); // ~100 KiB
        {
            // Ask for 10 .rev files; the archive only splits into 3
            // volumes, so exactly 3 .rev files must be produced.
            let mut ar = RarArchive::create_with_options(
                &base,
                crate::options::CreateOptions {
                    volume_size: Some(32768),
                    recovery_volume_count: Some(10),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("big.bin", &data, 0).unwrap();
            ar.close().unwrap();
        }
        let revs: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| {
                let p = e.unwrap().path();
                (p.extension().is_some_and(|e| e == "rev")).then_some(p)
            })
            .collect();
        let volumes: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| {
                let p = e.unwrap().path();
                (p.extension().is_some_and(|e| e == "rar")).then_some(p)
            })
            .collect();
        assert_eq!(volumes.len(), 3, "expected 3 data volumes");
        assert_eq!(
            revs.len(),
            3,
            "recovery volume count must be capped at data volumes"
        );
        std::fs::remove_dir_all(dir.path()).ok();
    }

    #[test]
    fn recovery_volume_exact_count_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("vol.part1.rar");
        let data = b"volume recovery payload ".repeat(4000);
        {
            let mut ar = RarArchive::create_with_options(
                &base,
                crate::options::CreateOptions {
                    volume_size: Some(32768),
                    recovery_volume_count: Some(2),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("big.bin", &data, 0).unwrap();
            ar.close().unwrap();
        }
        let revs: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| {
                let p = e.unwrap().path();
                (p.extension().is_some_and(|e| e == "rev")).then_some(p)
            })
            .collect();
        assert_eq!(revs.len(), 2, "expected exactly 2 .rev files");
        std::fs::remove_dir_all(dir.path()).ok();
    }

    #[test]
    fn recovery_volumes_roundtrip_and_repair() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("vol.part1.rar");
        let data = b"volume recovery payload ".repeat(4000); // ~100 KiB
        {
            let mut ar = RarArchive::create_with_options(
                &base,
                crate::options::CreateOptions {
                    volume_size: Some(32768),
                    recovery_volumes_percent: Some(20),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("big.bin", &data, 0).unwrap();
            ar.close().unwrap();
        }
        // Volumes + at least one .rev file must exist.
        let dir_entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let mut volume_paths: Vec<_> = dir_entries
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "rar"))
            .cloned()
            .collect();
        volume_paths.sort();
        let mut volumes: Vec<Vec<u8>> = volume_paths
            .iter()
            .map(|p| std::fs::read(p).unwrap())
            .collect();
        let revs: Vec<_> = dir_entries
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "rev"))
            .collect();
        assert!(
            volumes.len() >= 3,
            "expected split volumes, got {}",
            volumes.len()
        );
        assert_eq!(revs.len(), 1, "expected one .rev file");

        // The .rev must carry the REV5 signature, the volume table and a
        // payload whose CRC32 matches the header field.
        let rev = std::fs::read(revs[0]).unwrap();
        assert!(rev.starts_with(b"Rar!\x1aRev"));
        let header_size = u32::from_le_bytes(rev[12..16].try_into().unwrap()) as usize;
        let body = &rev[16..16 + header_size];
        let data_count = u16::from_le_bytes(body[1..3].try_into().unwrap()) as usize;
        assert_eq!(data_count, volumes.len());
        let payload = &rev[16 + header_size..];
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(payload);
        let payload_crc = u32::from_le_bytes(body[7..11].try_into().unwrap());
        assert_eq!(hasher.finalize(), payload_crc, ".rev payload CRC mismatch");
        for (i, vol) in volumes.iter().enumerate() {
            let mut h = crc32fast::Hasher::new();
            h.update(vol);
            let table_crc =
                u32::from_le_bytes(body[11 + i * 12 + 8..11 + i * 12 + 12].try_into().unwrap());
            assert_eq!(
                h.finalize(),
                table_crc,
                ".rev volume table CRC mismatch for volume {i}"
            );
        }

        // Reconstruct a missing middle volume from the .rev parity and the
        // remaining volumes (WinRAR `rc` equivalent).
        let missing = volumes.len() / 2;
        let expected = volumes[missing].clone();
        volumes.remove(missing);

        let maxlen = volumes.iter().map(|v| v.len()).max().unwrap_or(0);
        let maxlen = if maxlen % 2 == 0 { maxlen } else { maxlen + 1 };
        let mut padded: Vec<Vec<u8>> = volumes
            .iter()
            .map(|v| {
                let mut x = v.clone();
                x.resize(maxlen, 0);
                x
            })
            .collect();
        padded.insert(missing, payload.to_vec());

        let gf = crate::recovery::rar5::shared_gf16();
        let matrix = crate::recovery::rar5::make_encoder_matrix(padded.len(), 1).unwrap();
        let mut rebuilt = vec![0u8; maxlen];
        let denom = matrix[0][missing];
        for off in (0..maxlen).step_by(2) {
            let mut symbol = 0u16;
            for (i, shard) in padded.iter().enumerate() {
                let v = u16::from_le_bytes([shard[off], shard[off + 1]]);
                if i == missing {
                    // The parity shard participates with coefficient 1.
                    symbol ^= v;
                } else {
                    symbol ^= gf.mul(matrix[0][i], v);
                }
            }
            let v = gf.div(symbol, denom).unwrap();
            rebuilt[off..off + 2].copy_from_slice(&v.to_le_bytes());
        }
        rebuilt.truncate(expected.len());
        assert_eq!(rebuilt, expected, "reconstructed volume must match");
        std::fs::remove_dir_all(dir.path()).ok();
    }

    #[test]
    fn recovery_record_roundtrip_and_repair() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("rar");
        let data = b"recovery test payload ".repeat(1000);
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    recovery_percent: Some(5),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("a.bin", &data, 0).unwrap();
            ar.close().unwrap();
        }
        let raw = std::fs::read(&path).unwrap();
        // The RR service header (name "RR") and the {RB} shard magic must
        // be present, and the main header must carry the recovery flag.
        assert!(raw.windows(2).any(|w| w == b"RR"));
        assert!(raw.windows(4).any(|w| w == b"{RB}"));
        // The plaintext must not be touched by the recovery record.
        let mut ar = RarArchive::open(&path).unwrap();
        assert_eq!(ar.read("a.bin").unwrap(), data);

        // Damage bytes inside ONE data shard (NR parity shards can repair
        // up to NR damaged shards; the archive here is ~21 KiB → D=21,
        // NR=1).
        let mut damaged = raw.clone();
        for pos in [100usize, 200, 300] {
            damaged[pos] ^= 0xFF;
        }
        let repaired = crate::recovery::rar5::repair_inline_recovery_archive(&damaged).unwrap();
        assert_eq!(repaired, raw, "repair must restore the original bytes");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn recovery_record_relocates_damaged_shards_from_twin_file_blocks() {
        // Two members with identical content pack byte-identically, so a
        // damaged shard inside one member's data block can be relocated
        // from the twin block even when the damage spans more shards than
        // the recovery record can correct (NR=1, damage covers 2 shards).
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("rar");
        let data = b"twin payload for relocated repair ".repeat(1000);
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    recovery_percent: Some(5),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("a.bin", &data, 0).unwrap();
            ar.add_bytes("b.bin", &data, 0).unwrap();
            ar.close().unwrap();
        }
        let raw = std::fs::read(&path).unwrap();
        let m = raw
            .windows(4)
            .position(|w| w == b"{RB}")
            .expect("recovery record present");
        let ds = u16::from_le_bytes([raw[m + 0x3a], raw[m + 0x3b]]) as usize;
        let gc = u64::from_le_bytes(raw[m + 0x2a..m + 0x32].try_into().unwrap()) as usize;
        assert!(ds >= 4, "expected a multi-shard archive, got {ds}");
        let b_name = raw
            .windows(5)
            .position(|w| w == b"b.bin")
            .expect("second member header");

        // Damage two complete shards that fall inside b.bin's data block
        // (their byte-identical copies survive in a.bin's block). The last
        // shard index is chosen so both damaged shards stay inside b.bin's
        // data area and never touch file headers.
        let last = (ds - 1).min(b_name + 5 + gc / 2);
        let s1 = last.saturating_sub(2) * gc;
        let s2 = (last.saturating_sub(1)) * gc;
        assert!(s1 >= b_name, "damaged shards must sit in b.bin data block");
        let mut damaged = raw.clone();
        for byte in damaged.iter_mut().take(s2 + gc).skip(s1) {
            *byte ^= 0xFF;
        }
        let repaired = crate::recovery::rar5::repair_inline_recovery_archive(&damaged).unwrap();
        assert_eq!(
            repaired, raw,
            "relocated repair must restore the original bytes"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn recovery_record_refuses_damage_without_twin_or_parity_capacity() {
        // Distinct member contents have no twin block; two damaged shards
        // exceed the single parity shard, so repair must refuse instead of
        // writing wrong bytes.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("rar");
        let a = b"alpha payload ".repeat(1000);
        let b = b"beta payload differs ".repeat(1000);
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    recovery_percent: Some(5),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("a.bin", &a, 0).unwrap();
            ar.add_bytes("b.bin", &b, 0).unwrap();
            ar.close().unwrap();
        }
        let raw = std::fs::read(&path).unwrap();
        let m = raw
            .windows(4)
            .position(|w| w == b"{RB}")
            .expect("recovery record present");
        let ds = u16::from_le_bytes([raw[m + 0x3a], raw[m + 0x3b]]) as usize;
        let gc = u64::from_le_bytes(raw[m + 0x2a..m + 0x32].try_into().unwrap()) as usize;
        let last = (ds - 1).min(10);
        let s1 = last.saturating_sub(2) * gc;
        let s2 = (last.saturating_sub(1)) * gc;
        let mut damaged = raw.clone();
        for byte in damaged.iter_mut().take(s2 + gc).skip(s1) {
            *byte ^= 0xFF;
        }
        assert!(
            crate::recovery::rar5::repair_inline_recovery_archive(&damaged).is_err(),
            "repair must refuse damage beyond parity capacity without a twin"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn recovery_record_with_password_and_headers() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("rar");
        let data = b"encrypted + recovery ".repeat(500);
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    password: Some("pw".into()),
                    encrypt_headers: true,
                    recovery_percent: Some(10),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("secret.bin", &data, 0).unwrap();
            ar.close().unwrap();
        }
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.windows(4).any(|w| w == b"{RB}"));
        let mut ar = RarArchive::open_with_password(&path, "pw").unwrap();
        assert_eq!(ar.read("secret.bin").unwrap(), data);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn header_encryption_roundtrip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("rar");
        let data = b"Hidden content!".repeat(100);
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    password: Some("hdr-pw".into()),
                    encrypt_headers: true,
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("secret/name.txt", &data, 3).unwrap();
            ar.close().unwrap();
        }
        // The raw archive must not contain the plaintext file name.
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !raw.windows(b"secret/name.txt".len())
                .any(|w| w == b"secret/name.txt")
        );
        {
            let mut ar = RarArchive::open_with_password(&path, "hdr-pw").unwrap();
            assert_eq!(ar.read("secret/name.txt").unwrap(), data);
        }
        // Wrong password must be rejected by the header check value.
        let err = RarArchive::open_with_password(&path, "nope").err();
        assert!(err.is_some());
        assert!(
            matches!(err, Some(RarError::WrongPassword)),
            "unexpected error: {err:?}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn header_encryption_requires_password() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("rar");
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    password: Some("pw".into()),
                    encrypt_headers: true,
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("a.txt", b"data", 0).unwrap();
            ar.close().unwrap();
        }
        // Opening without a password must fail: headers are encrypted.
        assert!(RarArchive::open(&path).is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn in_memory_sink_archive_is_well_formed() {
        // The stream seam: an archive written into a Cursor (no disk)
        // must be byte-valid — same envelope, quick-open and end blocks a
        // file archive would carry.
        use std::io::{Cursor, Read, Seek, SeekFrom};

        let opts = crate::options::CreateOptions {
            quick_open: true,
            ..Default::default()
        };
        let mut ar = RarArchive::create_with_sink(
            PathBuf::from("mem.rar"),
            opts,
            Box::new(Cursor::new(Vec::new())),
        )
        .unwrap();
        ar.add_bytes("a.txt", b"hello", 3).unwrap();
        ar.add_bytes("b.bin", &vec![7u8; 1000], 0).unwrap();
        let mut sink = ar.finish_into_sink().unwrap();
        sink.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        sink.read_to_end(&mut bytes).unwrap();

        // Structural scan through the same seam the archive scanner uses.
        let mut cursor = Cursor::new(&bytes);
        cursor.set_position(8); // skip signature
        let mut types = Vec::new();
        while let Ok(Some(meta)) = crate::rar50::headers::read_block(&mut cursor, None) {
            types.push(meta.block_type);
            cursor.set_position(meta.data_end);
            if meta.block_type == BLOCK_TYPE_END_ARCHIVE {
                break;
            }
        }
        assert_eq!(types.first(), Some(&BLOCK_TYPE_ARCHIVE_HEADER));
        assert!(types.contains(&BLOCK_TYPE_FILE_HEADER), "{types:?}");
        assert!(types.contains(&BLOCK_TYPE_SERVICE_HEADER), "{types:?}");
        assert_eq!(types.last(), Some(&BLOCK_TYPE_END_ARCHIVE), "{types:?}");

        // Persisted, the in-memory archive must open and read back.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.rar");
        std::fs::write(&path, &bytes).unwrap();
        let mut rar = RarArchive::open(&path).unwrap();
        assert_eq!(rar.read("a.txt").unwrap(), b"hello");
        assert_eq!(rar.read("b.bin").unwrap(), vec![7u8; 1000]);
    }

    #[test]
    fn multivolume_create_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mv.rar");

        // Generate test data (102400 bytes)
        let mut rng_data = vec![0u8; 102400];
        for (i, b) in rng_data.iter_mut().enumerate() {
            *b = (i.wrapping_mul(7) ^ (i >> 3)) as u8;
        }
        let small = b"Hello from multi-volume test\n";

        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    volume_size: Some(30000),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("big.bin", &rng_data, 0).unwrap();
            ar.add_bytes("small.txt", small, 0).unwrap();
            ar.close().unwrap();
        }

        // Verify volumes were created
        let vols = discover_volumes(&path);
        assert!(vols.len() > 1, "should create multiple volumes");

        // Read back
        {
            let mut ar = RarArchive::open(&vols[0]).unwrap();
            let entries = ar.list().to_vec();
            assert_eq!(entries.len(), 2);

            assert_eq!(ar.read("big.bin").unwrap(), rng_data);
            assert_eq!(ar.read("small.txt").unwrap(), small.to_vec());
        }
    }

    #[test]
    fn multivolume_create_compressed_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mvc.rar");

        let data = b"Compressible data pattern!\n".repeat(3000);
        let small = b"Tiny file";

        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    volume_size: Some(30000),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("data.txt", &data, 3).unwrap();
            ar.add_bytes("small.txt", small, 3).unwrap();
            ar.close().unwrap();
        }

        let vols = discover_volumes(&path);
        assert!(!vols.is_empty());

        {
            let mut ar = RarArchive::open(&vols[0]).unwrap();
            assert_eq!(ar.read("data.txt").unwrap(), data);
            assert_eq!(ar.read("small.txt").unwrap(), small.to_vec());
        }
    }

    #[test]
    fn header_encrypted_multivolume_self_roundtrip() {
        // Read-side support for -hp volume sets (WinRAR repeats the
        // plaintext encryption header on every volume; every block after it
        // is IV + AES-CBC). Covers both STORE and compressed members.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mvhp.rar");

        let store_data = (0..120_000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let comp_data = b"header encrypted volume payload ".repeat(4_000);

        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    volume_size: Some(30_000),
                    password: Some("pw".into()),
                    encrypt_headers: true,
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("store.bin", &store_data, 0).unwrap();
            ar.add_bytes("comp.bin", &comp_data, 3).unwrap();
            ar.close().unwrap();
        }

        let vols = discover_volumes(&path);
        assert!(vols.len() > 1, "precondition: multiple volumes");

        {
            let mut ar = RarArchive::open_with_password(&vols[0], "pw").unwrap();
            assert_eq!(ar.namelist(), ["store.bin", "comp.bin"]);
            assert_eq!(ar.read("store.bin").unwrap(), store_data);
            assert_eq!(ar.read("comp.bin").unwrap(), comp_data);
        }
        // Wrong password must be rejected.
        assert!(RarArchive::open_with_password(&vols[0], "nope").is_err());
    }

    #[test]
    fn multivolume_discover_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disc.rar");

        let data = vec![0u8; 50000];
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    volume_size: Some(20000),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("data.bin", &data, 0).unwrap();
            ar.close().unwrap();
        }

        // Discover from part1
        let vols = discover_volumes(&dir.path().join("disc.part1.rar"));
        assert!(vols.len() > 1);

        // Discover from part2
        let vols2 = discover_volumes(&dir.path().join("disc.part2.rar"));
        assert_eq!(vols2.len(), vols.len());
        assert_eq!(
            vols2[0].file_name().unwrap().to_str().unwrap(),
            "disc.part1.rar"
        );
    }

    #[test]
    fn multivolume_open_from_any_part() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anypart.rar");

        let data = vec![42u8; 80000];
        {
            let mut ar = RarArchive::create_with_options(
                &path,
                crate::options::CreateOptions {
                    volume_size: Some(30000),
                    ..Default::default()
                },
            )
            .unwrap();
            ar.add_bytes("data.bin", &data, 0).unwrap();
            ar.close().unwrap();
        }

        // Open from part2
        let part2 = dir.path().join("anypart.part2.rar");
        let mut ar = RarArchive::open(&part2).unwrap();
        assert_eq!(ar.read("data.bin").unwrap(), data);
    }

    #[test]
    fn sanitize_archive_path_rejects_unsafe_names() {
        for bad in [
            "",
            "../evil",
            "a/../../b",
            "/etc/passwd",
            "//server/share",
            "C:/windows",
            "c:\\windows",
            "file.txt\0",
            ".",
            "./",
        ] {
            assert!(
                sanitize_archive_path(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn sanitize_archive_path_normalizes_safe_names() {
        assert_eq!(sanitize_archive_path("a/b.txt").unwrap(), "a/b.txt");
        assert_eq!(sanitize_archive_path("a\\b.txt").unwrap(), "a/b.txt");
        assert_eq!(
            sanitize_archive_path("./a//b/./c.txt").unwrap(),
            "a/b/c.txt"
        );
        assert_eq!(sanitize_archive_path("dir/").unwrap(), "dir");
    }

    /// Names of leftover staging files (`.rar5tmp-*`) in `dir`.
    fn temp_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("rar5tmp"))
            .collect()
    }

    #[test]
    fn create_is_not_visible_until_close_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.rar");
        let mut ar =
            RarArchive::create_with_options(&path, crate::options::CreateOptions::default())
                .unwrap();
        ar.add_bytes("a.txt", b"data", 0).unwrap();
        // Creation is staged: nothing appears at the target path until close.
        assert!(!path.exists());
        ar.close().unwrap();
        assert!(path.exists());
        assert!(temp_leftovers(dir.path()).is_empty());
        let mut rar = RarArchive::open(&path).unwrap();
        assert_eq!(rar.read("a.txt").unwrap(), b"data");
    }

    #[test]
    fn dropped_write_is_finalized_and_committed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.rar");
        {
            let mut ar =
                RarArchive::create_with_options(&path, crate::options::CreateOptions::default())
                    .unwrap();
            ar.add_bytes("a.txt", b"data", 0).unwrap();
        }
        assert!(path.exists());
        assert!(temp_leftovers(dir.path()).is_empty());
        let mut rar = RarArchive::open(&path).unwrap();
        assert_eq!(rar.read("a.txt").unwrap(), b"data");
    }

    #[test]
    fn failed_commit_leaves_no_archive_or_temp() {
        let dir = tempfile::tempdir().unwrap();
        // A directory at the target path: the final rename must fail.
        let target = dir.path().join("t.rar");
        std::fs::create_dir(&target).unwrap();
        {
            let mut ar =
                RarArchive::create_with_options(&target, crate::options::CreateOptions::default())
                    .unwrap();
            ar.add_bytes("a.txt", b"data", 0).unwrap();
            assert!(ar.close().is_err());
        }
        // The target is untouched and the staged temp file was cleaned up.
        assert!(target.is_dir());
        assert!(temp_leftovers(dir.path()).is_empty());
    }

    #[test]
    fn append_keeps_original_untouched_until_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.rar");
        let original = {
            let mut ar =
                RarArchive::create_with_options(&path, crate::options::CreateOptions::default())
                    .unwrap();
            ar.add_bytes("a.txt", b"original", 0).unwrap();
            ar.close().unwrap();
            std::fs::read(&path).unwrap()
        };
        {
            let mut ar = RarArchive::open_append(&path).unwrap();
            // The append is staged: the original file stays byte-identical
            // while the append is in progress.
            assert_eq!(std::fs::read(&path).unwrap(), original);
            ar.add_bytes("b.txt", b"appended", 0).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), original);
            ar.close().unwrap();
        }
        assert_ne!(std::fs::read(&path).unwrap(), original);
        assert!(temp_leftovers(dir.path()).is_empty());
        let mut rar = RarArchive::open(&path).unwrap();
        assert_eq!(rar.read("a.txt").unwrap(), b"original");
        assert_eq!(rar.read("b.txt").unwrap(), b"appended");
    }

    #[test]
    fn multivolume_creation_stages_volumes_until_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mv.rar");
        let data = vec![42u8; 80000];
        let mut ar = RarArchive::create_with_options(
            &path,
            crate::options::CreateOptions {
                volume_size: Some(30000),
                ..Default::default()
            },
        )
        .unwrap();
        ar.add_bytes("data.bin", &data, 0).unwrap();
        // Volumes are staged under a temporary base: no final volume exists
        // until close.
        assert!(!dir.path().join("mv.part1.rar").exists());
        assert!(!temp_leftovers(dir.path()).is_empty());
        ar.close().unwrap();
        assert!(dir.path().join("mv.part1.rar").exists());
        assert!(dir.path().join("mv.part2.rar").exists());
        assert!(temp_leftovers(dir.path()).is_empty());
        let mut rar = RarArchive::open(&path).unwrap();
        assert_eq!(rar.read("data.bin").unwrap(), data);
    }
}
