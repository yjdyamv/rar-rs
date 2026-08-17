/// RarArchive — high-level RAR5 archive interface.
///
/// Supports opening existing archives for reading/extraction and creating
/// new archives from scratch.
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::codec::DecoderState;
use crate::compression;
use crate::constants::*;
use crate::encryption::{self, parse_archive_encrypt_header};
use crate::error::{RarError, RarResult};
use crate::headers::*;
use crate::vint;

/// Write pipeline (member creation, batch addition, streaming payload
/// writer) in a sibling impl block.
#[path = "write.rs"]
mod write;
/// Surgical rewrite pipeline (delete/rename/comment/recovery mutation) in
/// a sibling impl block.
#[path = "rewrite.rs"]
mod rewrite;

/// Maximum archive prefix buffered for inline recovery-record parity.
/// Streamed recovery records are not implemented yet; larger archives must
/// create recovery records without `recovery_percent`.
const MAX_RECOVERY_PREFIX_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// Maximum accepted RAR5 dictionary-size log (4 GiB, the RAR5 format
/// ceiling; WinRAR 7.23 accepts the same range — larger, non-power-of-two
/// dictionaries only exist in the RAR7 format, which is out of scope).
/// Larger values are rejected at decode time to bound window allocations.
const MAX_DICT_SIZE_LOG: u8 = 15;

/// Memory budget (packed + unpacked) for the optional parallel extraction
/// path; larger archives stream sequentially to stay bounded.
#[cfg(feature = "parallel")]
const PARALLEL_BUFFER_LIMIT: u64 = 256 * 1024 * 1024;
/// Parallel extraction only engages for at least this many members...
#[cfg(feature = "parallel")]
const PARALLEL_MIN_MEMBERS: usize = 4;
/// ...and at least this much total unpacked data (Rayon overhead amortized).
#[cfg(feature = "parallel")]
const PARALLEL_MIN_UNPACKED: u64 = 64 * 1024 * 1024;
/// Parallel batch compression (feature `parallel`): members up to this
/// size are compressed whole in Rayon waves; larger non-solid files are
/// compressed in parallel chunks with bounded memory.
#[cfg(feature = "parallel")]
const PARALLEL_COMPRESS_MAX_MEMBER: u64 = 64 * 1024 * 1024;
/// Members at least this large take the streaming compressed path in
/// [`RarArchive::add_file`]: input is compressed in bounded chunks into a
/// temporary spill file and then streamed into the archive, so memory
/// stays bounded for any file size (P4: >4 GiB single-file creation).
const STREAM_COMPRESS_THRESHOLD: u64 = 64 * 1024 * 1024;
/// Total input bytes buffered per parallel compression wave (feature
/// `parallel`).
#[cfg(feature = "parallel")]
const PARALLEL_COMPRESS_WAVE_BUDGET: u64 = 256 * 1024 * 1024;

/// Compression thread count set with [`set_compression_threads`]
/// (like `rar -mt`); 0 = automatic sizing.
static COMPRESSION_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Extraction thread count set with [`set_extraction_threads`];
/// 0 = automatic sizing.
static EXTRACTION_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Set the compression thread count used by the `parallel` feature
/// (like `rar -mt<N>`). `0` restores automatic sizing.
pub fn set_compression_threads(threads: usize) {
    COMPRESSION_THREADS.store(threads, std::sync::atomic::Ordering::Relaxed);
}

/// Set the extraction thread count used by the `parallel` feature.
/// `0` restores automatic sizing.
pub fn set_extraction_threads(threads: usize) {
    EXTRACTION_THREADS.store(threads, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(feature = "parallel")]
fn configured_threads() -> Option<usize> {
    let n = COMPRESSION_THREADS.load(std::sync::atomic::Ordering::Relaxed);
    (n > 0).then_some(n)
}

#[cfg(feature = "parallel")]
fn configured_extraction_threads() -> Option<usize> {
    let n = EXTRACTION_THREADS.load(std::sync::atomic::Ordering::Relaxed);
    (n > 0).then_some(n)
}

/// Dedicated Rayon pool for batch compression.
///
/// The global pool (16 threads on this class of machine) makes many small
/// members *slower*: per-task allocation contention and SMT scheduling
/// overhead dominate tiny jobs. A small dedicated pool (at most 4 threads,
/// fewer on low-core machines) keeps the parallel win for medium/large
/// members without the small-member regression.
#[cfg(feature = "parallel")]
fn pool_threads(default: usize) -> usize {
    #[cfg(not(target_family = "wasm"))]
    {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(default)
    }
    // WASM cannot query the host CPU count (WASI reports 1 core), so follow
    // the emnapi worker-pool sizing and let the host override explicitly.
    // Precedence: SA_RAR5_WASM_WORKERS > NAPI_RS_ASYNC_WORK_POOL_SIZE >
    // UV_THREADPOOL_SIZE > `default`. The extension sets SA_RAR5_WASM_WORKERS
    // from Node's os.availableParallelism() so the encoder uses every core.
    #[cfg(target_family = "wasm")]
    {
        let from_env = |key: &str| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
        };
        from_env("SA_RAR5_WASM_WORKERS")
            .or_else(|| from_env("NAPI_RS_ASYNC_WORK_POOL_SIZE"))
            .or_else(|| from_env("UV_THREADPOOL_SIZE"))
            .unwrap_or(default)
            .max(1)
    }
}

/// Dedicated Rayon pool for batch compression.
#[cfg(feature = "parallel")]
fn compression_pool() -> &'static rayon::ThreadPool {
    use std::sync::OnceLock;

    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = configured_threads().unwrap_or_else(|| pool_threads(4).min(4));
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("rar5-compress-{i}"))
            .build()
            .expect("build rar5 compression pool")
    })
}

/// Dedicated Rayon pool for large-file chunk compression.
///
/// Large members are CPU-bound and benefit from every available core,
/// while the member-level wave pool stays at 4 threads to avoid regressions
/// on many small members (measurement: 10k x 4 KiB files are ~3x slower at
/// 16 threads than at 4).
#[cfg(feature = "parallel")]
fn large_file_pool() -> &'static rayon::ThreadPool {
    use std::sync::OnceLock;

    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = configured_threads().unwrap_or_else(|| pool_threads(4));
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("rar5-large-{i}"))
            .build()
            .expect("build rar5 large-file compression pool")
    })
}

/// Rayon pool for parallel extraction, sized with
/// [`set_extraction_threads`] (default: all cores).
#[cfg(feature = "parallel")]
fn extraction_pool() -> &'static rayon::ThreadPool {
    use std::sync::OnceLock;

    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = configured_extraction_threads().unwrap_or_else(|| pool_threads(4));
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("rar5-extract-{i}"))
            .build()
            .expect("build rar5 extraction pool")
    })
}

// Set while a Rayon worker is preparing batch members. Nested parallelism
// (filter candidate probing, BLAKE2sp leaves) is disabled for small members
// on these threads: workers already parallelize across members, and nested
// tasks oversubscribe the pool.
#[cfg(feature = "parallel")]
thread_local! {
    static IN_BATCH_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(feature = "parallel")]
pub(crate) fn in_batch_worker() -> bool {
    IN_BATCH_WORKER.with(|flag| flag.get())
}

#[cfg(feature = "parallel")]
struct BatchWorkerGuard;

#[cfg(feature = "parallel")]
impl BatchWorkerGuard {
    fn new() -> Self {
        IN_BATCH_WORKER.with(|flag| flag.set(true));
        BatchWorkerGuard
    }
}

#[cfg(feature = "parallel")]
impl Drop for BatchWorkerGuard {
    fn drop(&mut self) {
        IN_BATCH_WORKER.with(|flag| flag.set(false));
    }
}

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
struct PreparedEntry {
    name: String,
    unpacked_size: u64,
    attrs: u64,
    mtime: u32,
    file_crc: u32,
    method: u8,
    dict_size_log: u8,
    dict_size_bytes: Option<u64>,
    extra_data: Vec<u8>,
    stored_hash: Option<[u8; 32]>,
    payload: Vec<u8>,
}

/// Immutable snapshot of the writer settings needed to prepare a member
/// off-thread. `Sync`-safe where `&RarArchive` is not (the progress
/// callback is a `FnMut` trait object).
#[cfg(feature = "parallel")]
struct BatchPrepareCtx<'a> {
    password: Option<&'a str>,
    blake2: bool,
    dict_size_log: Option<u8>,
    dict_size_bytes: Option<u64>,
    save_ctime: bool,
    save_atime: bool,
    save_mtime: bool,
    save_owner: bool,
    time_precision_seconds: bool,
}

/// Decrypted member payload plus the key material needed for integrity
/// verification.
struct DecryptedPayload {
    data: Vec<u8>,
    params: Option<encryption::EncryptionParams>,
    keys: Option<encryption::DerivedKeys>,
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

/// Write sink that computes CRC32 and optional BLAKE2sp over streamed
/// output.
struct IntegritySink<'a> {
    inner: &'a mut dyn Write,
    crc: crc32fast::Hasher,
    blake: Option<crate::blake2sp::Hasher>,
}

impl<'a> IntegritySink<'a> {
    fn new(inner: &'a mut dyn Write, want_blake: bool) -> Self {
        Self {
            inner,
            crc: crc32fast::Hasher::new(),
            blake: want_blake.then(crate::blake2sp::Hasher::new),
        }
    }

    fn finish(self) -> (u32, Option<[u8; 32]>) {
        (self.crc.finalize(), self.blake.map(|h| h.finalize()))
    }
}

impl Write for IntegritySink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        if let Some(h) = self.blake.as_mut() {
            h.update(&buf[..n]);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl ArchiveEntry {
    pub fn name(&self) -> &str {
        &self.header.name
    }

    pub fn size(&self) -> u64 {
        self.header.unpacked_size
    }

    pub fn compressed_size(&self) -> u64 {
        self.header.packed_size
    }

    pub fn is_dir(&self) -> bool {
        self.header.is_directory
    }

    pub fn crc32(&self) -> Option<u32> {
        self.header.crc32_val
    }

    pub fn method_name(&self) -> &'static str {
        method_name(self.header.comp_method)
    }
}

/// RAR5 archive reader/writer.
pub struct RarArchive {
    path: PathBuf,
    mode: Mode,
    entries: Vec<ArchiveEntry>,
    /// The archive stream: a file, or any caller-provided seekable
    /// read/write sink (in-memory `Cursor` in tests, stdin/stdout for
    /// future `-si` support).
    stream: Option<Box<dyn ArchiveStream>>,
    /// Byte offset where the RAR5 signature begins (0 for plain archives,
    /// >0 for SFX archives whose stub precedes the archive).
    sfx_offset: u64,
    /// Persistent decoder state for RAR5 solid archive chains.
    solid_state: Option<DecoderState>,
    /// Index of the last file decoded in the solid chain (-1 = none).
    solid_decoded_through: isize,
    /// Password for encrypted archives.
    password: Option<String>,
    /// Encrypt archive headers (file names/structure hidden) — RAR5
    /// archive-level encryption header + AES-256-CBC per-block headers.
    header_encryption: bool,
    /// Archive-level encryption parameters when header encryption is on.
    archive_encr: Option<encryption::EncryptionParams>,
    /// Recovery record: recovery percent (0-100) when the archive is created
    /// with an inline RAR5 recovery record ("RR" service header).
    recovery_percent: Option<u8>,
    /// Recovery volumes: percent (0-100) of `.rev` files created alongside
    /// a multi-volume archive (WinRAR `-rv`).
    recovery_volumes_percent: Option<u8>,
    /// Recovery volumes: exact `.rev` file count (auto-capped at the data
    /// volume count).
    recovery_volumes_count: Option<u32>,
    /// File offset of the main archive header (for the recovery-record
    /// locator patch written at close time).
    main_header_start: Option<u64>,
    /// File offset of the recovery-record offset vint inside the main
    /// header's locator record (preallocated, patched at close time).
    rr_offset_field_pos: Option<u64>,
    /// All volume file paths (multi-volume archives).
    volume_paths: Vec<PathBuf>,
    /// Volume size limit for multi-volume creation (None = single volume).
    volume_size: Option<u64>,
    /// Current volume number during creation (1-indexed).
    current_volume: usize,
    /// Bytes written in the current volume during creation.
    volume_bytes_written: u64,
    /// Optional progress callback invoked during compression:
    /// `(bytes_processed_in_file, total_bytes_in_file)`.
    progress_callback: Option<Box<dyn FnMut(u64, u64) + Send>>,
    /// Create a solid archive (shared LZ window across compressed members).
    solid_mode: bool,
    /// Write a quick-open ("QO") service record at close time.
    quick_open: bool,
    /// Write BLAKE2sp hash records for members.
    blake2: bool,
    /// Cached (offset, full header bytes) of file headers for quick-open.
    quick_open_entries: Vec<(u64, Vec<u8>)>,
    /// File offset of the quick-open offset vint inside the main header's
    /// locator record (preallocated, patched at close time).
    qo_offset_field_pos: Option<u64>,
    /// Persistent RAR5 encoder state for solid archives.
    encoder_state: Option<crate::codec::EncoderState>,
    /// Requested dictionary log for compression (WinRAR `-md`);
    /// `None` = default selection.
    dict_size_log: Option<u8>,
    /// Requested dictionary size in bytes for RAR7 (v70) members
    /// (WinRAR `-md` above 4 GiB, any value > 4 GiB accepted).
    dict_size_bytes: Option<u64>,
    /// Save creation/change time in the FILE_TIME extra record (`-tsc`).
    save_ctime: bool,
    /// Save last access time in the FILE_TIME extra record (`-tsa`).
    save_atime: bool,
    /// Save the modification time (`-tsm`; false with `-tsm-`/`-ts-`).
    save_mtime: bool,
    /// Save owner/group on Unix (`-ow`).
    save_owner: bool,
    /// Save NTFS alternate data streams (`-os`; Windows only).
    save_streams: bool,
    /// NTFS alternate data streams ("STM" service records) attached to
    /// members, in archive order.
    streams: Vec<StreamRecord>,
    /// Store timestamps at 1-second precision (`-ts...1`).
    time_precision_seconds: bool,
    /// Options for the current read/extract operation (set per call).
    extract_options: crate::options::ExtractOptions,
}

/// An NTFS alternate data stream ("STM" service record) attached to an
/// archive member: the member index, the stream name (with the leading
/// colon, e.g. `:Zone.Identifier`), the stream payload location and its
/// compression parameters (the payload may be RAR5-compressed).
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
enum Mode {
    Read,
    Write,
    Append,
}

impl RarArchive {
    // ── Constructors ───────────────────────────────────────────────────────

    /// Open an existing RAR5 archive for reading.
    pub fn open(path: impl AsRef<Path>) -> RarResult<Self> {
        let path = path.as_ref().to_path_buf();
        let mut archive = RarArchive {
            path,
            mode: Mode::Read,
            entries: Vec::new(),
            sfx_offset: 0,
            stream: None,
            solid_state: None,
            solid_decoded_through: -1,
            password: None,
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
            progress_callback: None,
            solid_mode: false,
            quick_open: false,
            blake2: false,
            quick_open_entries: Vec::new(),
            qo_offset_field_pos: None,
            encoder_state: None,
            dict_size_log: None,
            dict_size_bytes: None,
            save_ctime: false,
            save_atime: false,
            save_mtime: true,
            save_owner: false,
            save_streams: false,
            streams: Vec::new(),
            time_precision_seconds: false,
            extract_options: crate::options::ExtractOptions::default(),
        };
        archive.open_read()?;
        Ok(archive)
    }

    /// Open an existing RAR5 archive with a password for encrypted content.
    pub fn open_with_password(path: impl AsRef<Path>, password: &str) -> RarResult<Self> {
        let path = path.as_ref().to_path_buf();
        let mut archive = RarArchive {
            path,
            mode: Mode::Read,
            entries: Vec::new(),
            sfx_offset: 0,
            stream: None,
            solid_state: None,
            solid_decoded_through: -1,
            password: Some(password.to_string()),
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
            progress_callback: None,
            solid_mode: false,
            quick_open: false,
            blake2: false,
            quick_open_entries: Vec::new(),
            qo_offset_field_pos: None,
            encoder_state: None,
            dict_size_log: None,
            dict_size_bytes: None,
            save_ctime: false,
            save_atime: false,
            save_mtime: true,
            save_owner: false,
            save_streams: false,
            streams: Vec::new(),
            time_precision_seconds: false,
            extract_options: crate::options::ExtractOptions::default(),
        };
        archive.open_read()?;
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
        let path = path.as_ref().to_path_buf();
        let mut archive = RarArchive {
            path,
            mode: Mode::Append,
            entries: Vec::new(),
            sfx_offset: 0,
            stream: None,
            solid_state: None,
            solid_decoded_through: -1,
            password: if password.is_empty() {
                None
            } else {
                Some(password.to_string())
            },
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
            progress_callback: None,
            solid_mode: false,
            quick_open: false,
            blake2: false,
            quick_open_entries: Vec::new(),
            qo_offset_field_pos: None,
            encoder_state: None,
            dict_size_log: None,
            dict_size_bytes: None,
            save_ctime: false,
            save_atime: false,
            save_mtime: true,
            save_owner: false,
            save_streams: false,
            streams: Vec::new(),
            time_precision_seconds: false,
            extract_options: crate::options::ExtractOptions::default(),
        };
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

        let first = crate::headers::read_block(&mut reader, self.archive_block_key().as_ref())?
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
                crate::headers::read_block(&mut reader, self.archive_block_key().as_ref())?
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
        while let Some(meta) = crate::headers::read_block(&mut reader, self.archive_block_key().as_ref())? {
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

        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;
        f.set_len(truncate_pos)?;
        f.seek(SeekFrom::End(0))?;
        self.stream = Some(Box::new(f));
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
        let first = crate::headers::read_block(&mut reader, self.archive_block_key().as_ref())?
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
                crate::headers::read_block(&mut reader, self.archive_block_key().as_ref())?
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
    /// recovery records and volume sizes. The dedicated `create*`
    /// constructors are thin wrappers around it.
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
    fn new_with_options(
        path: PathBuf,
        opts: crate::options::CreateOptions,
    ) -> RarResult<Self> {
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
            rr_offset_field_pos: None,
            volume_paths: Vec::new(),
            volume_size: opts.volume_size,
            current_volume: 0,
            volume_bytes_written: 0,
            progress_callback: None,
            solid_mode: opts.solid,
            quick_open,
            blake2: opts.blake2,
            quick_open_entries: Vec::new(),
            qo_offset_field_pos: None,
            encoder_state: None,
            dict_size_log: opts.dict_size_log,
            dict_size_bytes: opts.dict_size_bytes,
            save_ctime: opts.save_ctime,
            save_atime: opts.save_atime,
            save_mtime: opts.save_mtime,
            save_owner: opts.save_owner,
            save_streams: opts.save_streams,
            streams: Vec::new(),
            time_precision_seconds: opts.time_precision_seconds,
            extract_options: crate::options::ExtractOptions::default(),
        };
        Ok(archive)
    }

    /// Create a new RAR5 archive (overwrites existing file).
    #[deprecated(note = "use create_with_options instead")]
    pub fn create(path: impl AsRef<Path>) -> RarResult<Self> {
        Self::create_with_options(path, crate::options::CreateOptions::default())
    }

    /// Create a new multi-volume RAR5 archive.
    #[deprecated(note = "use create_with_options instead")]
    pub fn create_multivolume(path: impl AsRef<Path>, volume_size: u64) -> RarResult<Self> {
        Self::create_with_options(
            path,
            crate::options::CreateOptions {
                volume_size: Some(volume_size),
                ..Default::default()
            },
        )
    }

    /// Create a new multi-volume RAR5 archive with recovery volumes
    /// (`-rv`): `percent` of `.rev` files relative to the volume count.
    #[deprecated(note = "use create_with_options instead")]
    pub fn create_multivolume_with_recovery(
        path: impl AsRef<Path>,
        volume_size: u64,
        percent: u8,
    ) -> RarResult<Self> {
        Self::create_with_options(
            path,
            crate::options::CreateOptions {
                volume_size: Some(volume_size),
                recovery_volumes_percent: Some(percent.min(100)),
                ..Default::default()
            },
        )
    }

    /// Create a new multi-volume RAR5 archive with an exact number of
    /// recovery volumes. The count is auto-capped at the data volume count.
    #[deprecated(note = "use create_with_options instead")]
    pub fn create_multivolume_with_recovery_count(
        path: impl AsRef<Path>,
        volume_size: u64,
        rec_count: u32,
    ) -> RarResult<Self> {
        Self::create_with_options(
            path,
            crate::options::CreateOptions {
                volume_size: Some(volume_size),
                recovery_volume_count: Some(rec_count),
                ..Default::default()
            },
        )
    }

    /// Create a new encrypted multi-volume RAR5 archive (overwrites
    /// existing file). File data is AES-256 encrypted; header encryption
    /// is not supported for multi-volume archives.
    #[deprecated(note = "use create_with_options instead")]
    pub fn create_multivolume_with_password(
        path: impl AsRef<Path>,
        volume_size: u64,
        password: &str,
    ) -> RarResult<Self> {
        Self::create_with_options(
            path,
            crate::options::CreateOptions {
                volume_size: Some(volume_size),
                password: Some(password.to_string()),
                ..Default::default()
            },
        )
    }

    /// Create a new multi-volume RAR5 archive with header encryption
    /// (WinRAR `-hp` equivalent): every volume starts with the plaintext
    /// encryption header and all blocks are encrypted.
    #[deprecated(note = "use create_with_options instead")]
    pub fn create_multivolume_with_password_headers(
        path: impl AsRef<Path>,
        volume_size: u64,
        password: &str,
    ) -> RarResult<Self> {
        Self::create_with_options(
            path,
            crate::options::CreateOptions {
                volume_size: Some(volume_size),
                password: Some(password.to_string()),
                encrypt_headers: true,
                ..Default::default()
            },
        )
    }

    /// Create a new encrypted multi-volume RAR5 archive with an exact
    /// number of `.rev` recovery volumes.
    #[deprecated(note = "use create_with_options instead")]
    pub fn create_multivolume_with_recovery_count_and_password(
        path: impl AsRef<Path>,
        volume_size: u64,
        rec_count: u32,
        password: &str,
    ) -> RarResult<Self> {
        Self::create_with_options(
            path,
            crate::options::CreateOptions {
                volume_size: Some(volume_size),
                recovery_volume_count: Some(rec_count),
                password: Some(password.to_string()),
                ..Default::default()
            },
        )
    }

    /// Create a new encrypted RAR5 archive (overwrites existing file).
    #[deprecated(note = "use create_with_options instead")]
    pub fn create_with_password(path: impl AsRef<Path>, password: &str) -> RarResult<Self> {
        Self::create_with_options(
            path,
            crate::options::CreateOptions {
                password: Some(password.to_string()),
                ..Default::default()
            },
        )
    }

    /// Create a new RAR5 archive with encrypted headers (overwrites existing
    /// file). Hides file names and the whole archive structure: the main
    /// archive header is followed by an archive-level encryption header and
    /// every subsequent block header is AES-256-CBC encrypted.
    ///
    /// Not supported for multi-volume archives.
    #[deprecated(note = "use create_with_options instead")]
    pub fn create_with_password_headers(path: impl AsRef<Path>, password: &str) -> RarResult<Self> {
        Self::create_with_options(
            path,
            crate::options::CreateOptions {
                password: Some(password.to_string()),
                encrypt_headers: true,
                ..Default::default()
            },
        )
    }

    /// Create a new RAR5 archive with an inline recovery record
    /// (overwrites existing file). `percent` is the recovery percentage
    /// (0-100), matching WinRAR's `-rr` switch.
    #[deprecated(note = "use create_with_options instead")]
    #[allow(deprecated)] // legacy constructor delegating to its sibling
    pub fn create_with_recovery(path: impl AsRef<Path>, percent: u8) -> RarResult<Self> {
        Self::create_with_password_recovery(path, "", percent)
    }

    /// Create a new encrypted RAR5 archive with an inline recovery record.
    #[deprecated(note = "use create_with_options instead")]
    pub fn create_with_password_recovery(
        path: impl AsRef<Path>,
        password: &str,
        percent: u8,
    ) -> RarResult<Self> {
        Self::create_with_options(
            path,
            crate::options::CreateOptions {
                password: if password.is_empty() {
                    None
                } else {
                    Some(password.to_string())
                },
                recovery_percent: Some(percent.min(100)),
                ..Default::default()
            },
        )
    }

    /// Create a new RAR5 archive with header encryption and an inline
    /// recovery record.
    #[deprecated(note = "use create_with_options instead")]
    pub fn create_with_password_headers_recovery(
        path: impl AsRef<Path>,
        password: &str,
        percent: u8,
    ) -> RarResult<Self> {
        Self::create_with_options(
            path,
            crate::options::CreateOptions {
                password: Some(password.to_string()),
                encrypt_headers: true,
                recovery_percent: Some(percent.min(100)),
                ..Default::default()
            },
        )
    }

    // ── Lifecycle ──────────────────────────────────────────────────────────

    fn open_read(&mut self) -> RarResult<()> {
        self.volume_paths = discover_volumes(&self.path);
        if self.volume_paths.len() > 1 {
            self.scan_all_volumes()?;
        } else {
            let f = File::open(&self.path)?;
            self.stream = Some(Box::new(f));
            self.verify_signature()?;
            self.scan_blocks()?;
        }
        Ok(())
    }

    fn open_write(&mut self) -> RarResult<()> {
        if let Some(volume_size) = self.volume_size {
            if volume_size == 0 {
                return Err(RarError::Format(
                    "volume size must be greater than zero".into(),
                ));
            }
            let base = get_volume_base(&self.path);
            let parent = self.path.parent().unwrap_or(Path::new("."));
            let vol_path = volume_path(parent, &base, 1);
            self.volume_paths = vec![vol_path.clone()];
            self.current_volume = 1;
            let f = read_write_create(&vol_path)?;
            self.stream = Some(Box::new(f));
            self.write_signature()?;
            self.write_archive_encryption_header_if_needed()?;
            self.write_archive_header_vol(None)?;
            self.volume_bytes_written = self.stream.as_mut().unwrap().stream_position()?;
            return Ok(());
        }

        let f = read_write_create(&self.path)?;
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
                encryption::EncryptionParams::generate_for_password(password, ENCR_PBKDF2_ITER_LOG);
            self.archive_encr = Some(encr);
        }
        let block = self.archive_encr.as_ref().unwrap().to_archive_header_block();
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(&block)?;
        Ok(())
    }

    /// On-disk size of a block header: header encryption wraps every header
    /// in `[16-byte IV][PKCS7-padded ciphertext]`.
    fn on_disk_header_len(&self, plain_len: u64) -> u64 {
        if self.header_encryption {
            16 + ((plain_len + 15) & !15)
        } else {
            plain_len
        }
    }

    /// Write a block header, wrapping it in `[16-byte IV][AES-256-CBC
    /// encrypted header]` when header encryption is enabled.
    fn write_block_header(&mut self, header_bytes: &[u8]) -> RarResult<()> {
        let stream = self.stream.as_mut().unwrap();
        if let Some(ref encr) = self.archive_encr {
            let password = self
                .password
                .as_ref()
                .ok_or_else(|| RarError::Encrypted("no password set".into()))?;
            let key = encr.get_key(password);
            let mut iv = [0u8; ENCR_IV_SIZE];
            rand::fill(&mut iv);
            let ciphertext = encryption::encrypt_data(header_bytes, &key, &iv);
            stream.write_all(&iv)?;
            stream.write_all(&ciphertext)?;
        } else {
            stream.write_all(header_bytes)?;
        }
        Ok(())
    }

    /// Finalize the archive (writes end-of-archive block in write mode).
    pub fn close(&mut self) -> RarResult<()> {
        self.finish_writing()?;
        self.stream = None;
        if self.recovery_volumes_percent.is_some() || self.recovery_volumes_count.is_some() {
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
        let nd = self.volume_paths.len();
        if nd == 0 {
            return Err(RarError::Format("no volumes for recovery volumes".into()));
        }
        if nd > 65535 {
            return Err(RarError::Format(format!(
                "too many volumes ({nd}) for recovery volumes; maximum is 65535"
            )));
        }
        // Exact count wins; the percent variant is converted at close time.
        let rec_count = if let Some(count) = self.recovery_volumes_count {
            (count as usize).min(nd)
        } else if let Some(percent) = self.recovery_volumes_percent {
            crate::recovery::rev5::plan_recovery_volume_count(nd, percent as u64)?
        } else {
            return Ok(());
        };

        // Stream all volumes in lockstep chunks: per-chunk Reed-Solomon
        // parity keeps memory bounded at O(chunk x volumes) and CRCs are
        // computed in the same pass.
        const CHUNK: u64 = 1024 * 1024;
        let mut volume_sizes = Vec::with_capacity(self.volume_paths.len());
        let mut readers = Vec::with_capacity(self.volume_paths.len());
        let mut crcs = Vec::with_capacity(self.volume_paths.len());
        for vol in &self.volume_paths {
            let size = fs::metadata(vol)?.len();
            volume_sizes.push(size);
            readers.push(File::open(vol)?);
            crcs.push(crc32fast::Hasher::new());
        }
        let max_len = *volume_sizes.iter().max().unwrap_or(&0);
        let padded_max = if max_len % 2 == 0 {
            max_len
        } else {
            max_len + 1
        };

        let mut payloads: Vec<Vec<u8>> = vec![Vec::new(); rec_count];
        let mut offset = 0u64;
        while offset < padded_max {
            let want = (padded_max - offset).min(CHUNK) as usize;
            let mut chunk_bufs: Vec<Vec<u8>> = Vec::with_capacity(nd);
            for (i, reader) in readers.iter_mut().enumerate() {
                let mut buf = vec![0u8; want];
                if offset < volume_sizes[i] {
                    let to_read = (volume_sizes[i] - offset).min(want as u64) as usize;
                    let n = read_up_to(reader, &mut buf[..to_read])?;
                    if n != to_read {
                        return Err(RarError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!(
                                "volume {} shrank while building recovery volumes",
                                self.volume_paths[i].display()
                            ),
                        )));
                    }
                    crcs[i].update(&buf[..to_read]);
                    buf[to_read..].fill(0); // zero-pad to the chunk length
                }
                chunk_bufs.push(buf);
            }
            let refs: Vec<&[u8]> = chunk_bufs.iter().map(|b| b.as_slice()).collect();
            let parity = crate::recovery::rar5::encode_parity_shards(&refs, rec_count)
                .map_err(|e| RarError::Format(format!("recovery volumes encode: {e}")))?;
            for (k, p) in parity.into_iter().enumerate() {
                payloads[k].extend(p);
            }
            offset += want as u64;
        }
        let volume_crcs: Vec<u32> = crcs.into_iter().map(|h| h.finalize()).collect();

        let base = get_volume_base(&self.path);
        let parent = self.path.parent().unwrap_or(Path::new("."));
        for (k, payload) in payloads.iter().enumerate() {
            let rev_path = parent.join(format!("{base}.part{}.rev", k + 1));
            let file = crate::recovery::rev5::build_recovery_volume_file(
                k,
                rec_count,
                &volume_sizes,
                &volume_crcs,
                payload,
            );
            std::fs::write(&rev_path, &file)?;
        }
        self.recovery_volumes_percent = None;
        Ok(())
    }

    /// Compute the RAR5 recovery record over the archive written so far
    /// and append the `"RR"` service header. The main header locator was
    /// already patched by [`Self::close`].
    fn write_recovery_record(&mut self) -> RarResult<()> {
        let path = self.path.clone();
        self.write_recovery_record_from(&path)
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
        let hdr = crate::headers::build_service_block("RR", &subdata, rr_data.len() as u64, crate::constants::BLOCK_FLAG_SKIP_IF_UNKNOWN);

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
        let hdr = crate::headers::build_service_block("QO", &subdata, payload.len() as u64, crate::constants::BLOCK_FLAG_SKIP_IF_UNKNOWN);

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
            let first_pt = encryption::decrypt_data(&first, &key, &iv)?;
            let (hsize, vint_len) = vint::decode_from_slice(&first_pt, 4)
                .map_err(|e| RarError::Format(format!("main header vint: {e}")))?;
            let total_raw = 4 + vint_len + hsize as usize;
            let enc_size = total_raw.div_ceil(16) * 16;
            let mut full_ct = vec![0u8; enc_size];
            full_ct[..16].copy_from_slice(&first);
            if enc_size > 16 {
                stream.read_exact(&mut full_ct[16..])?;
            }
            let full_pt = encryption::decrypt_data(&full_ct, &key, &iv)?;
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
            let ciphertext = encryption::encrypt_data(&new_header, &key, &iv);
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
    /// The callback receives `(bytes_processed, bytes_total)` for the file
    /// currently being added. Every file entry starts with a `(0, total)`
    /// event, then reports absolute bytes processed while the member is
    /// compressed, and ends with `(total, total)` once the entry has been
    /// written, so callers can accumulate per-file deltas into a global
    /// percent-done UI without double-counting.
    pub fn set_progress_callback(&mut self, callback: Option<Box<dyn FnMut(u64, u64) + Send>>) {
        self.progress_callback = callback;
    }

    // ── Signature ──────────────────────────────────────────────────────────

    fn verify_signature(&mut self) -> RarResult<()> {
        // The signature must appear at the start for plain archives and
        // after the embedded stub for SFX archives (scan up to 8 MiB,
        // like the reference readers).
        const SFX_SCAN_LIMIT: usize = 8 * 1024 * 1024;
        let stream = self.stream.as_mut().unwrap();
        let file_size = stream.seek(SeekFrom::End(0))?;
        stream.seek(SeekFrom::Start(0))?;
        let scan = file_size.min(SFX_SCAN_LIMIT as u64) as usize;
        let mut buf = vec![0u8; scan];
        let n = stream.read(&mut buf)?;
        buf.truncate(n);
        let rar5_pos = find_bytes(&buf, RAR5_SIGNATURE);
        let rar4_pos = find_bytes(&buf, b"Rar!\x1a\x07\x00");
        let sfx_offset = match (rar5_pos, rar4_pos) {
            (Some(_), Some(r4)) if r4 < rar5_pos.unwrap() => {
                return Err(RarError::Unsupported(
                    "RAR4 archives are not supported; use 7-Zip to read or extract them".into(),
                ));
            }
            (Some(r5), _) => r5 as u64,
            (None, Some(_)) => {
                return Err(RarError::Unsupported(
                    "RAR4 archives are not supported; use 7-Zip to read or extract them".into(),
                ));
            }
            (None, None) => {
                return Err(RarError::Format(
                    "not a RAR archive (signature not found)".into(),
                ));
            }
        };
        self.sfx_offset = sfx_offset;
        stream.seek(SeekFrom::Start(sfx_offset + RAR5_SIGNATURE.len() as u64))?;
        Ok(())
    }

    fn write_signature(&mut self) -> RarResult<()> {
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(RAR5_SIGNATURE)?;
        Ok(())
    }

    // ── Block scanning ─────────────────────────────────────────────────────

    fn scan_blocks(&mut self) -> RarResult<()> {
        self.entries.clear();
        self.streams.clear();

        // None until the plaintext archive-level encryption header arrives
        // (header-encrypted archives: every block after it is `[IV][AES-256-
        // CBC header]`).
        let mut encr_key: Option<[u8; 32]> = None;
        let mut last_file_index: Option<usize> = None;

        loop {
            let meta = match crate::headers::read_block(
                self.stream.as_mut().unwrap(),
                encr_key.as_ref(),
            )? {
                Some(meta) => meta,
                None => break,
            };
            let raw = &meta.raw;
            let stream_pos = self.stream.as_mut().unwrap().stream_position()?;

            match raw.block_type {
                BLOCK_TYPE_ARCHIVE_HEADER => {
                    let _ah = ArchiveHeader::from_raw(&raw)?;
                }
                BLOCK_TYPE_FILE_HEADER => {
                    let fh = FileHeader::from_raw(&raw, stream_pos)?;
                    let chunk = DataChunk {
                        volume_index: 0,
                        data_offset: fh.data_offset,
                        packed_size: fh.packed_size,
                        crc32_val: fh.crc32_val,
                        is_final: true,
                        extra_data: fh.extra_data.clone(),
                    };
                    self.entries.push(ArchiveEntry {
                        header: fh,
                        chunks: vec![chunk],
                    });
                    last_file_index = Some(self.entries.len() - 1);
                }
                BLOCK_TYPE_SERVICE_HEADER
                    if raw.flags & crate::constants::BLOCK_FLAG_DEPENDS_PREV != 0 =>
                {
                    // NTFS stream record ("STM"): the SUBDATA extra holds
                    // the stream name (":name"), the data area the content.
                    let name = self.service_block_name(&meta)?;
                    if name.as_deref() == Some("STM")
                        && let Some(owner_index) = last_file_index
                        && let Some(stream_name) = crate::headers::parse_service_subdata(
                            &crate::headers::block_extra_area(&raw.header_data),
                        )
                        && !stream_name.is_empty()
                        && let Some((unpacked_size, method, dict_size_log)) =
                            crate::headers::parse_stream_params(&raw.header_data)
                    {
                        self.streams.push(StreamRecord {
                            owner_index,
                            name: String::from_utf8_lossy(&stream_name).into_owned(),
                            data_offset: raw.data_offset,
                            data_size: raw.data_size,
                            unpacked_size,
                            method,
                            dict_size_log,
                        });
                    }
                }
                BLOCK_TYPE_END_ARCHIVE => break,
                BLOCK_TYPE_ENCRYPT_HEADER => {
                    let password = self.password.as_ref().ok_or_else(|| {
                        RarError::Encrypted(
                            "archive has encrypted headers; provide a password".into(),
                        )
                    })?;
                    let params = parse_archive_encrypt_header(&raw)?;
                    if !params.verify_password(password) {
                        return Err(RarError::WrongPassword);
                    }
                    encr_key = Some(params.get_key(password));
                }
                _ => {}
            }

            if raw.data_size > 0 {
                self.stream
                    .as_mut()
                    .unwrap()
                    .seek(SeekFrom::Start(raw.data_offset + raw.data_size))?;
            }
        }

        Ok(())
    }

    /// Scan all volumes of a multi-volume archive.
    ///
    /// Header-encrypted volume sets repeat the plaintext archive-level
    /// encryption header at the start of EVERY volume (WinRAR convention);
    /// every block after it is `[16-byte IV][AES-256-CBC encrypted
    /// header]`. The archive key is derived once per volume and reused for
    /// all of its blocks.
    fn scan_all_volumes(&mut self) -> RarResult<()> {
        self.entries.clear();
        let mut pending: Option<ArchiveEntry> = None;

        for (vol_idx, vol_path) in self.volume_paths.iter().enumerate() {
            let mut stream = File::open(vol_path)?;

            // Verify signature
            let mut sig = [0u8; 8];
            stream.read_exact(&mut sig)?;
            if sig != *RAR5_SIGNATURE {
                return Err(RarError::Format(format!(
                    "volume {} has bad signature",
                    vol_path.display()
                )));
            }

            // None until this volume's plaintext encryption header arrives.
            let mut encr_key: Option<[u8; 32]> = None;

            loop {
                let raw = match crate::headers::read_block(&mut stream, encr_key.as_ref())? {
                    Some(meta) => meta.raw,
                    None => break,
                };

                let stream_pos = stream.stream_position()?;

                match raw.block_type {
                    BLOCK_TYPE_ARCHIVE_HEADER => {
                        let _ah = ArchiveHeader::from_raw(&raw)?;
                    }
                    BLOCK_TYPE_FILE_HEADER => {
                        let fh = FileHeader::from_raw(&raw, stream_pos)?;
                        let continues_from = raw.flags & BLOCK_FLAG_DATA_CONTINUES != 0;
                        let continues_to = raw.flags & BLOCK_FLAG_DATA_CONTINUE_TO != 0;

                        let chunk = DataChunk {
                            volume_index: vol_idx,
                            data_offset: fh.data_offset,
                            packed_size: fh.packed_size,
                            crc32_val: fh.crc32_val,
                            is_final: !continues_to,
                            extra_data: fh.extra_data.clone(),
                        };

                        if continues_from {
                            if let Some(ref mut entry) = pending {
                                entry.chunks.push(chunk);
                                if !continues_to {
                                    // Final chunk: total packed size and the
                                    // final chunk's CRC (MAC'd when
                                    // encrypted). For encrypted members the
                                    // final chunk also carries the full extra
                                    // records (encryption with the hash-key
                                    // MAC bit, BLAKE2sp hash, time); the
                                    // reader must verify with those, so merge
                                    // them in when present.
                                    let total_packed: u64 =
                                        entry.chunks.iter().map(|c| c.packed_size).sum();
                                    entry.header.packed_size = total_packed;
                                    entry.header.crc32_val = fh.crc32_val;
                                    if !fh.extra_data.is_empty() {
                                        entry.header.extra_data = fh.extra_data.clone();
                                        entry.header.hash_type = fh.hash_type;
                                        entry.header.hash_value = fh.hash_value;
                                        entry.header.mtime_ns = fh.mtime_ns;
                                        entry.header.owner = fh.owner.clone();
                                        entry.header.group = fh.group.clone();
                                        entry.header.version = fh.version;
                                    }
                                    self.entries.push(pending.take().unwrap());
                                }
                            }
                        } else if continues_to {
                            pending = Some(ArchiveEntry {
                                header: fh,
                                chunks: vec![chunk],
                            });
                        } else {
                            self.entries.push(ArchiveEntry {
                                header: fh,
                                chunks: vec![chunk],
                            });
                        }
                    }
                    BLOCK_TYPE_END_ARCHIVE => {
                        let eoa = EndOfArchiveHeader::from_raw(&raw)?;
                        let _ = eoa;
                        break; // continue to next volume
                    }
                    BLOCK_TYPE_ENCRYPT_HEADER => {
                        let password = self.password.as_ref().ok_or_else(|| {
                            RarError::Encrypted(
                                "archive has encrypted headers; provide a password".into(),
                            )
                        })?;
                        let params = parse_archive_encrypt_header(&raw)?;
                        if !params.verify_password(password) {
                            return Err(RarError::WrongPassword);
                        }
                        encr_key = Some(params.get_key(password));
                    }
                    _ => {}
                }

                if raw.data_size > 0 {
                    stream.seek(SeekFrom::Start(raw.data_offset + raw.data_size))?;
                }
            }
        }

        // Keep the first volume open as the default stream
        self.stream = Some(Box::new(File::open(&self.volume_paths[0])?));
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

    fn start_next_volume(&mut self) -> RarResult<()> {
        self.write_end_block_flags(true)?;
        // Close current volume
        self.stream = None;
        self.current_volume += 1;
        let parent = self.path.parent().unwrap_or(Path::new("."));
        let base = get_volume_base(&self.path);
        let vol_path = volume_path(parent, &base, self.current_volume);
        self.volume_paths.push(vol_path.clone());
        let f = read_write_create(&vol_path)?;
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

    // ── Public API: listing ────────────────────────────────────────────────

    /// Return all entries in the archive.
    pub fn list(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    /// Find an entry by name.
    pub fn get_entry(&self, name: &str) -> Option<&ArchiveEntry> {
        self.entries.iter().find(|e| e.name() == name)
    }

    /// Return a list of all entry names.
    pub fn namelist(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name()).collect()
    }

    // ── Public API: reading ────────────────────────────────────────────────

    /// Read and return the uncompressed content of a member.
    pub fn read(&mut self, name: &str) -> RarResult<Vec<u8>> {
        self.read_with_options(name, crate::options::ExtractOptions::default())
    }

    /// Read a member with explicit limits (see [`crate::ExtractOptions`]).
    pub fn read_with_options(
        &mut self,
        name: &str,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<Vec<u8>> {
        let target_idx = self
            .entries
            .iter()
            .position(|e| e.name() == name)
            .ok_or_else(|| RarError::MemberNotFound { name: name.to_string() })?;
        self.extract_options = opts;
        self.validate_entry_limits(target_idx)?;
        if self.is_solid_chain_member(target_idx) {
            return self.decode_solid_through(target_idx);
        }
        self.decode_file_at(target_idx, None)
    }

    /// Extract all archive contents to `dest_dir` (safe defaults).
    pub fn extract_all(&mut self, dest_dir: impl AsRef<Path>) -> RarResult<()> {
        self.extract_all_with_options(dest_dir, crate::options::ExtractOptions::default())
    }

    /// Extract all archive contents with explicit options.
    pub fn extract_all_with_options(
        &mut self,
        dest_dir: impl AsRef<Path>,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<()> {
        let dest = dest_dir.as_ref();
        fs::create_dir_all(dest)?;
        self.extract_options = opts;

        #[cfg(feature = "parallel")]
        {
            if self.extract_all_parallel(dest, opts)? {
                return Ok(());
            }
        }

        let mut total_unpacked = 0u64;
        let entries: Vec<_> = self.entries.clone();
        for entry in &entries {
            total_unpacked = total_unpacked
                .checked_add(entry.header.unpacked_size)
                .ok_or_else(|| RarError::LimitExceeded {
                    limit: opts.max_total_unpacked_bytes.unwrap_or(u64::MAX),
                    context: "total unpacked size overflow".into(),
                })?;
            if let Some(limit) = opts.max_total_unpacked_bytes
                && total_unpacked > limit
            {
                return Err(RarError::LimitExceeded {
                    limit,
                    context: format!(
                        "total unpacked size {total_unpacked} exceeds limit while extracting {}",
                        entry.name()
                    ),
                });
            }
            self.extract_entry(entry, dest)?;
        }
        Ok(())
    }

    /// Parallel extraction for eligible archives (optional `parallel`
    /// feature).
    ///
    /// Eligible: at least [`PARALLEL_MIN_MEMBERS`] members, no solid chains,
    /// no split/multi-volume members, no progress callback, and total packed
    /// + unpacked sizes within a bounded memory budget. Packed payloads are
    ///   read sequentially, then decoded and integrity-checked with Rayon
    ///   workers (archive order preserved by replaying writes sequentially
    ///   afterwards). Ineligible archives fall back to the sequential path
    ///   unchanged. The codec's decode is memory-bandwidth-bound, so the
    ///   parallel path mainly helps on machines where decompression is
    ///   CPU-bound; it engages only for member counts and sizes where Rayon
    ///   overhead is amortized.
    #[cfg(feature = "parallel")]
    fn extract_all_parallel(
        &mut self,
        dest: &Path,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<bool> {
        use rayon::prelude::*;

        if self.progress_callback.is_some() || self.entries.len() < PARALLEL_MIN_MEMBERS {
            return Ok(false);
        }
        for (i, e) in self.entries.iter().enumerate() {
            if self.is_solid_chain_member(i) || e.chunks.len() != 1 {
                return Ok(false);
            }
        }
        let mut total_packed = 0u64;
        let mut total_unpacked = 0u64;
        for e in &self.entries {
            total_packed = total_packed.saturating_add(e.header.packed_size);
            total_unpacked = total_unpacked.saturating_add(e.header.unpacked_size);
            if total_packed > PARALLEL_BUFFER_LIMIT || total_unpacked > PARALLEL_BUFFER_LIMIT {
                return Ok(false);
            }
        }
        if total_unpacked < PARALLEL_MIN_UNPACKED {
            return Ok(false);
        }
        if let Some(limit) = opts.max_total_unpacked_bytes
            && total_unpacked > limit
        {
            return Err(RarError::LimitExceeded {
                limit,
                context: "total unpacked size exceeds limit".into(),
            });
        }

        // Phase 1: read + decrypt all payloads sequentially.
        let mut payloads: Vec<(usize, DecryptedPayload)> = Vec::with_capacity(self.entries.len());
        for i in 0..self.entries.len() {
            payloads.push((i, self.read_packed_data(i)?));
        }
        let headers: Vec<FileHeader> = self.entries.iter().map(|e| e.header.clone()).collect();

        struct DecodedMember {
            idx: usize,
            data: Vec<u8>,
        }

        // Phase 2: decode + integrity-check in parallel.
        let results: Vec<RarResult<DecodedMember>> = extraction_pool().install(|| {
            payloads
                .into_par_iter()
                .map(|(i, payload)| {
                    let hdr = &headers[i];
                    if hdr.comp_dict_size > MAX_DICT_SIZE_LOG {
                        return Err(RarError::LimitExceeded {
                            limit: MAX_DICT_SIZE_LOG as u64,
                            context: format!(
                                "{}: dictionary size log {} exceeds supported maximum {}",
                                hdr.name, hdr.comp_dict_size, MAX_DICT_SIZE_LOG
                            ),
                        });
                    }
                    if let Some(limit) = opts.max_unpacked_bytes
                        && hdr.unpacked_size > limit
                    {
                        return Err(RarError::LimitExceeded {
                            limit,
                            context: format!(
                                "{}: unpacked size {} exceeds limit",
                                hdr.name, hdr.unpacked_size
                            ),
                        });
                    }

                    let data = if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
                        Vec::new()
                    } else if hdr.comp_method == COMP_METHOD_STORE {
                        payload.data
                    } else {
                        crate::codec::decode(
                            &payload.data,
                            hdr.unpacked_size,
                            crate::codec::DecodeOptions {
                                dict_size_log: hdr.comp_dict_size,
                                dict_size_bytes: hdr.dict_size_bytes,
                                extra_dist: hdr.dict_size_bytes.is_some(),
                                state: None,
                            },
                        )
                        .map_err(RarError::Unsupported)?
                    };

                    let crc = crc32fast::hash(&data);
                    let blake = if hdr.hash_value.is_some() {
                        Some(crate::blake2sp::hash(&data))
                    } else {
                        None
                    };
                    verify_integrity_for(
                        hdr,
                        crc,
                        blake,
                        payload.params.as_ref(),
                        payload.keys.as_ref(),
                    )?;
                    Ok(DecodedMember { idx: i, data })
                })
                .collect()
        });

        // Phase 3: replay writes sequentially in archive order.
        for result in results {
            let member = result?;
            let entry = &self.entries[member.idx];
            let dest_path = self.safe_dest_path(dest, &entry.header.name)?;
            if entry.is_dir() {
                fs::create_dir_all(&dest_path)?;
                continue;
            }
            if let Some(redir) = parse_redirect_record(&entry.header.extra_data) {
                self.extract_redirection(dest, &dest_path, &redir)?;
                continue;
            }
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let tmp_path = temp_sibling_path(&dest_path);
            let write_result = (|| -> RarResult<()> {
                let mut file = File::create(&tmp_path)?;
                file.write_all(&member.data)?;
                file.flush()?;
                Ok(())
            })();
            match write_result {
                Ok(()) => replace_file(&tmp_path, &dest_path)?,
                Err(e) => {
                    let _ = fs::remove_file(&tmp_path);
                    return Err(e);
                }
            }
            if entry.header.mtime != 0 || entry.header.mtime_ns.is_some() {
                self.apply_member_times(&entry.header, &dest_path);
            }
            self.extract_member_streams(member.idx, &dest_path)?;
        }
        Ok(true)
    }

    /// Extract a single entry to `dest_dir` (safe defaults).
    pub fn extract(&mut self, name: &str, dest_dir: impl AsRef<Path>) -> RarResult<PathBuf> {
        self.extract_with_options(name, dest_dir, crate::options::ExtractOptions::default())
    }

    /// Extract a single entry with explicit options.
    pub fn extract_with_options(
        &mut self,
        name: &str,
        dest_dir: impl AsRef<Path>,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<PathBuf> {
        let dest = dest_dir.as_ref();
        fs::create_dir_all(dest)?;
        self.extract_options = opts;
        let idx = self
            .entries
            .iter()
            .position(|e| e.name() == name)
            .ok_or_else(|| RarError::MemberNotFound { name: name.to_string() })?;
        self.validate_entry_limits(idx)?;
        self.extract_entry(&self.entries[idx].clone(), dest)
    }

    /// Validate per-entry header limits against the current extract options.
    fn validate_entry_limits(&self, idx: usize) -> RarResult<()> {
        let hdr = &self.entries[idx].header;
        if hdr.comp_dict_size > MAX_DICT_SIZE_LOG {
            return Err(RarError::LimitExceeded {
                limit: MAX_DICT_SIZE_LOG as u64,
                context: format!(
                    "{}: dictionary size log {} exceeds supported maximum {}",
                    hdr.name, hdr.comp_dict_size, MAX_DICT_SIZE_LOG
                ),
            });
        }
        if let Some(limit) = self.extract_options.max_unpacked_bytes
            && hdr.unpacked_size > limit
        {
            return Err(RarError::LimitExceeded {
                limit,
                context: format!(
                    "{}: unpacked size {} exceeds limit",
                    hdr.name, hdr.unpacked_size
                ),
            });
        }
        Ok(())
    }

    /// Extract one entry. File contents are decoded to a temporary file and
    /// renamed over the destination only after integrity checks pass, so a
    /// failure never leaves partial or corrupt output behind.
    fn extract_entry(&mut self, entry: &ArchiveEntry, dest_dir: &Path) -> RarResult<PathBuf> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.header.data_offset == entry.header.data_offset)
            .unwrap_or(0);
        self.validate_entry_limits(idx)?;

        // Flat extraction (`rar e` / `unrar e`): members land in the
        // destination directory under their basename. The safe-path policy
        // always applies here — the full member name is sanitized (which
        // rejects `..`/absolute/drive names) before its basename is used,
        // so traversal-shaped names cannot escape the destination.
        let dest_path = if self.extract_options.flat_paths {
            if entry.is_dir() {
                return Ok(dest_dir.to_path_buf());
            }
            let safe_name = sanitize_archive_path(&entry.header.name)?;
            let base = safe_name.rsplit('/').next().unwrap_or(&safe_name);
            dest_dir.join(base)
        } else {
            self.safe_dest_path(dest_dir, &entry.header.name)?
        };

        // `-o-` (skip existing): members whose destination already exists
        // are left untouched.
        if self.extract_options.skip_existing && dest_path.exists() {
            return Ok(dest_path);
        }

        // `-or` (auto rename): when the destination exists, insert `(N)`
        // before the extension (like WinRAR: a.txt -> a(1).txt).
        let mut dest_path = dest_path;
        if self.extract_options.auto_rename && !entry.is_dir() {
            let mut n = 1;
            while dest_path.exists() {
                let file_name = dest_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let (stem, ext) = match file_name.rfind('.') {
                    Some(dot) if dot > 0 => (&file_name[..dot], &file_name[dot..]),
                    _ => (file_name.as_str(), ""),
                };
                dest_path = dest_path.with_file_name(format!("{stem}({n}){ext}"));
                n += 1;
            }
        }

        if entry.is_dir() {
            fs::create_dir_all(&dest_path)?;
            return Ok(dest_path);
        }

        // RAR5 redirect records (symlinks, hardlinks, file copies): the
        // entry carries no data, only the target reference.
        if let Some(redir) = parse_redirect_record(&entry.header.extra_data) {
            return self.extract_redirection(dest_dir, &dest_path, &redir);
        }

        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = temp_sibling_path(&dest_path);
        let result = (|| -> RarResult<u64> {
            let mut file = File::create(&tmp_path)?;
            let written = if self.is_solid_chain_member(idx) {
                self.decode_solid_through_to(idx, &mut file)?
            } else {
                self.decode_file_to(idx, &mut file, None)?
            };
            file.flush()?;
            Ok(written)
        })();

        match result {
            Ok(_) => {
                replace_file(&tmp_path, &dest_path)?;
            }
            Err(e) => {
                if self.extract_options.keep_broken {
                    // `-kb`: keep the partially extracted file.
                    let _ = replace_file(&tmp_path, &dest_path);
                } else {
                    let _ = fs::remove_file(&tmp_path);
                }
                return Err(e);
            }
        }

        // Restore mtime (best-effort), including the nanosecond fraction
        // from the FILE_TIME extra record when present.
        if entry.header.mtime != 0 || entry.header.mtime_ns.is_some() {
            self.apply_member_times(&entry.header, &dest_path);
        }
        // Restore NTFS alternate data streams attached to this member
        // (no-op on non-Windows, like the reference extractor).
        self.extract_member_streams(idx, &dest_path)?;

        Ok(dest_path)
    }

    /// Write the "STM" stream records attached to member `idx` onto the
    /// extracted file (`file:name`); Windows only.
    fn extract_member_streams(&mut self, idx: usize, dest_path: &Path) -> RarResult<()> {
        #[cfg(windows)]
        {
            use std::io::Read;
            let owned: Vec<StreamRecord> = self
                .streams
                .iter()
                .filter(|s| s.owner_index == idx)
                .cloned()
                .collect();
            for s in owned {
                // Read the stream payload (possibly RAR5-compressed).
                let mut packed = vec![0u8; s.data_size as usize];
                {
                    let stream = self.stream.as_mut().unwrap();
                    stream.seek(SeekFrom::Start(s.data_offset))?;
                    stream.read_exact(&mut packed)?;
                }
                let data = if s.method == crate::constants::COMP_METHOD_STORE {
                    packed
                } else {
                    crate::codec::decode_standalone(
                        &packed,
                        s.unpacked_size,
                        s.dict_size_log,
                        None,
                        false,
                    )
                    .map_err(|e| RarError::Format(format!("stream decode: {e}")))?
                };
                write::write_windows_stream(dest_path, &s.name, &data)?;
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (idx, dest_path);
        }
        Ok(())
    }

    /// Restore a member's stored timestamps on the extracted file: the
    /// modification time always (when nonzero), plus access time when
    /// requested via [`ExtractOptions`]. The creation time is set through
    /// `SetFileTime` on Windows (std has no creation-time setter) and is a
    /// `SetFileTime` on Windows (std has no creation-time setter) and is a
    /// no-op on Unix, where the change time cannot be set (matching
    /// WinRAR's behavior).
    fn apply_member_times(&self, hdr: &crate::headers::FileHeader, dest_path: &Path) {
        let mut times = std::fs::FileTimes::new();
        let mut any = false;
        if hdr.mtime != 0 || hdr.mtime_ns.is_some() {
            let mut mtime = UNIX_EPOCH + std::time::Duration::from_secs(hdr.mtime as u64);
            if let Some(ns) = hdr.mtime_ns {
                mtime += std::time::Duration::from_nanos(ns as u64);
            }
            times = times.set_modified(mtime);
            any = true;
        }
        if self.extract_options.set_access_time
            && let Some((secs, ns)) = hdr.atime
        {
            let t = UNIX_EPOCH
                + std::time::Duration::from_secs(secs)
                + std::time::Duration::from_nanos(ns as u64);
            times = times.set_accessed(t);
            any = true;
        }
        if any {
            let _ = std::fs::File::options()
                .write(true)
                .open(dest_path)
                .and_then(|f| f.set_times(times));
        }
        #[cfg(windows)]
        if self.extract_options.set_creation_time
            && let Some((secs, ns)) = hdr.ctime
        {
            let _ = write::windows_set_creation_time(dest_path, secs, ns);
        }
    }

    /// Materialize a RAR5 file redirection (symlink, hardlink or file
    /// copy) at `dest_path`.
    fn extract_redirection(
        &self,
        dest_dir: &Path,
        dest_path: &Path,
        redir: &RedirectSpec,
    ) -> RarResult<PathBuf> {
        const REDIR_UNIX_SYMLINK: u64 = 0x01;
        const REDIR_WINDOWS_SYMLINK: u64 = 0x02;
        const REDIR_WINDOWS_JUNCTION: u64 = 0x03;
        const REDIR_HARDLINK: u64 = 0x04;
        const REDIR_FILE_COPY: u64 = 0x05;
        match redir.redir_type {
            REDIR_UNIX_SYMLINK | REDIR_WINDOWS_SYMLINK | REDIR_WINDOWS_JUNCTION => {
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&redir.target, dest_path)?;
                }
                #[cfg(not(unix))]
                {
                    return Err(RarError::Unsupported(
                        "symbolic links are not supported on this platform".into(),
                    ));
                }
            }
            REDIR_HARDLINK | REDIR_FILE_COPY => {
                let target_path = self.safe_dest_path(dest_dir, &redir.target)?;
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                if redir.redir_type == REDIR_HARDLINK {
                    fs::hard_link(&target_path, dest_path)?;
                } else {
                    fs::copy(&target_path, dest_path)?;
                }
            }
            _ => {
                // Unknown redirection type: fall back to an empty regular
                // file so the archive remains extractable.
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(dest_path, [])?;
            }
        }
        Ok(dest_path.to_path_buf())
    }

    /// Compute the destination path for an entry name, applying the safe
    /// path policy (sanitization + canonical containment check).
    fn safe_dest_path(&self, dest_dir: &Path, name: &str) -> RarResult<PathBuf> {
        let sanitized = if self.extract_options.safe_paths {
            sanitize_archive_path(name)?
        } else {
            name.replace('\\', "/")
        };
        let dest_path = dest_dir.join(&sanitized);
        if self.extract_options.safe_paths
            && let Some(parent) = dest_path.parent()
        {
            fs::create_dir_all(parent)?;
            let canon_dest = dest_dir.canonicalize()?;
            let canon_parent = parent.canonicalize()?;
            if !canon_parent.starts_with(&canon_dest) {
                return Err(RarError::Security(format!(
                    "entry {name:?} resolves outside the destination directory"
                )));
            }
        }
        Ok(dest_path)
    }

    /// Check if entry at `idx` is in a solid chain (is solid itself, or
    /// the next entry after it is solid).
    fn is_solid_chain_member(&self, idx: usize) -> bool {
        let hdr = &self.entries[idx].header;
        if hdr.comp_solid {
            return true;
        }
        // First file in a solid group isn't flagged solid but the next one is
        if idx + 1 < self.entries.len() && self.entries[idx + 1].header.comp_solid {
            return true;
        }
        false
    }

    /// Decode all files in the solid chain up through `target_idx`,
    /// returning the data for `target_idx`.
    fn decode_solid_through(&mut self, target_idx: usize) -> RarResult<Vec<u8>> {
        let chain_start = self.find_solid_chain_start(target_idx);

        // If we've already decoded past this point, and it's a forward request, reuse state.
        // If we need to go backwards, reset.
        if self.solid_decoded_through >= chain_start as isize
            && self.solid_decoded_through < target_idx as isize
        {
            // Continue from where we left off
        } else if self.solid_decoded_through >= target_idx as isize {
            // Already decoded this file — but we don't cache the output,
            // so we must restart from the beginning.
            self.solid_state = None;
            self.solid_decoded_through = -1;
        } else {
            // Starting fresh
            self.solid_state = None;
            self.solid_decoded_through = -1;
        }

        // Determine dict_size from the first compressed entry in the chain
        if self.solid_state.is_none() {
            let dict_size = self.member_dict_window(chain_start)?;
            self.solid_state = Some(DecoderState::new(dict_size));
        }

        let start_from = (self.solid_decoded_through + 1) as usize;
        let mut target_data = Vec::new();

        for i in start_from..=target_idx {
            let entry = self.entries[i].clone();
            if entry.is_dir() {
                continue;
            }

            // Solid members decode through the streaming decoder (bounded by
            // the dictionary, not the member size): the buffered decoder
            // reconstructs the whole member in the sliding window, which
            // cannot hold more than the dictionary size. Intermediate
            // members are decoded into a discard sink so the shared window
            // advances.
            let mut state = self.solid_state.take().unwrap();
            let mut data = Vec::new();
            let written = self.decode_file_to(i, &mut data, Some(&mut state))?;
            let _ = written;
            self.solid_state = Some(state);

            self.solid_decoded_through = i as isize;

            if i == target_idx {
                target_data = data;
            }
        }

        Ok(target_data)
    }

    /// Find the start index of the solid chain containing `target_idx`
    /// (the first non-directory file at or before it that is not solid,
    /// followed by solid files).
    fn find_solid_chain_start(&self, target_idx: usize) -> usize {
        let mut chain_start = target_idx;
        for i in (0..target_idx).rev() {
            if self.entries[i].is_dir() {
                continue;
            }
            if self.entries[i].header.comp_solid || self.is_solid_chain_member(i) {
                chain_start = i;
            } else {
                break;
            }
        }
        chain_start
    }

    /// Streaming variant of [`Self::decode_solid_through`]: decodes the
    /// chain up to `target_idx`, writing only the target member to
    /// `writer` (intermediate members are decoded to a discard sink so the
    /// shared window advances).
    fn decode_solid_through_to(
        &mut self,
        target_idx: usize,
        writer: &mut dyn Write,
    ) -> RarResult<u64> {
        let chain_start = self.find_solid_chain_start(target_idx);

        if self.solid_decoded_through >= chain_start as isize
            && self.solid_decoded_through < target_idx as isize
        {
            // Continue from where we left off.
        } else {
            // Restart the chain when we need to go backwards or start fresh.
            self.solid_state = None;
            self.solid_decoded_through = -1;
        }

        if self.solid_state.is_none() {
            let dict_size = self.member_dict_window(chain_start)?;
            self.solid_state = Some(DecoderState::new(dict_size));
        }

        let start_from = (self.solid_decoded_through + 1) as usize;
        let mut target_written = 0u64;
        let mut discard = io::sink();

        for i in start_from..=target_idx {
            let entry = self.entries[i].clone();
            if entry.is_dir() {
                continue;
            }
            let sink: &mut dyn Write = if i == target_idx {
                writer
            } else {
                &mut discard
            };
            let mut state = self.solid_state.take().unwrap();
            let written = self.decode_file_to(i, sink, Some(&mut state))?;
            self.solid_state = Some(state);
            self.solid_decoded_through = i as isize;
            if i == target_idx {
                target_written = written;
            }
        }

        Ok(target_written)
    }

    /// Read packed data for an entry, potentially across multiple volumes.
    ///
    /// The returned payload is decrypted (when applicable) together with
    /// the derived keys needed for integrity verification.
    fn read_packed_data(&mut self, idx: usize) -> RarResult<DecryptedPayload> {
        let entry = &self.entries[idx];
        let hdr = &entry.header;
        let chunks = &entry.chunks;
        let max_packed = self.max_packed_bytes();

        let mut total_packed = 0u64;
        for c in chunks {
            total_packed =
                total_packed
                    .checked_add(c.packed_size)
                    .ok_or_else(|| RarError::LimitExceeded {
                        limit: max_packed,
                        context: format!("{}: packed size overflow", hdr.name),
                    })?;
            if total_packed > max_packed {
                return Err(RarError::LimitExceeded {
                    limit: max_packed,
                    context: format!(
                        "{}: packed data {total_packed} bytes exceeds limit",
                        hdr.name
                    ),
                });
            }
        }

        let mut packed_data = Vec::new();
        packed_data
            .try_reserve_exact(total_packed as usize)
            .map_err(|_| RarError::LimitExceeded {
                limit: max_packed,
                context: format!("{}: cannot allocate packed data", hdr.name),
            })?;

        for chunk in chunks {
            let chunk_start = packed_data.len();
            if chunk.volume_index == 0 {
                let stream = self.stream.as_mut().unwrap();
                stream.seek(SeekFrom::Start(chunk.data_offset))?;
                let mut limited = stream.take(chunk.packed_size);
                limited.read_to_end(&mut packed_data)?;
            } else {
                let mut f = File::open(&self.volume_paths[chunk.volume_index])?;
                f.seek(SeekFrom::Start(chunk.data_offset))?;
                let mut limited = f.take(chunk.packed_size);
                limited.read_to_end(&mut packed_data)?;
            }

            // Verify intermediate chunk CRC (packed data CRC)
            if !chunk.is_final
                && let Some(expected_crc) = chunk.crc32_val
            {
                let actual_crc = crc32fast::hash(&packed_data[chunk_start..]);
                if actual_crc != expected_crc {
                    return Err(RarError::Crc {
                        expected: expected_crc,
                        actual: actual_crc,
                        context: format!("{} vol {}", hdr.name, chunk.volume_index),
                    });
                }
            }
        }

        // Decrypt if encrypted, deriving keys once for both decryption and
        // integrity verification.
        let params = if !hdr.extra_data.is_empty() {
            encryption::parse_encryption_extra(&hdr.extra_data)?
        } else {
            None
        };
        let keys = if let Some(ref p) = params {
            let password = self.password.as_ref().ok_or_else(|| {
                RarError::Encrypted(format!("{}: encrypted, no password set", hdr.name))
            })?;
            if !p.verify_password(password) {
                return Err(RarError::WrongPassword);
            }
            let keys = p.derive_keys(password)?;
            let mut data = encryption::decrypt_data(&packed_data, &keys.key, &p.iv)?;
            if hdr.comp_method == COMP_METHOD_STORE {
                data.truncate(hdr.unpacked_size as usize);
            }
            packed_data = data;
            Some(keys)
        } else {
            None
        };

        Ok(DecryptedPayload {
            data: packed_data,
            params,
            keys,
        })
    }

    /// Maximum packed bytes accepted for one member. Bounded by the
    /// configured unpacked limit plus a small overhead, or a hard 8 GiB
    /// guard when unlimited.
    fn max_packed_bytes(&self) -> u64 {
        self.extract_options
            .max_unpacked_bytes
            .map(|u| u.saturating_add(1 << 20))
            .unwrap_or(8 * 1024 * 1024 * 1024)
    }

    /// Decode a single file into memory, optionally with a shared
    /// DecoderState (solid archives), verifying CRC32/BLAKE2sp.
    fn decode_file_at(
        &mut self,
        idx: usize,
        state: Option<&mut DecoderState>,
    ) -> RarResult<Vec<u8>> {
        self.validate_entry_limits(idx)?;
        let hdr = &self.entries[idx].header;

        // Empty files / directories
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(Vec::new());
        }

        let payload = self.read_packed_data(idx)?;
        let hdr = &self.entries[idx].header;

        let raw_data = if hdr.comp_method == COMP_METHOD_STORE {
            payload.data
        } else {
            compression::decompress(
                &payload.data,
                hdr.comp_method,
                hdr.unpacked_size,
                hdr.comp_dict_size,
                state,
            )
            .map_err(RarError::Unsupported)?
        };

        let crc = crc32fast::hash(&raw_data);
        let blake = self.entries[idx]
            .header
            .hash_value
            .map(|_| crate::blake2sp::hash(&raw_data));
        self.verify_integrity(
            idx,
            crc,
            blake,
            payload.params.as_ref(),
            payload.keys.as_ref(),
        )?;
        Ok(raw_data)
    }

    /// Decode a single file, streaming output to `writer` (bounded memory),
    /// verifying CRC32/BLAKE2sp over the written bytes.
    /// Actual dictionary size of a member in bytes: RAR5 uses
    /// `128 KiB << comp_dict_size`, RAR7 carries the byte count directly
    /// (possibly non-power-of-two). The sliding window rounds up to a
    /// power of two. Enforces the extraction dictionary cap
    /// (`ExtractOptions::max_dict_size`, WinRAR's `-mdx`).
    fn member_dict_window(&self, idx: usize) -> RarResult<usize> {
        let hdr = &self.entries[idx].header;
        let bytes = match hdr.dict_size_bytes {
            Some(b) => b,
            None => (128u64 * 1024) << hdr.comp_dict_size,
        };
        if let Some(cap) = self.extract_options.max_dict_size
            && bytes > cap
        {
            return Err(RarError::LimitExceeded {
                limit: cap,
                context: format!(
                    "{}: dictionary size {bytes} bytes exceeds the extraction cap (use -mdx to raise it)",
                    hdr.name
                ),
            });
        }
        let bytes = usize::try_from(bytes)
            .map_err(|_| RarError::Format("dictionary size overflows host address space".into()))?;
        bytes
            .checked_next_power_of_two()
            .ok_or_else(|| RarError::Format("dictionary size overflows host address space".into()))
    }

    fn decode_file_to(
        &mut self,
        idx: usize,
        writer: &mut dyn Write,
        state: Option<&mut DecoderState>,
    ) -> RarResult<u64> {
        self.validate_entry_limits(idx)?;
        let hdr = &self.entries[idx].header;
        let _ = self.member_dict_window(idx)?; // enforces the -mdx cap
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(0);
        }

        let payload = self.read_packed_data(idx)?;
        let hdr = &self.entries[idx].header;
        let mut sink = IntegritySink::new(writer, self.entries[idx].header.hash_value.is_some());

        let written = if hdr.comp_method == COMP_METHOD_STORE {
            sink.write_all(&payload.data).map_err(RarError::Io)?;
            payload.data.len() as u64
        } else {
            crate::codec::decode_to_writer(
                &payload.data,
                hdr.unpacked_size,
                crate::codec::DecodeOptions {
                    dict_size_log: hdr.comp_dict_size,
                    dict_size_bytes: hdr.dict_size_bytes,
                    extra_dist: hdr.comp_version == 1,
                    state,
                },
                &mut sink,
            )
            .map_err(RarError::Unsupported)?
        };

        let (crc, blake) = sink.finish();
        self.verify_integrity(
            idx,
            crc,
            blake,
            payload.params.as_ref(),
            payload.keys.as_ref(),
        )?;
        Ok(written)
    }

    /// Verify CRC32 and BLAKE2sp integrity of decoded data. Encrypted
    /// members use the hash-key MAC when the encryption record requests it.
    fn verify_integrity(
        &self,
        idx: usize,
        crc: u32,
        blake: Option<[u8; 32]>,
        params: Option<&encryption::EncryptionParams>,
        keys: Option<&encryption::DerivedKeys>,
    ) -> RarResult<()> {
        let hdr = &self.entries[idx].header;
        verify_integrity_for(hdr, crc, blake, params, keys)
    }

    /// [`crate::headers::read_block`].
    fn archive_block_key(&self) -> Option<[u8; 32]> {
        let encr = self.archive_encr.as_ref()?;
        let password = self.password.as_ref()?;
        Some(encr.get_key(password))
    }

}

/// Wraps a writer and counts the bytes written through it.
struct CountingWriter<'a> {
    inner: &'a mut dyn Write,
    written: u64,
}

impl<'a> CountingWriter<'a> {
    fn new(inner: &'a mut dyn Write) -> Self {
        Self { inner, written: 0 }
    }

    fn written(&self) -> u64 {
        self.written
    }
}

impl Write for CountingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Wraps a writer and reports `(bytes_written, total)` through a progress
/// callback after every write.
struct ProgressWriter<'a> {
    inner: &'a mut dyn Write,
    total: u64,
    written: u64,
    cb: &'a mut dyn FnMut(u64, u64),
}

impl Write for ProgressWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        (self.cb)(self.written, self.total);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// CRC32 sink for the streaming probe pass.
struct CrcSink<'a>(&'a mut crc32fast::Hasher);

impl Write for CrcSink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A member's payload in transit: plaintext passthrough or on-the-fly
/// AES-256-CBC encryption.
#[allow(clippy::large_enum_variant)] // the emitter holds an AES cipher + carry buffer
enum PayloadStream {
    Plain,
    Encrypted(CbcRangeEmitter),
}

fn payload_stream(key_iv: &Option<([u8; 32], [u8; 16])>) -> PayloadStream {
    match key_iv {
        Some((key, iv)) => PayloadStream::Encrypted(CbcRangeEmitter::new(key, iv)),
        None => PayloadStream::Plain,
    }
}

impl PayloadStream {
    /// Stream the payload bytes for plaintext range `[start, end)` of a
    /// member with `plain_len` plaintext bytes to `sink`. The bytes
    /// emitted are the member's on-disk data (the ciphertext when
    /// encrypted). Ranges must be issued in ascending order covering
    /// `[0, packed_len)` exactly.
    fn emit_to(
        &mut self,
        reader: &mut File,
        plain_len: u64,
        start: u64,
        end: u64,
        sink: &mut dyn Write,
    ) -> RarResult<()> {
        match self {
            PayloadStream::Plain => {
                reader.seek(SeekFrom::Start(start))?;
                let mut remaining = end - start;
                let mut buf = vec![0u8; 1 << 20];
                while remaining > 0 {
                    let want = buf.len().min(remaining as usize);
                    let mut filled = 0usize;
                    while filled < want {
                        let n = reader.read(&mut buf[filled..want])?;
                        if n == 0 {
                            return Err(RarError::Io(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                format!(
                                    "file changed size while being archived: still missing {remaining} bytes"
                                ),
                            )));
                        }
                        filled += n;
                    }
                    sink.write_all(&buf[..want]).map_err(RarError::Io)?;
                    remaining -= want as u64;
                }
                Ok(())
            }
            PayloadStream::Encrypted(emitter) => {
                emitter.emit_to(reader, plain_len, start, end, sink)
            }
        }
    }
}

/// Emits one member's continuous AES-256-CBC ciphertext in arbitrary byte
/// ranges. RAR5 volume chunks split the member's ciphertext at arbitrary
/// boundaries (WinRAR's volumes are byte-exact), so the encryptor — which
/// only produces complete 16-byte blocks — reads the plaintext ahead to
/// the next block boundary and carries the produced-but-unemitted tail
/// bytes (≤ 15) over to the following range.
struct CbcRangeEmitter {
    enc: encryption::Aes256CbcStream,
    /// Ciphertext bytes already produced but belonging to a later range.
    carry: Vec<u8>,
    /// Plaintext position consumed by the encryptor (block-aligned).
    consumed: u64,
}

impl CbcRangeEmitter {
    fn new(key: &[u8; 32], iv: &[u8; 16]) -> Self {
        Self {
            enc: encryption::Aes256CbcStream::new(key, iv),
            carry: Vec::new(),
            consumed: 0,
        }
    }

    /// Emit the ciphertext for plaintext range `[start, end)` of a member
    /// with `plain_len` plaintext bytes, zero-padding the member's final
    /// partial block (RAR5 padding). Emits exactly `end - start` bytes.
    fn emit_to(
        &mut self,
        reader: &mut File,
        plain_len: u64,
        start: u64,
        end: u64,
        sink: &mut dyn Write,
    ) -> RarResult<()> {
        // Bounded sub-ranges keep the read-ahead buffer at ~1 MiB even for
        // multi-GiB volume chunks; the carry keeps the stream continuous.
        const SUB: u64 = 1 << 20;
        let mut pos = start;
        while pos < end {
            let sub_end = (pos + SUB).min(end);
            let read_end = sub_end.div_ceil(16) * 16;
            let mut out = std::mem::take(&mut self.carry);
            if self.consumed < read_end {
                let total = (read_end - self.consumed) as usize;
                let mut buf = vec![0u8; total];
                let want = plain_len.saturating_sub(self.consumed).min(total as u64) as usize;
                if want > 0 {
                    reader.seek(SeekFrom::Start(self.consumed))?;
                    let mut filled = 0usize;
                    while filled < want {
                        let n = reader.read(&mut buf[filled..want])?;
                        if n == 0 {
                            return Err(RarError::Io(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                format!(
                                    "file changed size while being archived: expected {plain_len} plaintext bytes"
                                ),
                            )));
                        }
                        filled += n;
                    }
                }
                self.enc.encrypt_in_place(&mut buf)?;
                out.extend_from_slice(&buf);
                self.consumed = read_end;
            }
            let keep = (read_end - sub_end) as usize;
            let split = out.len() - keep;
            self.carry = out.split_off(split);
            sink.write_all(&out).map_err(RarError::Io)?;
            pos = sub_end;
        }
        Ok(())
    }
}

/// Removes a temporary spill file on drop (covers every error path).
struct SpillGuard(PathBuf);

impl Drop for SpillGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Temporary spill file for the streaming compressed path, kept next to
/// the archive being written.
fn spill_path_for(archive_path: &Path) -> PathBuf {
    let name = archive_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_string());
    archive_path.with_file_name(format!(".{name}.rar5spill-{}", temp_suffix()))
}

impl Drop for RarArchive {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Verify CRC32 and BLAKE2sp integrity against a file header. Encrypted
/// members use the hash-key MAC when the encryption record requests it.
fn verify_integrity_for(
    hdr: &FileHeader,
    crc: u32,
    blake: Option<[u8; 32]>,
    params: Option<&encryption::EncryptionParams>,
    keys: Option<&encryption::DerivedKeys>,
) -> RarResult<()> {
    let uses_mac = params.is_some_and(|p| p.uses_hash_mac());

    if let Some(expected) = hdr.crc32_val {
        let mut actual = crc;
        if uses_mac {
            let keys = keys.ok_or_else(|| {
                RarError::Encrypted(format!(
                    "{}: missing derived keys for MAC verification",
                    hdr.name
                ))
            })?;
            actual = keys.mac_crc32(actual);
        }
        if actual != expected {
            return Err(RarError::Crc {
                expected,
                actual,
                context: hdr.name.clone(),
            });
        }
    }

    if let (Some(expected), Some(actual)) = (hdr.hash_value, blake) {
        let actual = if uses_mac {
            let keys = keys.ok_or_else(|| {
                RarError::Encrypted(format!(
                    "{}: missing derived keys for hash MAC verification",
                    hdr.name
                ))
            })?;
            keys.mac_hash32(actual)
        } else {
            actual
        };
        if !encryption::constant_time_eq(&expected, &actual) {
            return Err(RarError::HashMismatch {
                expected,
                actual,
                context: hdr.name.clone(),
            });
        }
    }
    Ok(())
}

/// Byte offset where the RAR5 archive begins inside an SFX file (the end
/// of the embedded stub). Returns `None` when no signature is found.
pub fn sfx_offset_of(input: &[u8]) -> Option<usize> {
    find_bytes(input, RAR5_SIGNATURE)
}

/// Find the first occurrence of `needle` in `haystack`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}


/// WinRAR 7.23 dictionary selection for a non-solid member: the requested
/// dictionary (`-md`, or the default 32 MiB at every compression level) is
/// capped at twice the file size rounded down to a power of two, floored at
/// 128 KiB, and clamped to the RAR5 range (128 KiB .. 4 GiB, log 0..15).
fn dict_log_for(data_size: usize, requested: Option<u8>, _level: u8) -> u8 {
    let base = 128 * 1024;
    let file_pow2 = 1usize << (usize::BITS - 1 - data_size.max(1).leading_zeros());
    let auto_cap = (file_pow2 * 2).max(base);
    let requested_bytes = requested.map_or(32 * 1024 * 1024, |log| base << log);
    let target = auto_cap.min(requested_bytes);
    let mut log = 0u8;
    while (base << log) < target && log < 15 {
        log += 1;
    }
    log
}

/// WinRAR 7.23 dictionary selection for one member, covering both RAR5
/// (v50) and RAR7 (v70) creation. Returns `(encoder_window_log,
/// header_dict_bytes)`:
///
/// - `header_dict_bytes = None`: a plain RAR5 member; the log drives both
///   the header `comp_dict_size` field and the encoder window.
/// - `header_dict_bytes = Some(b)`: a RAR7 member whose header declares an
///   actual dictionary of `b` bytes (possibly not a power of two, WinRAR's
///   `-md` above 4 GiB). The encoder window stays bounded — match
///   distances are chunk-limited anyway — only the header declares the
///   large dictionary.
///
/// Like WinRAR, a > 4 GiB request is still capped at twice the file size
/// rounded down to a power of two; when the cap lands in the RAR5 range
/// the member is written as plain v50 with the capped log.
fn dict_params_for(
    data_size: usize,
    requested_log: Option<u8>,
    requested_bytes: Option<u64>,
    level: u8,
) -> (u8, Option<u64>) {
    let Some(requested) = requested_bytes else {
        return (dict_log_for(data_size, requested_log, level), None);
    };
    let base = 128 * 1024;
    let file_pow2 = 1usize << (usize::BITS - 1 - data_size.max(1).leading_zeros());
    let auto_cap = file_pow2.saturating_mul(2).max(base);
    let capped = (requested as usize).min(auto_cap);
    if capped > 4 * 1024 * 1024 * 1024 {
        // RAR7 (v70): the header declares the big dictionary; the encoder
        // window follows the plain RAR5 selection rules.
        (
            dict_log_for(data_size, requested_log, level),
            Some(capped as u64),
        )
    } else {
        // The 2x-file-size cap fell into the RAR5 range: plain v50 member
        // with the capped dictionary.
        let mut log = 0u8;
        while (base << log) < capped && log < 15 {
            log += 1;
        }
        (log, None)
    }
}

/// Sample-probe large inputs for incompressibility.
///
/// Compressing small samples with the same method costs ~20 ms per sample
/// and reliably identifies media/archives/random data, which would
/// otherwise spend minutes in the match finder only to end up STORE
/// anyway. The 90% threshold is conservative: genuinely compressible
/// inputs (text, code, structured binary) compress the samples far below
/// it. Sampling the head plus quarter points catches files whose tails are
/// incompressible (e.g. text + random media), which the old head-only
/// probe missed. A file is only STOREd when at least half of the samples
/// are incompressible, so files with a small random section keep
/// compressing.
const SAMPLE_PROBE_HEAD: usize = 512 * 1024;
const SAMPLE_PROBE_TAIL: usize = 256 * 1024;
const SAMPLE_REPEAT_STEP: usize = 16;
const SAMPLE_REPEAT_MIN_MATCH: usize = 64;

/// In-memory stride probe (used by `add_bytes`).
fn sample_is_incompressible(data: &[u8], method: u8) -> bool {
    if data.len() < 4 * SAMPLE_PROBE_HEAD {
        return false;
    }
    let mut bad = 0;
    if incompressible_sample(&data[..SAMPLE_PROBE_HEAD], method) {
        bad += 1;
    }
    let mut samples: Vec<&[u8]> = Vec::new();
    for &pos in &[data.len() / 4, data.len() / 2, data.len() * 3 / 4] {
        if pos >= SAMPLE_PROBE_HEAD
            && pos + SAMPLE_PROBE_TAIL <= data.len()
            && incompressible_sample(&data[pos..pos + SAMPLE_PROBE_TAIL], method)
        {
            bad += 1;
        }
        if pos + SAMPLE_PROBE_TAIL <= data.len() {
            samples.push(&data[pos..pos + SAMPLE_PROBE_TAIL]);
        }
    }
    // A file whose random-looking regions repeat each other (e.g. a
    // backup with a distant copy of a random block) is compressible via
    // long-range matching — the raw incompressibility vote must not
    // STORE it. Such regions are byte-identical, which no sampling
    // density can distinguish from plain randomness.
    if bad >= 2 && samples_have_distant_repeats(&data[..SAMPLE_PROBE_HEAD], &samples) {
        return false;
    }
    bad >= 2
}

/// File-based stride probe: head + samples at the quarter points.
fn sample_is_incompressible_file(path: &Path, size: u64, method: u8) -> RarResult<bool> {
    let mut f = File::open(path)?;
    let mut head = vec![0u8; SAMPLE_PROBE_HEAD];
    let n = read_up_to(&mut f, &mut head)?;
    let mut bad = 0;
    if incompressible_sample(&head[..n], method) {
        bad += 1;
    }
    let mut samples: Vec<Vec<u8>> = Vec::new();
    for &quarter in &[size / 4, size / 2, size * 3 / 4] {
        if quarter < SAMPLE_PROBE_HEAD as u64 {
            continue;
        }
        f.seek(SeekFrom::Start(quarter))?;
        let mut sample = vec![0u8; SAMPLE_PROBE_TAIL];
        let n = read_up_to(&mut f, &mut sample)?;
        if n > 0 && incompressible_sample(&sample[..n], method) {
            bad += 1;
        }
        if n > 0 {
            samples.push(sample[..n].to_vec());
        }
    }
    // Same long-range-repeat escape hatch as the in-memory probe.
    let slices: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
    if bad >= 2 && samples_have_distant_repeats(&head[..n], &slices) {
        return Ok(false);
    }
    Ok(bad >= 2)
}

/// Detect byte-identical repeats between the head sample and the quarter
/// samples (sampled every [`SAMPLE_REPEAT_STEP`] bytes, requiring at
/// least [`SAMPLE_REPEAT_MIN_MATCH`] equal bytes). Used to avoid STORE
/// for files whose incompressible-looking regions are distant copies of
/// each other — compressible through the long-range match finder.
fn samples_have_distant_repeats(head: &[u8], samples: &[&[u8]]) -> bool {
    use std::collections::HashMap;
    let mut regions: Vec<&[u8]> = Vec::with_capacity(samples.len() + 1);
    regions.push(head);
    regions.extend(samples.iter().copied());
    for i in 0..regions.len() {
        let a = regions[i];
        if a.len() < SAMPLE_REPEAT_STEP + SAMPLE_REPEAT_MIN_MATCH {
            continue;
        }
        // Hash every SAMPLE_REPEAT_STEP-th 4-byte window of region a.
        let mut hashes: HashMap<u32, usize> =
            HashMap::with_capacity(a.len() / SAMPLE_REPEAT_STEP);
        let mut off = 0;
        while off + 4 <= a.len() {
            let h = (a[off] as u32)
                | ((a[off + 1] as u32) << 8)
                | ((a[off + 2] as u32) << 16)
                | ((a[off + 3] as u32) << 24);
            hashes.insert(h.wrapping_mul(0x9E3779B1), off);
            off += SAMPLE_REPEAT_STEP;
        }
        for b in &regions[i + 1..] {
            let mut off = 0;
            while off + 4 <= b.len() {
                let h = (b[off] as u32)
                    | ((b[off + 1] as u32) << 8)
                    | ((b[off + 2] as u32) << 16)
                    | ((b[off + 3] as u32) << 24);
                if let Some(&a_off) = hashes.get(&h.wrapping_mul(0x9E3779B1)) {
                    // Verify a real run of equal bytes (hash collisions
                    // must not count).
                    let limit = SAMPLE_REPEAT_MIN_MATCH
                        .min(a.len() - a_off)
                        .min(b.len() - off);
                    let mut len = 0;
                    while len < limit && a[a_off + len] == b[off + len] {
                        len += 1;
                    }
                    if len >= SAMPLE_REPEAT_MIN_MATCH {
                        return true;
                    }
                }
                off += SAMPLE_REPEAT_STEP;
            }
        }
    }
    false
}

fn incompressible_sample(sample: &[u8], method: u8) -> bool {
    if sample.is_empty() {
        return false;
    }
    let packed = compression::compress(sample, method, 0).unwrap_or_default();
    packed.len() >= sample.len() * 9 / 10
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// Deterministic pseudo-random bytes (LCG) — incompressible.
    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    #[test]
    fn probe_recognizes_distant_copy_as_compressible() {
        // 8 MiB of random data followed by its exact copy: the probe
        // samples are all random, but the distant repeat means the file
        // compresses via long-range matching — it must NOT be STOREd.
        let half = 4 * 1024 * 1024usize;
        let mut data = pseudo_random(half, 42);
        data.extend_from_slice(&data.clone());
        assert!(
            !sample_is_incompressible(&data, 3),
            "distant copy must not be probed as incompressible"
        );
    }

    #[test]
    fn probe_stores_pure_random() {
        let data = pseudo_random(8 * 1024 * 1024, 7);
        assert!(
            sample_is_incompressible(&data, 3),
            "pure random must be probed as incompressible"
        );
    }

    #[test]
    fn probe_leaves_compressible_data_alone() {
        // Text-like data compresses far below the 90% threshold.
        let mut data = b"the quick brown fox jumps over the lazy dog "
            .iter()
            .copied()
            .cycle()
            .take(8 * 1024 * 1024)
            .collect::<Vec<u8>>();
        data.extend_from_slice(&data.clone());
        assert!(!sample_is_incompressible(&data, 3));
    }
}

/// Compute the plaintext CRC32 (and optional BLAKE2sp) of a file in a
/// single streaming pass.
fn hash_file(path: &Path, size: u64, want_blake: bool) -> RarResult<(u32, Option<[u8; 32]>)> {
    let mut crc = crc32fast::Hasher::new();
    let mut blake = want_blake.then(crate::blake2sp::Hasher::new);
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        crc.update(&buf[..n]);
        if let Some(h) = blake.as_mut() {
            h.update(&buf[..n]);
        }
    }
    if total != size {
        return Err(RarError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("file changed size while being hashed: expected {size} bytes, read {total}"),
        )));
    }
    Ok((crc.finalize(), blake.map(|h| h.finalize())))
}

/// Read until `buf` is full or EOF; returns the number of bytes read.
fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Sanitize an archive member name for safe extraction.
///
/// Rejects empty names, absolute paths, `..` traversal components, NUL
/// bytes and Windows drive/ADS components (`:`). Backslashes are treated
/// as separators and redundant `.`/empty components are dropped.
fn sanitize_archive_path(name: &str) -> RarResult<String> {
    if name.is_empty() {
        return Err(RarError::Security("empty entry name".into()));
    }
    if name.contains('\0') {
        return Err(RarError::Security("entry name contains a NUL byte".into()));
    }
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(RarError::Security(format!(
            "absolute entry name {name:?} rejected"
        )));
    }
    let mut out = String::new();
    for comp in normalized.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            return Err(RarError::Security(format!(
                "entry name {name:?} contains a '..' traversal component"
            )));
        }
        if comp.contains(':') {
            return Err(RarError::Security(format!(
                "entry name {name:?} contains a ':' (drive/ADS) component"
            )));
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(comp);
    }
    if out.is_empty() {
        return Err(RarError::Security(format!(
            "entry name {name:?} resolves to an empty path"
        )));
    }
    Ok(out)
}

/// Unique suffix for temporary sibling files/volumes. WASI provides no
/// `std::process::id`, so derive uniqueness from the monotonic counter and
/// the system clock instead.
fn temp_suffix() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{nanos:x}{counter:x}")
}

/// Build a unique temporary sibling path for atomic extraction.
fn temp_sibling_path(dest_path: &Path) -> PathBuf {
    let file_name = dest_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "entry".to_string());
    let tmp_name = format!(".{file_name}.rar5tmp-{}", temp_suffix());
    dest_path.with_file_name(tmp_name)
}

/// Open a file for both reading and writing, truncating it. The archive
/// stream is read back for locator patches and recovery records, and
/// Windows `File::create` opens write-only (`GENERIC_WRITE`), so the
/// write path uses a read+write handle.
fn read_write_create(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

/// Replace `dest` with `src` (atomic on Unix; falls back to remove+rename
/// on platforms where rename over an existing file fails).
fn replace_file(src: &Path, dest: &Path) -> RarResult<()> {
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(_) if dest.exists() => {
            fs::remove_file(dest)?;
            fs::rename(src, dest)?;
            Ok(())
        }
        Err(e) => Err(RarError::Io(e)),
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

    // Match .partN.rar naming
    if let Some(base) = extract_volume_base(&name) {
        let parent = path.parent().unwrap_or(Path::new("."));
        let mut volumes = Vec::new();
        let mut n = 1;
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
    }

    vec![path.to_path_buf()]
}

/// Extract volume base from a filename like `archive.part3.rar` → `archive`.
fn extract_volume_base(name: &str) -> Option<String> {
    // Case-insensitive match for .partN.rar
    let lower = name.to_lowercase();
    if let Some(idx) = lower.find(".part") {
        let after = &lower[idx + 5..];
        if let Some(rar_idx) = after.find(".rar") {
            let num_str = &after[..rar_idx];
            if num_str.chars().all(|c| c.is_ascii_digit()) && !num_str.is_empty() {
                return Some(name[..idx].to_string());
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
    if let Some(base) = extract_volume_base(name) {
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

pub(crate) fn get_volume_base(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive");
    if let Some(base) = extract_volume_base(name) {
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

#[cfg(test)]
mod tests {
    #![allow(deprecated)] // tests exercise the legacy constructor family
    use super::*;

    #[test]
    fn encrypted_store_roundtrip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("rar");
        let data = b"Hello, encrypted world!";
        {
            let mut ar = RarArchive::create_with_password(&path, "secret").unwrap();
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
            let mut ar = RarArchive::create_with_password(&path, "pw").unwrap();
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
            let mut ar = RarArchive::create_with_password(&path, "right").unwrap();
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
            let mut ar = RarArchive::create_with_password(&path, "multi").unwrap();
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
            let mut ar =
                RarArchive::create_multivolume_with_recovery_count(&base, 32768, 10).unwrap();
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
            let mut ar =
                RarArchive::create_multivolume_with_recovery_count(&base, 32768, 2).unwrap();
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
            let mut ar = RarArchive::create_multivolume_with_recovery(&base, 32768, 20).unwrap();
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
            let mut ar = RarArchive::create_with_recovery(&path, 5).unwrap();
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
            let mut ar = RarArchive::create_with_recovery(&path, 5).unwrap();
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
        for i in s1..s2 + gc {
            damaged[i] ^= 0xFF;
        }
        let repaired = crate::recovery::rar5::repair_inline_recovery_archive(&damaged).unwrap();
        assert_eq!(repaired, raw, "relocated repair must restore the original bytes");
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
            let mut ar = RarArchive::create_with_recovery(&path, 5).unwrap();
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
        for i in s1..s2 + gc {
            damaged[i] ^= 0xFF;
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
            let mut ar =
                RarArchive::create_with_password_headers_recovery(&path, "pw", 10).unwrap();
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
            let mut ar = RarArchive::create_with_password_headers(&path, "hdr-pw").unwrap();
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
            let mut ar = RarArchive::create_with_password_headers(&path, "pw").unwrap();
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
        while let Ok(Some(meta)) = crate::headers::read_block(&mut cursor, None) {
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
            let mut ar = RarArchive::create_multivolume(&path, 30000).unwrap();
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
            let mut ar = RarArchive::create_multivolume(&path, 30000).unwrap();
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
            let mut ar = RarArchive::create_multivolume_with_password_headers(&path, 30_000, "pw")
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
            let mut ar = RarArchive::create_multivolume(&path, 20000).unwrap();
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
            let mut ar = RarArchive::create_multivolume(&path, 30000).unwrap();
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
}


