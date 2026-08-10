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

/// Maximum archive prefix buffered for inline recovery-record parity.
/// Streamed recovery records are not implemented yet; larger archives must
/// create recovery records without `recovery_percent`.
const MAX_RECOVERY_PREFIX_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// Maximum accepted RAR5 dictionary-size log (1 GiB, the WinRAR 5.x
/// maximum). Larger values are rejected at decode time to bound window
/// allocations.
const MAX_DICT_SIZE_LOG: u8 = 13;

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
}

/// Decrypted member payload plus the key material needed for integrity
/// verification.
struct DecryptedPayload {
    data: Vec<u8>,
    params: Option<encryption::EncryptionParams>,
    keys: Option<encryption::DerivedKeys>,
}

/// Byte span of one block in the archive being rewritten, with its parsed
/// (plaintext) header and the exact on-disk header bytes.
struct BlockMeta {
    block_type: u64,
    flags: u64,
    /// Absolute offset where the block starts (the CRC32 field).
    block_start: u64,
    /// Absolute offset where the data area starts (right after the header;
    /// for header-encrypted archives after the IV + ciphertext).
    data_offset: u64,
    /// Absolute offset one past the end of the block.
    data_end: u64,
    /// Exact bytes of the header as stored on disk: `[CRC32][size vint]
    /// [body]`, or `[IV][ciphertext]` for header-encrypted archives.
    header_bytes: Vec<u8>,
    /// Length of the size vint inside the plaintext header.
    hsize_vint_len: usize,
    raw: RawBlock,
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
    stream: Option<File>,
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
    /// Options for the current read/extract operation (set per call).
    extract_options: crate::options::ExtractOptions,
}

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

        let first = self
            .read_next_block(&mut reader)?
            .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
        let main_meta = match first.block_type {
            BLOCK_TYPE_ENCRYPT_HEADER => {
                let params = parse_archive_encrypt_header(&first.raw)?;
                let password = self.password.as_ref().ok_or_else(|| {
                    RarError::Encrypted("archive has encrypted headers; provide a password".into())
                })?;
                if !params.verify_password(password) {
                    return Err(RarError::Encrypted("wrong password".into()));
                }
                self.archive_encr = Some(params);
                self.header_encryption = true;
                self.read_next_block(&mut reader)?
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
            return Err(RarError::Unsupported("archive is locked".into()));
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
        while let Some(meta) = self.read_next_block(&mut reader)? {
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
        self.stream = Some(f);
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
        let first = self
            .read_next_block(&mut reader)?
            .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
        let main_meta = match first.block_type {
            BLOCK_TYPE_ENCRYPT_HEADER => {
                let params = parse_archive_encrypt_header(&first.raw)?;
                let password = self.password.as_ref().ok_or_else(|| {
                    RarError::Encrypted("archive has encrypted headers; provide a password".into())
                })?;
                if !params.verify_password(password) {
                    return Err(RarError::Encrypted("wrong password".into()));
                }
                self.archive_encr = Some(params);
                self.header_encryption = true;
                self.read_next_block(&mut reader)?
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

        self.stream = Some(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)?,
        );
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
        self.stream = Some(File::create(&tmp_path)?);
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
        if opts.solid && opts.volume_size.is_some() {
            return Err(RarError::Unsupported(
                "solid archives with multiple volumes are not supported yet".into(),
            ));
        }
        if opts.encrypt_headers && opts.password.as_deref().is_none_or(|pw| pw.is_empty()) {
            return Err(RarError::Encrypted(
                "header encryption requires a password".into(),
            ));
        }
        if opts.encrypt_headers && opts.volume_size.is_some() {
            return Err(RarError::Unsupported(
                "header encryption is not supported for multi-volume archives".into(),
            ));
        }
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

        let path = path.as_ref().to_path_buf();
        let mut archive = RarArchive {
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
            extract_options: crate::options::ExtractOptions::default(),
        };
        archive.open_write()?;
        Ok(archive)
    }

    /// Create a new RAR5 archive (overwrites existing file).
    pub fn create(path: impl AsRef<Path>) -> RarResult<Self> {
        Self::create_with_options(path, crate::options::CreateOptions::default())
    }

    /// Create a new multi-volume RAR5 archive.
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

    /// Create a new encrypted RAR5 archive (overwrites existing file).
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
    pub fn create_with_recovery(path: impl AsRef<Path>, percent: u8) -> RarResult<Self> {
        Self::create_with_password_recovery(path, "", percent)
    }

    /// Create a new encrypted RAR5 archive with an inline recovery record.
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
            self.stream = Some(f);
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
            let f = File::create(&vol_path)?;
            self.stream = Some(f);
            self.write_signature()?;
            self.write_archive_header_vol(None)?;
            self.volume_bytes_written = self.stream.as_ref().unwrap().stream_position()?;
            return Ok(());
        }

        let f = File::create(&self.path)?;
        self.stream = Some(f);
        self.write_signature()?;
        if self.header_encryption {
            // The archive-level encryption header precedes the main archive
            // header; every header after it (main, file, end) is encrypted.
            let password = self.password.as_ref().ok_or_else(|| {
                RarError::Encrypted("header encryption requires a password".into())
            })?;
            let encr =
                encryption::EncryptionParams::generate_for_password(password, ENCR_PBKDF2_ITER_LOG);
            let block = encr.to_archive_header_block();
            let stream = self.stream.as_mut().unwrap();
            stream.write_all(&block)?;
            self.archive_encr = Some(encr);
        }
        self.write_archive_header()?;
        Ok(())
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
        if self.stream.is_some() && (self.mode == Mode::Write || self.mode == Mode::Append) {
            let qo_offset = if self.quick_open {
                Some(self.write_quick_open_record()?)
            } else {
                None
            };
            let rr_offset = if self.recovery_percent.is_some() {
                Some(self.stream.as_ref().unwrap().stream_position()?)
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
        self.stream = None;
        if self.recovery_volumes_percent.is_some() || self.recovery_volumes_count.is_some() {
            self.write_recovery_volumes()?;
        }
        Ok(())
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
        let mut body = Vec::new();
        body.extend(vint::encode(0x03u64)); // service header
        body.extend(vint::encode(
            BLOCK_FLAG_EXTRA_DATA | BLOCK_FLAG_DATA_AREA | BLOCK_FLAG_SKIP_IF_UNKNOWN,
        ));
        let subdata = {
            let rec = vec![percent as u8]; // recovery percent (single byte, <= 100)
            let mut extra = Vec::new();
            extra.extend(vint::encode((1 + rec.len()) as u64)); // record size: type + data
            extra.extend(vint::encode(0x07u64)); // service data record type
            extra.extend(rec);
            extra
        };
        body.extend(vint::encode(subdata.len() as u64)); // extra area size
        body.extend(vint::encode(rr_data.len() as u64)); // data size
        body.extend(vint::encode(0u64)); // file flags
        body.extend(vint::encode(rr_data.len() as u64)); // unpacked size
        body.extend(vint::encode(0u64)); // attributes
        body.extend(vint::encode(0u64)); // compression info (store)
        body.extend(vint::encode(OS_UNIX));
        body.extend(vint::encode(2u64)); // name length
        body.extend(b"RR");
        body.extend(subdata);

        let size_bytes = vint::encode(body.len() as u64);
        let mut header_content = Vec::with_capacity(size_bytes.len() + body.len());
        header_content.extend(&size_bytes);
        header_content.extend(&body);
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header_content);
        let crc = hasher.finalize();
        let mut hdr = Vec::with_capacity(4 + header_content.len());
        hdr.extend(crc.to_le_bytes());
        hdr.extend(header_content);

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
        let mut body = Vec::new();
        body.extend(vint::encode(0x03u64)); // service header
        body.extend(vint::encode(
            BLOCK_FLAG_EXTRA_DATA | BLOCK_FLAG_DATA_AREA | BLOCK_FLAG_SKIP_IF_UNKNOWN,
        ));
        let mut extra = Vec::new();
        extra.extend(vint::encode(1u64)); // record size: type only
        extra.extend(vint::encode(0x07u64)); // service data record type
        body.extend(vint::encode(extra.len() as u64)); // extra area size
        body.extend(vint::encode(payload.len() as u64)); // data size
        body.extend(vint::encode(0u64)); // file flags
        body.extend(vint::encode(payload.len() as u64)); // unpacked size
        body.extend(vint::encode(0u64)); // attributes
        body.extend(vint::encode(0u64)); // compression info (store)
        body.extend(vint::encode(OS_UNIX));
        body.extend(vint::encode(2u64)); // name length
        body.extend(b"QO");
        body.extend(extra);

        let size_bytes = vint::encode(body.len() as u64);
        let mut header_content = Vec::with_capacity(size_bytes.len() + body.len());
        header_content.extend(&size_bytes);
        header_content.extend(&body);
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header_content);
        let crc = hasher.finalize();
        let mut hdr = Vec::with_capacity(4 + header_content.len());
        hdr.extend(crc.to_le_bytes());
        hdr.extend(header_content);

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

        // Rebuild the main header: read it back (plaintext or decrypted).
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
            let mut reader = std::fs::File::open(&self.path)?;
            let mut iv = [0u8; 16];
            reader.seek(SeekFrom::Start(start))?;
            reader.read_exact(&mut iv)?;
            // Decrypt the first block to learn the header size.
            let mut first = [0u8; 16];
            reader.read_exact(&mut first)?;
            let first_pt = encryption::decrypt_data(&first, &key, &iv)?;
            let (hsize, vint_len) = vint::decode_from_slice(&first_pt, 4)
                .map_err(|e| RarError::Format(format!("main header vint: {e}")))?;
            let total_raw = 4 + vint_len + hsize as usize;
            let enc_size = total_raw.div_ceil(16) * 16;
            let mut full_ct = vec![0u8; enc_size];
            full_ct[..16].copy_from_slice(&first);
            if enc_size > 16 {
                reader.read_exact(&mut full_ct[16..])?;
            }
            let full_pt = encryption::decrypt_data(&full_ct, &key, &iv)?;
            full_pt[..total_raw].to_vec()
        } else {
            let mut reader = std::fs::File::open(&self.path)?;
            reader.seek(SeekFrom::Start(start))?;
            // Read the whole header: parse the size first.
            let mut crc_hdr = [0u8; 5];
            reader.read_exact(&mut crc_hdr)?;
            let (hsize, vint_len) = vint::decode_from_slice(&crc_hdr, 4)
                .map_err(|e| RarError::Format(format!("main header vint: {e}")))?;
            let total = 4 + vint_len + hsize as usize;
            let mut hdr = vec![0u8; total];
            hdr[..5].copy_from_slice(&crc_hdr);
            reader.read_exact(&mut hdr[5..])?;
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

        let stream = self.stream.as_mut().unwrap();

        loop {
            let raw = match RawBlock::read_from(stream) {
                Ok(b) => b,
                Err(RarError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            };

            let stream_pos = stream.stream_position()?;

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
                }
                BLOCK_TYPE_END_ARCHIVE => break,
                BLOCK_TYPE_ENCRYPT_HEADER => {
                    return self.scan_encrypted_blocks(&raw);
                }
                _ => {}
            }

            if raw.data_size > 0 {
                stream.seek(SeekFrom::Start(raw.data_offset + raw.data_size))?;
            }
        }

        Ok(())
    }

    /// Parse the archive-level encryption header and scan all encrypted blocks.
    ///
    /// In header-encrypted archives, each block after the encryption header is:
    /// `[16-byte IV] [AES-256-CBC encrypted header, padded to 16B] [file data if any]`
    fn scan_encrypted_blocks(&mut self, encrypt_raw: &RawBlock) -> RarResult<()> {
        let password = self.password.as_ref().ok_or_else(|| {
            RarError::Encrypted("archive has encrypted headers; provide a password".into())
        })?;

        // Parse the encryption header to get salt, strength, etc.
        let encr_params = parse_archive_encrypt_header(encrypt_raw)?;

        if !encr_params.verify_password(password) {
            return Err(RarError::Encrypted("wrong password".into()));
        }

        let key = encr_params.get_key(password);
        let stream = self.stream.as_mut().unwrap();

        loop {
            // Each encrypted block: [16-byte IV] [encrypted header padded to 16B]
            let mut iv = [0u8; 16];
            match stream.read_exact(&mut iv) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }

            // Read first 16 encrypted bytes to determine header size
            let mut first_block = [0u8; 16];
            stream.read_exact(&mut first_block)?;

            let first_pt = encryption::decrypt_data(&first_block, &key, &iv)?;

            // Parse CRC and header size from decrypted data
            let _crc = u32::from_le_bytes(first_pt[0..4].try_into().unwrap());
            let (hdr_size, vint_len) = vint::decode_from_slice(&first_pt, 4)
                .map_err(|e| RarError::Format(format!("encrypted block vint: {e}")))?;

            if hdr_size == 0 || hdr_size > 2 * 1024 * 1024 {
                return Err(RarError::Format(format!(
                    "implausible encrypted header size: {hdr_size}"
                )));
            }

            // Total raw bytes = CRC(4) + vint + header_body, padded to 16B
            let total_raw = 4 + vint_len + hdr_size as usize;
            let enc_size = total_raw.div_ceil(16) * 16;

            // Read remaining encrypted blocks (we already have the first 16)
            let mut full_ct = vec![0u8; enc_size];
            full_ct[..16].copy_from_slice(&first_block);
            if enc_size > 16 {
                stream.read_exact(&mut full_ct[16..])?;
            }

            // Decrypt the full header
            let full_pt = encryption::decrypt_data(&full_ct, &key, &iv)?;

            // Extract just the header data (skip CRC + vint)
            let header_data = full_pt[4 + vint_len..4 + vint_len + hdr_size as usize].to_vec();

            // Verify CRC
            let size_bytes = vint::encode(hdr_size);
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&size_bytes);
            hasher.update(&header_data);
            let computed_crc = hasher.finalize();
            if computed_crc != _crc {
                return Err(RarError::Crc {
                    expected: _crc,
                    actual: computed_crc,
                    context: "encrypted block header".into(),
                });
            }

            // Parse block type and flags from header_data
            let mut offset = 0;
            let (block_type, n) = vint::decode_from_slice(&header_data, offset)
                .map_err(|e| RarError::Format(format!("block type: {e}")))?;
            offset += n;
            let (flags, n) = vint::decode_from_slice(&header_data, offset)
                .map_err(|e| RarError::Format(format!("block flags: {e}")))?;
            offset += n;

            let mut _extra_size = 0u64;
            let mut data_size = 0u64;
            if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
                let (v, n) = vint::decode_from_slice(&header_data, offset)
                    .map_err(|e| RarError::Format(format!("extra size: {e}")))?;
                _extra_size = v;
                offset += n;
            }
            if flags & BLOCK_FLAG_DATA_AREA != 0 {
                let (v, n) = vint::decode_from_slice(&header_data, offset)
                    .map_err(|e| RarError::Format(format!("data size: {e}")))?;
                data_size = v;
                offset += n;
            }
            let _ = offset;

            // Build a RawBlock so we can reuse existing header parsers
            let raw = RawBlock {
                header_crc: _crc,
                header_data,
                data_size,
                data_offset: stream.stream_position()?,
                block_type,
                flags,
            };

            match block_type {
                BLOCK_TYPE_ARCHIVE_HEADER => {
                    let _ah = ArchiveHeader::from_raw(&raw)?;
                }
                BLOCK_TYPE_FILE_HEADER => {
                    let fh = FileHeader::from_raw(&raw, raw.data_offset)?;
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
                }
                BLOCK_TYPE_END_ARCHIVE => break,
                _ => {}
            }

            // Skip file data area if present
            if data_size > 0 {
                stream.seek(SeekFrom::Current(data_size as i64))?;
            }
        }

        Ok(())
    }

    /// Scan all volumes of a multi-volume archive.
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

            loop {
                let raw = match RawBlock::read_from(&mut stream) {
                    Ok(b) => b,
                    Err(RarError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e),
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
                                    // Final chunk
                                    let total_packed: u64 =
                                        entry.chunks.iter().map(|c| c.packed_size).sum();
                                    entry.header.packed_size = total_packed;
                                    entry.header.crc32_val = fh.crc32_val;
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
                        if eoa.flags & END_FLAG_NEXT_VOLUME == 0 {
                            break;
                        } else {
                            break; // continue to next volume
                        }
                    }
                    BLOCK_TYPE_ENCRYPT_HEADER => {
                        return Err(RarError::Unsupported(
                            "header-encrypted multi-volume archives not yet supported".into(),
                        ));
                    }
                    _ => {}
                }

                if raw.data_size > 0 {
                    stream.seek(SeekFrom::Start(raw.data_offset + raw.data_size))?;
                }
            }
        }

        // Keep the first volume open as the default stream
        self.stream = Some(File::open(&self.volume_paths[0])?);
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

        let main_header_start = self.stream.as_ref().unwrap().stream_position()?;
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
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(&hdr.to_bytes())?;
        Ok(())
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
        let f = File::create(&vol_path)?;
        self.stream = Some(f);
        self.write_signature()?;
        // Volume number: part2 → 1, part3 → 2, etc.
        let vol_num = (self.current_volume - 1) as u64;
        self.write_archive_header_vol(Some(vol_num))?;
        self.volume_bytes_written = self.stream.as_ref().unwrap().stream_position()?;
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
            .ok_or_else(|| RarError::Format(format!("member not found: {name:?}")))?;
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
                        crate::codec::decode_standalone(
                            &payload.data,
                            hdr.unpacked_size,
                            hdr.comp_dict_size,
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
                let mut mtime =
                    UNIX_EPOCH + std::time::Duration::from_secs(entry.header.mtime as u64);
                if let Some(ns) = entry.header.mtime_ns {
                    mtime += std::time::Duration::from_nanos(ns as u64);
                }
                let times = std::fs::FileTimes::new().set_modified(mtime);
                let _ = std::fs::File::options()
                    .write(true)
                    .open(&dest_path)
                    .and_then(|f| f.set_times(times));
            }
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
            .ok_or_else(|| RarError::Format(format!("member not found: {name:?}")))?;
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

        let dest_path = self.safe_dest_path(dest_dir, &entry.header.name)?;

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
                let _ = fs::remove_file(&tmp_path);
                return Err(e);
            }
        }

        // Restore mtime (best-effort), including the nanosecond fraction
        // from the FILE_TIME extra record when present.
        if entry.header.mtime != 0 || entry.header.mtime_ns.is_some() {
            let mut mtime = UNIX_EPOCH + std::time::Duration::from_secs(entry.header.mtime as u64);
            if let Some(ns) = entry.header.mtime_ns {
                mtime += std::time::Duration::from_nanos(ns as u64);
            }
            let times = std::fs::FileTimes::new().set_modified(mtime);
            let _ = std::fs::File::options()
                .write(true)
                .open(&dest_path)
                .and_then(|f| f.set_times(times));
        }

        Ok(dest_path)
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
            let dict_log = self.entries[chain_start].header.comp_dict_size;
            let dict_size = (128usize * 1024)
                .checked_shl(dict_log as u32)
                .ok_or_else(|| {
                    RarError::Format("dictionary size overflows host address space".into())
                })?;
            self.solid_state = Some(DecoderState::new(dict_size));
        }

        let start_from = (self.solid_decoded_through + 1) as usize;
        let mut target_data = Vec::new();

        for i in start_from..=target_idx {
            let entry = self.entries[i].clone();
            if entry.is_dir() {
                continue;
            }

            // RAR5 solid: temporarily take state to satisfy borrow checker
            let mut state = self.solid_state.take().unwrap();
            let data = self.decode_file_at(i, Some(&mut state))?;
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
            let dict_log = self.entries[chain_start].header.comp_dict_size;
            let dict_size = (128usize * 1024)
                .checked_shl(dict_log as u32)
                .ok_or_else(|| {
                    RarError::Format("dictionary size overflows host address space".into())
                })?;
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
                return Err(RarError::Encrypted("wrong password".into()));
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
    fn decode_file_to(
        &mut self,
        idx: usize,
        writer: &mut dyn Write,
        state: Option<&mut DecoderState>,
    ) -> RarResult<u64> {
        self.validate_entry_limits(idx)?;
        let hdr = &self.entries[idx].header;
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
                hdr.comp_dict_size,
                state,
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

    // ── Public API: deletion ───────────────────────────────────────────────

    /// Delete members from a RAR5 archive without rebuilding the whole
    /// archive.
    ///
    /// The archive is rewritten surgically: every block before the first
    /// deleted member is copied verbatim, and for non-solid archives every
    /// kept member after the deletion point is copied verbatim too (file
    /// header and compressed payload included) — only the main header
    /// locator, the quick-open record and the end block are re-emitted.
    /// This matches the official `rar d`, which never recompresses
    /// non-solid archives.
    ///
    /// Solid archives: when the first deleted member belongs to a solid
    /// chain, the members after it reference the removed data, so the whole
    /// chain is decoded and recompressed from its start; everything before
    /// the chain start is still copied verbatim (the official `rar d`
    /// recompresses the entire archive in this case).
    ///
    /// Multi-volume archives: kept members keep their exact compressed
    /// payloads but are re-split at the volume size limit, and `.rev`
    /// recovery volumes are regenerated (the official `rar` CLI refuses to
    /// modify multi-volume archives at all).
    ///
    /// Other behaviors:
    /// - inline recovery records are rebuilt over the rewritten archive,
    ///   so `rar r` can still repair it (the official `rar d` drops them);
    /// - the quick-open record is rebuilt from the kept members;
    /// - when every member is deleted, the archive is erased;
    /// - with the `parallel` feature, verbatim block data is prefetched by
    ///   a background thread, overlapping reads with writes and with the
    ///   solid-chain recompression.
    ///
    /// Returns the number of deleted members. Fails with
    /// [`RarError::Format`] when any requested name is not present, and
    /// with [`RarError::Unsupported`] for locked archives.
    pub fn delete(&mut self, names: &[&str]) -> RarResult<usize> {
        if self.mode != Mode::Read {
            return Err(RarError::Format(
                "delete requires an archive opened for reading".into(),
            ));
        }
        let mut deleted = vec![false; self.entries.len()];
        let mut count = 0usize;
        for name in names {
            let idx = self
                .entries
                .iter()
                .enumerate()
                .find(|(i, e)| e.name() == *name && !deleted[*i])
                .map(|(i, _)| i)
                .ok_or_else(|| RarError::Format(format!("member not found: {name:?}")))?;
            deleted[idx] = true;
            count += 1;
        }
        if count == 0 {
            return Err(RarError::Format("no members to delete".into()));
        }

        if count == self.entries.len() {
            // Matching `rar d`: an archive whose every member is deleted is
            // erased entirely (every volume for multi-volume archives).
            if self.main_header_is_locked()? {
                return Err(RarError::Unsupported("archive is locked".into()));
            }
            for vol in &self.volume_paths {
                let _ = fs::remove_file(vol);
            }
            self.stream = None;
            self.entries.clear();
            self.solid_state = None;
            self.solid_decoded_through = -1;
            return Ok(count);
        }

        let first_deleted = deleted.iter().position(|d| *d).unwrap();
        let chain = self.chain_range_around(first_deleted);

        if self.volume_paths.len() > 1 {
            // The official `rar` CLI refuses to modify multi-volume
            // archives ("Cannot modify volume"); we re-split the volumes
            // instead (superset).
            self.rewrite_multivolume(&deleted, chain, None)?;
        } else {
            let src_path = self.path.clone();
            let tmp_path = temp_sibling_path(&src_path);
            let mut reader = File::open(&src_path)?;
            self.stream = Some(File::create(&tmp_path)?);
            self.quick_open_entries.clear();
            // Rewriting rediscovers header encryption from the file itself.
            self.header_encryption = false;
            self.archive_encr = None;

            let result = self.rewrite_blocks(
                &mut reader,
                &deleted,
                chain,
                None,
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
        }

        self.mode = Mode::Read;
        self.solid_state = None;
        self.solid_decoded_through = -1;
        self.open_read()?;
        Ok(count)
    }

    /// Rewrite a multi-volume archive, omitting deleted members.
    ///
    /// Kept members keep their exact compressed payloads but are re-split
    /// at the volume size limit (the official `rar` CLI refuses to modify
    /// multi-volume archives at all; this matches WinRAR's rebuild
    /// behavior). Solid chains are decoded and recompressed like in the
    /// single-volume path. Trailing QO/RR service records are dropped and
    /// `.rev` recovery volumes are regenerated.
    fn rewrite_multivolume(
        &mut self,
        deleted: &[bool],
        chain: Option<(usize, usize)>,
        rename_map: Option<&std::collections::HashMap<usize, String>>,
    ) -> RarResult<()> {
        if self.header_encryption {
            return Err(RarError::Unsupported(
                "deleting from header-encrypted multi-volume archives is not supported".into(),
            ));
        }
        let orig_volumes = self.volume_paths.clone();
        let mut vol_sizes = Vec::with_capacity(orig_volumes.len());
        for vol in &orig_volumes {
            vol_sizes.push(fs::metadata(vol)?.len());
        }
        // Every volume except the last is exactly the size limit.
        let volume_size = *vol_sizes[..vol_sizes.len() - 1]
            .iter()
            .min()
            .unwrap_or(&vol_sizes[0]);
        if volume_size == 0 {
            return Err(RarError::Format(
                "cannot rewrite: volume size is zero".into(),
            ));
        }

        let base = get_volume_base(&self.path);
        let parent = self.path.parent().unwrap_or(Path::new(".")).to_path_buf();
        // Write to a temporary volume base and rename over the originals
        // only after every volume succeeded (a failure never destroys the
        // original set).
        let tmp_base = format!(".{base}.rar5tmp-{}", std::process::id());
        let tmp_base_path = parent.join(&tmp_base);

        // Write the new volume set. Swapping `self.path` makes
        // start_next_volume create the temporary volume names.
        let saved_path = self.path.clone();
        self.path = tmp_base_path;
        self.volume_size = Some(volume_size);
        self.volume_paths = Vec::new();
        self.current_volume = 1;
        self.volume_bytes_written = 0;
        self.stream = Some(File::create(volume_path(&parent, &tmp_base, 1))?);
        self.write_signature()?;
        self.write_archive_header_vol(None)?;
        self.volume_bytes_written = self.stream.as_ref().unwrap().stream_position()?;

        let mut readers = VolumeReaders::new(&orig_volumes);
        let (mut dec, mut enc, mut enc_active) = (None, None, false);
        let mut in_chain = false;
        let mut chain_end = usize::MAX;
        for (idx, entry) in self.entries.clone().iter().enumerate() {
            if !in_chain
                && let Some((s, e)) = chain
                && s == idx
            {
                let dict_log = self.entries[idx].header.comp_dict_size;
                let dict_size =
                    (128usize * 1024)
                        .checked_shl(dict_log as u32)
                        .ok_or_else(|| {
                            RarError::Format("dictionary size overflows host address space".into())
                        })?;
                dec = Some(DecoderState::new(dict_size));
                enc = Some(crate::codec::EncoderState::default());
                enc_active = false;
                in_chain = true;
                chain_end = e;
            }
            let is_chain = in_chain && idx <= chain_end;
            if is_chain && idx == chain_end {
                in_chain = false;
            }

            if deleted[idx] {
                if is_chain && !entry.is_dir() && entry.header.comp_method != COMP_METHOD_STORE {
                    // Advance the chain window.
                    let _ =
                        self.decode_chain_member_volumes(&mut readers, idx, dec.as_mut().unwrap())?;
                }
                continue;
            }
            let entry_name = rename_map
                .and_then(|m| m.get(&idx))
                .cloned()
                .unwrap_or_else(|| entry.header.name.clone());
            if entry.is_dir() {
                let fh = FileHeader {
                    name: entry_name.clone(),
                    attributes: entry.header.attributes,
                    mtime: entry.header.mtime,
                    host_os: OS_UNIX,
                    file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_DIRECTORY,
                    is_directory: true,
                    ..Default::default()
                };
                let hdr_bytes = fh.to_bytes();
                self.write_block_header(&hdr_bytes)?;
                continue;
            }
            if is_chain && entry.header.comp_method != COMP_METHOD_STORE {
                self.recompress_chain_member_volumes_named(
                    &mut readers,
                    idx,
                    &entry_name,
                    dec.as_mut().unwrap(),
                    enc.as_mut().unwrap(),
                    &mut enc_active,
                )?;
                continue;
            }

            // Verbatim payload, re-split across the new volumes.
            let payload = self.read_packed_volumes(&mut readers, idx)?;
            let hdr = &entry.header;
            self.write_file_entry(
                &entry_name,
                hdr.unpacked_size,
                &payload,
                hdr.crc32_val.unwrap_or(0),
                hdr.comp_method,
                hdr.comp_dict_size,
                &hdr.extra_data,
                hdr.attributes,
                hdr.mtime,
                hdr.comp_solid,
                hdr.hash_value,
            )?;
        }
        self.write_end_block()?;
        self.stream = None;
        self.volume_size = None;
        self.path = saved_path;

        // Move the new volumes into place, drop stale ones, and regenerate
        // `.rev` recovery volumes when the original set had any.
        let result = (|| -> RarResult<()> {
            let mut final_volumes: Vec<PathBuf> = Vec::new();
            for n in 1..=orig_volumes.len() {
                let tmp = volume_path(&parent, &tmp_base, n);
                let final_path = volume_path(&parent, &base, n);
                let exists = fs::metadata(&tmp).map(|m| m.len() > 0).unwrap_or(false);
                if !exists {
                    let _ = fs::remove_file(&tmp);
                    continue;
                }
                replace_file(&tmp, &final_path)?;
                final_volumes.push(final_path);
            }
            // Drop any stale volumes beyond the new set.
            for n in final_volumes.len() + 1..=orig_volumes.len() {
                let _ = fs::remove_file(volume_path(&parent, &base, n));
            }
            self.volume_paths = final_volumes;

            let rev = parent.join(format!("{base}.part1.rev"));
            if rev.exists() {
                let (rec_count, _data_count) = rev_params_from_file(&rev)?;
                // Drop every stale `.rev` file, then regenerate the set.
                let mut n = 1u32;
                loop {
                    let old_rev = parent.join(format!("{base}.part{n}.rev"));
                    if old_rev.exists() {
                        let _ = fs::remove_file(old_rev);
                        n += 1;
                    } else {
                        break;
                    }
                }
                self.recovery_volumes_count = Some(rec_count.min(self.volume_paths.len() as u32));
                self.recovery_volumes_percent = None;
                self.write_recovery_volumes()?;
            }
            Ok(())
        })();
        if result.is_err() {
            for n in 1..=orig_volumes.len() {
                let _ = fs::remove_file(volume_path(&parent, &tmp_base, n));
            }
        }
        result
    }

    /// Read the full packed (and decrypted, when applicable) payload of a
    /// multi-volume member across its chunks on the original volumes.
    fn read_packed_volumes(
        &mut self,
        readers: &mut VolumeReaders,
        idx: usize,
    ) -> RarResult<Vec<u8>> {
        let entry = &self.entries[idx];
        let hdr = &entry.header;
        let mut packed = Vec::new();
        let total: u64 = entry.chunks.iter().map(|c| c.packed_size).sum();
        packed
            .try_reserve_exact(total as usize)
            .map_err(|_| RarError::LimitExceeded {
                limit: self.max_packed_bytes(),
                context: format!("{}: cannot allocate packed data", hdr.name),
            })?;
        for chunk in &entry.chunks {
            let chunk_start = packed.len();
            packed.extend(readers.read_chunk(
                chunk.volume_index,
                chunk.data_offset,
                chunk.packed_size,
            )?);
            if !chunk.is_final
                && let Some(expected_crc) = chunk.crc32_val
            {
                let actual_crc = crc32fast::hash(&packed[chunk_start..]);
                if actual_crc != expected_crc {
                    return Err(RarError::Crc {
                        expected: expected_crc,
                        actual: actual_crc,
                        context: format!("{} vol {}", hdr.name, chunk.volume_index),
                    });
                }
            }
        }

        let params = if !hdr.extra_data.is_empty() {
            encryption::parse_encryption_extra(&hdr.extra_data)?
        } else {
            None
        };
        if let Some(ref p) = params {
            let password = self.password.as_ref().ok_or_else(|| {
                RarError::Encrypted(format!("{}: encrypted, no password set", hdr.name))
            })?;
            if !p.verify_password(password) {
                return Err(RarError::Encrypted("wrong password".into()));
            }
            let keys = p.derive_keys(password)?;
            let mut data = encryption::decrypt_data(&packed, &keys.key, &p.iv)?;
            if hdr.comp_method == COMP_METHOD_STORE {
                data.truncate(hdr.unpacked_size as usize);
            }
            packed = data;
        }
        Ok(packed)
    }

    /// Decode a multi-volume chain member with a shared decoder state,
    /// verifying its integrity.
    fn decode_chain_member_volumes(
        &mut self,
        readers: &mut VolumeReaders,
        idx: usize,
        state: &mut DecoderState,
    ) -> RarResult<Vec<u8>> {
        let hdr = &self.entries[idx].header;
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(Vec::new());
        }
        let payload = self.read_packed_volumes(readers, idx)?;
        let hdr = &self.entries[idx].header;
        let raw_data = if hdr.comp_method == COMP_METHOD_STORE {
            payload
        } else {
            compression::decompress(
                &payload,
                hdr.comp_method,
                hdr.unpacked_size,
                hdr.comp_dict_size,
                Some(state),
            )
            .map_err(RarError::Unsupported)?
        };
        let crc = crc32fast::hash(&raw_data);
        let blake = self.entries[idx]
            .header
            .hash_value
            .map(|_| crate::blake2sp::hash(&raw_data));
        let params = if !hdr.extra_data.is_empty() {
            encryption::parse_encryption_extra(&hdr.extra_data)?
        } else {
            None
        };
        let keys = if let Some(ref p) = params {
            let password = self.password.as_ref().ok_or_else(|| {
                RarError::Encrypted(format!("{}: encrypted, no password set", hdr.name))
            })?;
            Some(p.derive_keys(password)?)
        } else {
            None
        };
        self.verify_integrity(idx, crc, blake, params.as_ref(), keys.as_ref())?;
        Ok(raw_data)
    }

    /// Decode and recompress one member of the affected solid chain in a
    /// multi-volume archive. `name` overrides the entry name (rename).
    fn recompress_chain_member_volumes_named(
        &mut self,
        readers: &mut VolumeReaders,
        idx: usize,
        name: &str,
        dec: &mut DecoderState,
        enc: &mut crate::codec::EncoderState,
        enc_active: &mut bool,
    ) -> RarResult<()> {
        let entry = self.entries[idx].clone();
        let hdr = &entry.header;
        let data = self.decode_chain_member_volumes(readers, idx, dec)?;

        let plain_crc = crc32fast::hash(&data);
        let plain_blake = hdr.hash_value.map(|_| crate::blake2sp::hash(&data));
        let packed = compression::compress_chunked(
            &data,
            hdr.comp_method,
            hdr.comp_dict_size,
            crate::codec::DEFAULT_CHUNK_SIZE,
            Some(enc),
            true,
            None,
        )
        .map_err(RarError::Unsupported)?;

        let (method, dsl, payload) = if packed.len() >= data.len() {
            enc.reset();
            *enc_active = false;
            (COMP_METHOD_STORE, 0u8, data.clone())
        } else {
            *enc_active = true;
            (hdr.comp_method, hdr.comp_dict_size, packed)
        };
        let (header_crc, extra_data, stored_hash, encr_params) =
            RarArchive::payload_extra_and_crc(self.password.as_deref(), plain_crc, plain_blake)?;
        let payload = RarArchive::encrypt_payload_with(
            self.password.as_deref(),
            encr_params.as_ref(),
            &payload,
        )?;
        self.write_file_entry(
            name,
            data.len() as u64,
            &payload,
            header_crc,
            method,
            dsl,
            &extra_data,
            hdr.attributes,
            hdr.mtime,
            *enc_active,
            stored_hash,
        )?;
        Ok(())
    }

    /// Rename members in the archive (like `rar rn`).
    ///
    /// Each `(old, new)` pair renames the first member whose name equals
    /// `old`; renaming a directory also renames the matching prefix of
    /// every member beneath it. Kept members keep their exact compressed
    /// payloads — only the file headers are re-emitted (the quick-open
    /// record is rebuilt and, unlike the official `rar rn`, the recovery
    /// record is rebuilt so `rar r` can still repair the archive).
    ///
    /// Multi-volume archives are re-split like [`Self::delete`] (the
    /// official `rar rn` patches only the first chunk header in place).
    ///
    /// Returns the number of renamed members. Fails with
    /// [`RarError::Format`] when any source name is not present, and with
    /// [`RarError::Unsupported`] for locked archives.
    pub fn rename(&mut self, renames: &[(&str, &str)]) -> RarResult<usize> {
        if self.mode != Mode::Read {
            return Err(RarError::Format(
                "rename requires an archive opened for reading".into(),
            ));
        }
        if self.main_header_is_locked()? {
            return Err(RarError::Unsupported("archive is locked".into()));
        }

        // Resolve the pairs sequentially, honoring earlier renames in the
        // same call and expanding directory renames to their descendants.
        let mut map: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
        let mut count = 0usize;
        for (old, new) in renames {
            let old_norm = old.trim_end_matches('/').to_string();
            let idx = self
                .entries
                .iter()
                .enumerate()
                .find(|(i, e)| {
                    let name = map
                        .get(i)
                        .map(|n| n.as_str())
                        .unwrap_or(e.name())
                        .trim_end_matches('/');
                    name == old_norm
                })
                .map(|(i, _)| i)
                .ok_or_else(|| RarError::Format(format!("member not found: {old:?}")))?;
            let is_dir = self.entries[idx].is_dir();
            let new_norm = new.trim_end_matches('/').to_string();
            if is_dir {
                map.insert(idx, format!("{new_norm}/"));
                let prefix = format!("{old_norm}/");
                for (i, e) in self.entries.iter().enumerate() {
                    if i == idx || map.contains_key(&i) {
                        continue;
                    }
                    if let Some(rest) = e.name().strip_prefix(&prefix) {
                        map.insert(i, format!("{new_norm}/{rest}"));
                    }
                }
            } else {
                map.insert(idx, new_norm.clone());
            }
            count += 1;
        }

        if self.volume_paths.len() > 1 {
            let deleted = vec![false; self.entries.len()];
            self.rewrite_multivolume(&deleted, None, Some(&map))?;
        } else {
            let src_path = self.path.clone();
            let tmp_path = temp_sibling_path(&src_path);
            let mut reader = File::open(&src_path)?;
            self.stream = Some(File::create(&tmp_path)?);
            self.quick_open_entries.clear();
            self.header_encryption = false;
            self.archive_encr = None;

            let deleted = vec![false; self.entries.len()];
            let result = self.rewrite_blocks(
                &mut reader,
                &deleted,
                None,
                None,
                Some(&map),
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
        }

        self.mode = Mode::Read;
        self.solid_state = None;
        self.solid_decoded_through = -1;
        self.open_read()?;
        Ok(count)
    }

    /// Read the archive comment (the "CMT" service block), if any.
    ///
    /// Header-encrypted archives store the comment encrypted; reading it
    /// requires the password and is not supported yet.
    pub fn get_comment(&mut self) -> RarResult<Option<Vec<u8>>> {
        let mut reader = File::open(&self.path)?;
        reader.seek(SeekFrom::Start(self.sfx_offset + 8))?;
        while let Some(meta) = self.read_next_block(&mut reader)? {
            match meta.block_type {
                BLOCK_TYPE_END_ARCHIVE => break,
                BLOCK_TYPE_SERVICE_HEADER
                    if self.service_block_name(&meta)?.as_deref() == Some("CMT") =>
                {
                    let mut data = vec![0u8; meta.raw.data_size as usize];
                    reader.seek(SeekFrom::Start(meta.data_offset))?;
                    reader.read_exact(&mut data)?;
                    return Ok(Some(data));
                }
                _ => {}
            }
            // Advance past the data area.
            reader.seek(SeekFrom::Start(meta.data_end))?;
        }
        Ok(None)
    }

    /// Set the archive comment (like `rar c`); an empty comment removes
    /// the existing one.
    ///
    /// The comment is stored in a "CMT" service block right after the main
    /// header; the quick-open and recovery records are rebuilt over the
    /// rewritten archive. Multi-volume archives are not supported.
    pub fn set_comment(&mut self, comment: &[u8]) -> RarResult<()> {
        if self.mode != Mode::Read {
            return Err(RarError::Format(
                "set_comment requires an archive opened for reading".into(),
            ));
        }
        if self.volume_paths.len() > 1 {
            return Err(RarError::Unsupported(
                "archive comments are not supported for multi-volume archives".into(),
            ));
        }
        if self.main_header_is_locked()? {
            return Err(RarError::Unsupported("archive is locked".into()));
        }
        let src_path = self.path.clone();
        let tmp_path = temp_sibling_path(&src_path);
        let mut reader = File::open(&src_path)?;
        self.stream = Some(File::create(&tmp_path)?);
        self.quick_open_entries.clear();
        self.header_encryption = false;
        self.archive_encr = None;

        let deleted = vec![false; self.entries.len()];
        // `Some(empty)` removes the existing comment: the plan drops CMT
        // blocks and nothing new is written.
        let result = self.rewrite_blocks(
            &mut reader,
            &deleted,
            None,
            None,
            None,
            Some(comment),
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

    /// Parse the main archive header and report whether the archive is
    /// locked. Runs before any destructive step of [`Self::delete`] so the
    /// erase-everything path is covered too.
    fn main_header_is_locked(&mut self) -> RarResult<bool> {
        let mut reader = File::open(&self.path)?;
        reader.seek(SeekFrom::Start(self.sfx_offset + 8))?;
        self.header_encryption = false;
        self.archive_encr = None;
        let first = self
            .read_next_block(&mut reader)?
            .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
        let main = match first.block_type {
            BLOCK_TYPE_ENCRYPT_HEADER => {
                let params = parse_archive_encrypt_header(&first.raw)?;
                let password = self.password.as_ref().ok_or_else(|| {
                    RarError::Encrypted("archive has encrypted headers; provide a password".into())
                })?;
                if !params.verify_password(password) {
                    return Err(RarError::Encrypted("wrong password".into()));
                }
                self.archive_encr = Some(params);
                self.header_encryption = true;
                self.read_next_block(&mut reader)?
                    .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?
            }
            BLOCK_TYPE_ARCHIVE_HEADER => first,
            _ => {
                return Err(RarError::Format(
                    "archive is missing the main header".into(),
                ));
            }
        };
        let ah = ArchiveHeader::from_raw(&main.raw)?;
        Ok(ah.flags & ARCHIVE_FLAG_LOCKED != 0)
    }

    /// Index range `[s, e]` of the solid chain affected by deleting member
    /// `idx`, when one exists.
    ///
    /// A member joins its predecessor's window when its header carries the
    /// solid flag, so the chain extends backwards while the entry at the
    /// boundary is solid and forwards while the next entry is solid (the
    /// first member of a chain is not flagged solid, matching the writer).
    /// Deleting the last member of a chain leaves the earlier members
    /// decodable (their windows are untouched), so no chain needs to be
    /// recompressed in that case.
    fn chain_range_around(&self, idx: usize) -> Option<(usize, usize)> {
        let mut s = idx;
        while s > 0 && self.entries[s].header.comp_solid {
            s -= 1;
        }
        let mut e = idx;
        while e + 1 < self.entries.len() && self.entries[e + 1].header.comp_solid {
            e += 1;
        }
        (idx < e).then_some((s, e))
    }

    /// Rewrite the archive file, omitting deleted members.
    ///
    /// `reader` reads the original archive; the rewritten bytes go to
    /// `self.stream` (the replacement file). With the `parallel` feature,
    /// verbatim block data is prefetched by a background thread and the
    /// affected solid chain is recompressed while the tail is already
    /// being read. Inline recovery records are rebuilt when the original
    /// had one (the percentage is carried over).
    #[allow(clippy::too_many_arguments)] // mirrors the delete() state machine
    fn rewrite_blocks(
        &mut self,
        reader: &mut File,
        deleted: &[bool],
        chain: Option<(usize, usize)>,
        force_rr: Option<u8>,
        rename_map: Option<&std::collections::HashMap<usize, String>>,
        comment: Option<&[u8]>,
        src_path: &Path,
        tmp_path: &Path,
    ) -> RarResult<()> {
        let plan = self.plan_rewrite(reader, deleted, chain, force_rr, rename_map, comment)?;
        self.execute_rewrite(&plan, src_path, tmp_path)?;
        Ok(())
    }

    /// Walk the archive and build the rewrite plan: verbatim copies for
    /// every kept block, recompression ops for the affected solid chain,
    /// and the dropped QO/RR service records (the RR percentage is parsed
    /// so the record can be rebuilt).
    fn plan_rewrite(
        &mut self,
        reader: &mut File,
        deleted: &[bool],
        chain: Option<(usize, usize)>,
        force_rr: Option<u8>,
        rename_map: Option<&std::collections::HashMap<usize, String>>,
        comment: Option<&[u8]>,
    ) -> RarResult<RewritePlan> {
        // Signature (after any embedded SFX stub).
        let mut sig = [0u8; 8];
        reader.seek(SeekFrom::Start(self.sfx_offset))?;
        reader.read_exact(&mut sig)?;

        // Leading blocks: optional archive encryption header (plaintext),
        // then the main archive header (rebuilt so the locator stays
        // consistent with the rewritten archive).
        let mut encrypt_header = None;
        let first = self
            .read_next_block(reader)?
            .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
        let main_meta = match first.block_type {
            BLOCK_TYPE_ENCRYPT_HEADER => {
                let params = parse_archive_encrypt_header(&first.raw)?;
                let password = self.password.as_ref().ok_or_else(|| {
                    RarError::Encrypted("archive has encrypted headers; provide a password".into())
                })?;
                if !params.verify_password(password) {
                    return Err(RarError::Encrypted("wrong password".into()));
                }
                self.archive_encr = Some(params);
                self.header_encryption = true;
                encrypt_header = Some(first.header_bytes);
                let main = self
                    .read_next_block(reader)?
                    .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
                if main.block_type != BLOCK_TYPE_ARCHIVE_HEADER {
                    return Err(RarError::Format(
                        "archive is missing the main header".into(),
                    ));
                }
                main
            }
            BLOCK_TYPE_ARCHIVE_HEADER => first,
            _ => {
                return Err(RarError::Format(
                    "archive is missing the main header".into(),
                ));
            }
        };

        // Decide quick-open capture and recovery rebuild from the main
        // header (write_main_header re-derives them from the same data).
        let ah = ArchiveHeader::from_raw(&main_meta.raw)?;
        let (had_qo, _had_rr, _) = split_main_extra(&ah.extra_data)?;
        let capture_qo = had_qo && !self.header_encryption;

        let mut ops = Vec::new();
        let mut entry_idx = 0usize;
        let mut chain_active = false;
        let mut chain_end = usize::MAX;
        let mut rr_percent = force_rr;
        // Service blocks flagged as dependent on the previous block (e.g.
        // NTFS streams/ACLs) belong to their file: drop them when that file
        // was deleted.
        let mut prev_file_deleted = false;

        while let Some(meta) = self.read_next_block(reader)? {
            match meta.block_type {
                BLOCK_TYPE_END_ARCHIVE => break,
                BLOCK_TYPE_FILE_HEADER => {
                    let idx = entry_idx;
                    entry_idx += 1;
                    if !chain_active
                        && let Some((s, e)) = chain
                        && s == idx
                    {
                        chain_active = true;
                        chain_end = e;
                    }
                    if chain_active && idx <= chain_end {
                        let entry = &self.entries[idx];
                        if entry.is_dir() || entry.header.comp_method == COMP_METHOD_STORE {
                            // Directories and STORE members never
                            // participate in the LZ window.
                            if !deleted[idx] {
                                ops.push(RewriteOp::CopyBlock {
                                    qo_header: None,
                                    header_bytes: meta.header_bytes,
                                    src_data: meta.data_offset,
                                    len: meta.raw.data_size,
                                });
                            }
                        } else {
                            ops.push(RewriteOp::Recompress {
                                idx,
                                is_deleted: deleted[idx],
                            });
                        }
                        if idx == chain_end {
                            chain_active = false;
                        }
                        prev_file_deleted = deleted[idx];
                    } else if deleted[idx] {
                        prev_file_deleted = true;
                    } else {
                        let header_bytes = match rename_map.and_then(|m| m.get(&idx)) {
                            Some(new_name) => {
                                let mut fh = self.entries[idx].header.clone();
                                fh.name = new_name.clone();
                                fh.to_bytes()
                            }
                            None => meta.header_bytes,
                        };
                        ops.push(RewriteOp::CopyBlock {
                            qo_header: if capture_qo {
                                Some(header_bytes.clone())
                            } else {
                                None
                            },
                            header_bytes,
                            src_data: meta.data_offset,
                            len: meta.raw.data_size,
                        });
                        prev_file_deleted = false;
                    }
                }
                BLOCK_TYPE_SERVICE_HEADER => {
                    let name = self.service_block_name(&meta)?;
                    if name.as_deref() == Some("RR") && rr_percent.is_none() {
                        rr_percent = self.rr_percent_from_block(&meta);
                    }
                    let drops = name.as_deref() == Some("QO")
                        || name.as_deref() == Some("RR")
                        || (comment.is_some() && name.as_deref() == Some("CMT"))
                        || (meta.flags & BLOCK_FLAG_DEPENDS_PREV != 0 && prev_file_deleted);
                    if !drops {
                        ops.push(RewriteOp::CopyBlock {
                            qo_header: None,
                            header_bytes: meta.header_bytes,
                            src_data: meta.data_offset,
                            len: meta.raw.data_size,
                        });
                    }
                }
                _ => ops.push(RewriteOp::CopyBlock {
                    qo_header: None,
                    header_bytes: meta.header_bytes,
                    src_data: meta.data_offset,
                    len: meta.raw.data_size,
                }),
            }
            // Advance past the data area (headers are read separately).
            reader.seek(SeekFrom::Start(meta.data_end))?;
        }

        Ok(RewritePlan {
            ops,
            encrypt_header,
            main_meta,
            rr_percent: rr_percent.filter(|p| *p <= 100),
            comment: comment.map(|c| c.to_vec()),
        })
    }

    /// Write the plan to `self.stream`: signature, archive encryption
    /// header, rebuilt main header, then every op in order, and finally
    /// the quick-open record, the main header locator patch, the recovery
    /// record and the end block.
    fn execute_rewrite(
        &mut self,
        plan: &RewritePlan,
        src_path: &Path,
        tmp_path: &Path,
    ) -> RarResult<()> {
        let out = self.stream.as_mut().unwrap();
        if self.sfx_offset > 0 {
            // Preserve the embedded SFX stub of the original archive.
            let mut stub = File::open(src_path)?;
            stub.seek(SeekFrom::Start(0))?;
            let mut limited = stub.take(self.sfx_offset + RAR5_SIGNATURE.len() as u64);
            io::copy(&mut limited, out)?;
        } else {
            out.write_all(RAR5_SIGNATURE)?;
        }
        if let Some(ref enc) = plan.encrypt_header {
            out.write_all(enc)?;
        }
        let (main_start, qo_field_pos, rr_field_pos, main_hdr) =
            self.write_main_header(&plan.main_meta, plan.rr_percent)?;
        if let Some(ref comment) = plan.comment
            && !comment.is_empty()
        {
            let block = build_comment_block(comment);
            self.write_block_header(&block)?;
        }

        // Prefetch every verbatim block with a background reader when the
        // total volume justifies the thread (parallel feature).
        #[cfg(feature = "parallel")]
        let mut pipeline: Option<CopyPipeline> = None;
        #[cfg(not(feature = "parallel"))]
        let pipeline: Option<()> = None;
        #[cfg(feature = "parallel")]
        {
            const PARALLEL_MIN_COPY: u64 = 32 * 1024 * 1024;
            let total_copy: u64 = plan
                .ops
                .iter()
                .map(|op| match op {
                    RewriteOp::CopyBlock { len, .. } => *len,
                    RewriteOp::Recompress { .. } => 0,
                })
                .sum();
            if total_copy >= PARALLEL_MIN_COPY && plan.ops.len() >= 4 {
                let jobs: Vec<CopyJob> = plan
                    .ops
                    .iter()
                    .filter_map(|op| match op {
                        RewriteOp::CopyBlock { src_data, len, .. } => Some(CopyJob {
                            src: *src_data,
                            len: *len,
                        }),
                        RewriteOp::Recompress { .. } => None,
                    })
                    .collect();
                pipeline = Some(CopyPipeline::start(src_path, &jobs));
            }
        }
        let mut reader = File::open(src_path)?;
        let mut dec = None;
        let mut enc = None;
        let mut enc_active = false;

        for op in &plan.ops {
            match op {
                RewriteOp::CopyBlock {
                    header_bytes,
                    src_data,
                    len,
                    qo_header,
                } => {
                    let stream = self.stream.as_mut().unwrap();
                    let out_pos = stream.stream_position()?;
                    if let Some(qh) = qo_header {
                        self.quick_open_entries.push((out_pos, qh.clone()));
                    }
                    stream.write_all(header_bytes)?;
                    #[cfg_attr(not(feature = "parallel"), allow(unused_mut))]
                    let mut left = *len;
                    #[cfg(feature = "parallel")]
                    if let Some(pipe) = &pipeline {
                        while left > 0 {
                            let buf = pipe.take()?.ok_or_else(|| {
                                RarError::Io(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "copy pipeline ended early",
                                ))
                            })?;
                            self.stream.as_mut().unwrap().write_all(&buf)?;
                            left -= buf.len() as u64;
                        }
                    }
                    #[cfg(not(feature = "parallel"))]
                    let _ = &pipeline;
                    if left > 0 {
                        reader.seek(SeekFrom::Start(*src_data))?;
                        let mut limited = (&mut reader).take(left);
                        io::copy(&mut limited, self.stream.as_mut().unwrap())?;
                    }
                }
                RewriteOp::Recompress { idx, is_deleted } => {
                    if dec.is_none() {
                        let dict_log = self.entries[*idx].header.comp_dict_size;
                        let dict_size =
                            (128usize * 1024)
                                .checked_shl(dict_log as u32)
                                .ok_or_else(|| {
                                    RarError::Format(
                                        "dictionary size overflows host address space".into(),
                                    )
                                })?;
                        dec = Some(DecoderState::new(dict_size));
                        enc = Some(crate::codec::EncoderState::default());
                        enc_active = false;
                    }
                    self.recompress_chain_member(
                        &mut reader,
                        *idx,
                        *is_deleted,
                        dec.as_mut().unwrap(),
                        enc.as_mut().unwrap(),
                        &mut enc_active,
                    )?;
                }
            }
        }
        #[cfg(feature = "parallel")]
        if let Some(mut pipe) = pipeline {
            pipe.finish();
        }
        #[cfg(not(feature = "parallel"))]
        let _ = pipeline;

        // Quick-open record (rebuilt from the kept headers), locator patch
        // (with the recovery offset), recovery record and end block.
        let qo_pos = if self.quick_open {
            Some(self.write_quick_open_record()?)
        } else {
            None
        };
        let rr_pos = if self.recovery_percent.is_some() {
            Some(self.stream.as_ref().unwrap().stream_position()?)
        } else {
            None
        };
        if qo_pos.is_some() || rr_pos.is_some() {
            self.patch_main_header(
                qo_pos,
                rr_pos,
                main_start,
                qo_field_pos,
                rr_field_pos,
                &main_hdr,
            )?;
        }
        if rr_pos.is_some() {
            self.write_recovery_record_from(tmp_path)?;
        }
        self.write_end_block()?;
        Ok(())
    }

    /// Decode and recompress one member of the affected solid chain.
    ///
    /// Kept members are decoded with the shared decoder window and
    /// recompressed with a shared encoder window; deleted members are only
    /// decoded (to advance the window) and their blocks are not written.
    fn recompress_chain_member(
        &mut self,
        reader: &mut File,
        idx: usize,
        is_deleted: bool,
        dec: &mut DecoderState,
        enc: &mut crate::codec::EncoderState,
        enc_active: &mut bool,
    ) -> RarResult<()> {
        if is_deleted {
            let _ = self.decode_chain_member(reader, idx, dec)?;
            return Ok(());
        }
        let entry = self.entries[idx].clone();
        let hdr = &entry.header;
        let data = self.decode_chain_member(reader, idx, dec)?;

        let plain_crc = crc32fast::hash(&data);
        let plain_blake = hdr.hash_value.map(|_| crate::blake2sp::hash(&data));
        let packed = compression::compress_chunked(
            &data,
            hdr.comp_method,
            hdr.comp_dict_size,
            crate::codec::DEFAULT_CHUNK_SIZE,
            Some(enc),
            true,
            None,
        )
        .map_err(RarError::Unsupported)?;

        let (method, dsl, payload) = if packed.len() >= data.len() {
            // Compression is a net loss: STORE resets the chain, matching
            // the sequential add_file path.
            enc.reset();
            *enc_active = false;
            (COMP_METHOD_STORE, 0u8, data.clone())
        } else {
            *enc_active = true;
            (hdr.comp_method, hdr.comp_dict_size, packed)
        };
        let (header_crc, extra_data, stored_hash, encr_params) =
            RarArchive::payload_extra_and_crc(self.password.as_deref(), plain_crc, plain_blake)?;
        let payload = RarArchive::encrypt_payload_with(
            self.password.as_deref(),
            encr_params.as_ref(),
            &payload,
        )?;
        self.write_file_entry(
            &hdr.name,
            data.len() as u64,
            &payload,
            header_crc,
            method,
            dsl,
            &extra_data,
            hdr.attributes,
            hdr.mtime,
            *enc_active,
            stored_hash,
        )?;
        Ok(())
    }

    /// Decode member `idx` with a shared decoder state, verifying its
    /// integrity. Reads directly from `reader` (the original archive) since
    /// `self.stream` is the replacement file during deletion.
    fn decode_chain_member(
        &mut self,
        reader: &mut File,
        idx: usize,
        state: &mut DecoderState,
    ) -> RarResult<Vec<u8>> {
        let hdr = &self.entries[idx].header;
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(Vec::new());
        }
        let payload = self.read_packed_single(reader, idx)?;
        let hdr = &self.entries[idx].header;
        let raw_data = if hdr.comp_method == COMP_METHOD_STORE {
            payload.data
        } else {
            compression::decompress(
                &payload.data,
                hdr.comp_method,
                hdr.unpacked_size,
                hdr.comp_dict_size,
                Some(state),
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

    /// Read the packed (and decrypted, when applicable) payload of a
    /// single-volume member directly from `reader`.
    fn read_packed_single(&mut self, reader: &mut File, idx: usize) -> RarResult<DecryptedPayload> {
        let entry = &self.entries[idx];
        let hdr = &entry.header;
        let chunk = entry
            .chunks
            .first()
            .ok_or_else(|| RarError::Format(format!("{}: no data chunk", hdr.name)))?;
        reader.seek(SeekFrom::Start(chunk.data_offset))?;
        let mut packed = Vec::new();
        packed
            .try_reserve_exact(chunk.packed_size as usize)
            .map_err(|_| RarError::LimitExceeded {
                limit: self.max_packed_bytes(),
                context: format!("{}: cannot allocate packed data", hdr.name),
            })?;
        reader.take(chunk.packed_size).read_to_end(&mut packed)?;

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
                return Err(RarError::Encrypted("wrong password".into()));
            }
            let keys = p.derive_keys(password)?;
            let mut data = encryption::decrypt_data(&packed, &keys.key, &p.iv)?;
            if hdr.comp_method == COMP_METHOD_STORE {
                data.truncate(hdr.unpacked_size as usize);
            }
            packed = data;
            Some(keys)
        } else {
            None
        };

        Ok(DecryptedPayload {
            data: packed,
            params,
            keys,
        })
    }

    /// Rebuild the main archive header for the rewritten archive: original
    /// flags, original extra records with the locator replaced by a fresh
    /// quick-open / recovery locator. Returns the header position, the
    /// plaintext offsets of the preallocated quick-open and recovery offset
    /// fields (patched at the end), and the full header bytes.
    #[allow(clippy::type_complexity)] // the header position + locator fields
    fn write_main_header(
        &mut self,
        meta: &BlockMeta,
        rr_percent: Option<u8>,
    ) -> RarResult<(u64, Option<usize>, Option<usize>, Vec<u8>)> {
        let ah = ArchiveHeader::from_raw(&meta.raw)?;
        if ah.flags & ARCHIVE_FLAG_LOCKED != 0 {
            return Err(RarError::Unsupported("archive is locked".into()));
        }
        let (had_qo, _had_rr, mut extra) = split_main_extra(&ah.extra_data)?;
        self.quick_open = had_qo && !self.header_encryption;
        // The recovery record is rebuilt when the original archive had one
        // (or when the caller forces it, e.g. the `rr` command).
        self.recovery_percent = rr_percent;

        let mut arch_flags = ah.flags & !ARCHIVE_FLAG_RECOVERY;
        if self.recovery_percent.is_some() {
            arch_flags |= ARCHIVE_FLAG_RECOVERY;
        }

        const LOCATOR_TYPE: u64 = 0x01;
        const LOCATOR_FLAG_QUICK_OPEN: u64 = 0x0001;
        const LOCATOR_FLAG_RECOVERY: u64 = 0x0002;
        let mut qo_field_pos = None;
        let mut rr_field_pos = None;
        let mut locator_flags = 0u64;
        if self.quick_open {
            locator_flags |= LOCATOR_FLAG_QUICK_OPEN;
        }
        if self.recovery_percent.is_some() {
            locator_flags |= LOCATOR_FLAG_RECOVERY;
        }
        let mut locator = Vec::new();
        if locator_flags != 0 {
            locator.extend(vint::encode(locator_flags));
            if self.quick_open {
                let p = locator.len();
                locator.extend_from_slice(&vint_fixed5(0));
                qo_field_pos = Some(p);
            }
            if self.recovery_percent.is_some() {
                let p = locator.len();
                locator.extend_from_slice(&vint_fixed5(0));
                rr_field_pos = Some(p);
            }
            let mut record = Vec::new();
            record.extend(vint::encode(locator.len() as u64));
            record.extend(vint::encode(LOCATOR_TYPE));
            record.extend(&locator);
            extra.extend(record);
        }

        let mut block_flags = 0u64;
        if !extra.is_empty() {
            block_flags |= BLOCK_FLAG_EXTRA_DATA;
        }

        let mut body = Vec::new();
        body.extend(vint::encode(BLOCK_TYPE_ARCHIVE_HEADER));
        body.extend(vint::encode(block_flags));
        if block_flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            body.extend(vint::encode(extra.len() as u64));
        }
        body.extend(vint::encode(arch_flags));
        body.extend(&extra);

        let size_bytes = vint::encode(body.len() as u64);
        let mut header_content = Vec::with_capacity(size_bytes.len() + body.len());
        header_content.extend(&size_bytes);
        header_content.extend(&body);
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header_content);
        let crc = hasher.finalize();
        let mut hdr = Vec::with_capacity(4 + header_content.len());
        hdr.extend(crc.to_le_bytes());
        hdr.extend(header_content);

        // Plaintext-relative offset of the locator offset fields inside the
        // locator body (see write_archive_header_with_locators).
        let field_base = 4usize
            + size_bytes.len()
            + vint::encoded_size(BLOCK_TYPE_ARCHIVE_HEADER)
            + vint::encoded_size(block_flags)
            + vint::encoded_size(extra.len() as u64)
            + vint::encoded_size(arch_flags)
            + vint::encoded_size(locator.len() as u64)
            + vint::encoded_size(LOCATOR_TYPE);
        let qo_field_pos = qo_field_pos.map(|p| field_base + p);
        let rr_field_pos = rr_field_pos.map(|p| field_base + p);

        let main_start = self.stream.as_ref().unwrap().stream_position()?;
        self.write_block_header(&hdr)?;
        Ok((main_start, qo_field_pos, rr_field_pos, hdr))
    }

    /// Patch the rewritten main header with the real quick-open and/or
    /// recovery-record offsets and rewrite it in place (the offset fields
    /// were preallocated as fixed 5-byte vints, so the header length never
    /// changes).
    fn patch_main_header(
        &mut self,
        qo_offset: Option<u64>,
        rr_offset: Option<u64>,
        main_start: u64,
        qo_field_pos: Option<usize>,
        rr_field_pos: Option<usize>,
        main_hdr: &[u8],
    ) -> RarResult<()> {
        let mut hdr = main_hdr.to_vec();
        let mut patched = false;
        if let (Some(qo), Some(field)) = (qo_offset, qo_field_pos) {
            let base = self.sfx_offset + RAR5_SIGNATURE.len() as u64;
            let field_bytes = vint_fixed5(qo.saturating_sub(base));
            if field + field_bytes.len() > hdr.len() {
                return Err(RarError::Format("locator field out of bounds".into()));
            }
            hdr[field..field + field_bytes.len()].copy_from_slice(&field_bytes);
            patched = true;
        }
        if let (Some(rr), Some(field)) = (rr_offset, rr_field_pos) {
            let base = self.sfx_offset + RAR5_SIGNATURE.len() as u64;
            let field_bytes = vint_fixed5(rr.saturating_sub(base));
            if field + field_bytes.len() > hdr.len() {
                return Err(RarError::Format("locator field out of bounds".into()));
            }
            hdr[field..field + field_bytes.len()].copy_from_slice(&field_bytes);
            patched = true;
        }
        if patched {
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&hdr[4..]);
            let crc = hasher.finalize();
            hdr[..4].copy_from_slice(&crc.to_le_bytes());
        }
        self.stream
            .as_mut()
            .unwrap()
            .seek(SeekFrom::Start(main_start))?;
        self.write_block_header(&hdr)?;
        self.stream.as_mut().unwrap().seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// Recovery percentage carried by a dropped "RR" service block
    /// (service data record type 0x07, single byte).
    fn rr_percent_from_block(&self, meta: &BlockMeta) -> Option<u8> {
        let data = &meta.raw.header_data;
        let mut offset = 0usize;
        let (_, n) = vint::decode_from_slice(data, offset).ok()?;
        offset += n;
        let (flags, n) = vint::decode_from_slice(data, offset).ok()?;
        offset += n;
        let mut extra_size = 0usize;
        if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            let (v, n) = vint::decode_from_slice(data, offset).ok()?;
            extra_size = v as usize;
            offset += n;
        }
        if flags & BLOCK_FLAG_DATA_AREA != 0 {
            let (_, n) = vint::decode_from_slice(data, offset).ok()?;
            offset += n;
        }
        // file flags, unpacked size, attributes, compression info, host OS
        for _ in 0..5 {
            let (_, n) = vint::decode_from_slice(data, offset).ok()?;
            offset += n;
        }
        let (name_len, n) = vint::decode_from_slice(data, offset).ok()?;
        offset += n + name_len as usize;
        if offset + extra_size > data.len() {
            return None;
        }
        let extra = &data[offset..offset + extra_size];
        let mut e = 0usize;
        let (rec_size, n) = vint::decode_from_slice(extra, e).ok()?;
        e += n;
        let rec_start = e;
        let (rec_type, n) = vint::decode_from_slice(extra, e).ok()?;
        let _ = n;
        if rec_type != 0x07 || rec_size == 0 {
            return None;
        }
        let data_end = rec_start + rec_size as usize;
        if data_end > extra.len() {
            return None;
        }
        Some(extra[data_end - 1])
    }

    /// Name of a service block (type 3), if parseable.
    fn service_block_name(&self, meta: &BlockMeta) -> RarResult<Option<String>> {
        let data = &meta.raw.header_data;
        let mut offset = 0usize;
        let (_, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("service block type: {e}")))?;
        offset += n;
        let (flags, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("service block flags: {e}")))?;
        offset += n;
        if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("service block extra size: {e}")))?;
            offset += n;
        }
        if flags & BLOCK_FLAG_DATA_AREA != 0 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("service block data size: {e}")))?;
            offset += n;
        }
        // file flags, unpacked size, attributes, then fixed time/CRC32
        // fields, then compression info and host OS.
        let (file_flags, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("service block file flags: {e}")))?;
        offset += n;
        for _ in 0..2 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("service block field: {e}")))?;
            offset += n;
        }
        if file_flags & FILE_FLAG_TIME_UNIX != 0 {
            offset += 4;
        }
        if file_flags & FILE_FLAG_CRC32 != 0 {
            offset += 4;
        }
        for _ in 0..2 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("service block field: {e}")))?;
            offset += n;
        }
        let (name_len, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("service block name: {e}")))?;
        offset += n;
        let end = (offset + name_len as usize).min(data.len());
        Ok(Some(
            String::from_utf8_lossy(&data[offset..end]).into_owned(),
        ))
    }

    /// Read the next block from the original archive, decrypting the header
    /// when archive header encryption is active, and capture the exact
    /// on-disk header bytes (needed for verbatim copies and the quick-open
    /// cache). Returns `None` at EOF. The reader must be positioned at the
    /// block start and is left at the data area start.
    fn read_next_block(&mut self, reader: &mut File) -> RarResult<Option<BlockMeta>> {
        if !self.header_encryption {
            let block_start = reader.stream_position()?;
            let mut header_bytes = Vec::with_capacity(32);
            let mut crc_buf = [0u8; 4];
            match reader.read_exact(&mut crc_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(e.into()),
            }
            header_bytes.extend_from_slice(&crc_buf);
            let mut vint_bytes = Vec::with_capacity(2);
            let hsize = loop {
                let mut b = [0u8; 1];
                reader.read_exact(&mut b)?;
                vint_bytes.push(b[0]);
                if b[0] & 0x80 == 0 {
                    break vint::decode_from_slice(&vint_bytes, 0)
                        .map_err(|e| RarError::Format(format!("bad vint: {e}")))?
                        .0;
                }
            };
            if hsize == 0 || hsize > 2 * 1024 * 1024 {
                return Err(RarError::Format(format!(
                    "implausible header size: {hsize}"
                )));
            }
            let hsize_vint_len = vint_bytes.len();
            header_bytes.extend_from_slice(&vint_bytes);
            let mut body = vec![0u8; hsize as usize];
            reader.read_exact(&mut body)?;
            header_bytes.extend_from_slice(&body);

            // Validate the header CRC over the size vint + body.
            let stored_crc = u32::from_le_bytes(crc_buf);
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&vint_bytes);
            hasher.update(&body);
            let computed = hasher.finalize();
            if computed != stored_crc {
                return Err(RarError::Crc {
                    expected: stored_crc,
                    actual: computed,
                    context: "block header".into(),
                });
            }

            let (block_type, flags, data_size) = parse_raw_block_fields(&body)?;
            let data_offset = reader.stream_position()?;
            let data_end = data_offset + data_size;
            let raw = RawBlock {
                header_crc: stored_crc,
                header_data: body,
                data_size,
                data_offset,
                block_type,
                flags,
            };
            return Ok(Some(BlockMeta {
                block_type,
                flags,
                block_start,
                data_offset,
                data_end,
                header_bytes,
                hsize_vint_len,
                raw,
            }));
        }
        self.read_next_encrypted_block(reader)
    }

    /// Read the next `[IV][AES-256-CBC encrypted header][data area]` block.
    fn read_next_encrypted_block(&mut self, reader: &mut File) -> RarResult<Option<BlockMeta>> {
        let block_start = reader.stream_position()?;
        let mut iv = [0u8; ENCR_IV_SIZE];
        match reader.read_exact(&mut iv) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let encr = self
            .archive_encr
            .as_ref()
            .ok_or_else(|| RarError::Format("archive encryption parameters missing".into()))?;
        let password = self.password.as_ref().ok_or_else(|| {
            RarError::Encrypted("archive has encrypted headers; provide a password".into())
        })?;
        let key = encr.get_key(password);

        let mut first_ct = [0u8; 16];
        reader.read_exact(&mut first_ct)?;
        let first_pt = encryption::decrypt_data(&first_ct, &key, &iv)?;
        let (_crc, vint_len, hsize) = {
            let stored_crc = u32::from_le_bytes(first_pt[..4].try_into().unwrap());
            let (hsize, vint_len) = vint::decode_from_slice(&first_pt, 4)
                .map_err(|e| RarError::Format(format!("encrypted block vint: {e}")))?;
            (stored_crc, vint_len, hsize)
        };
        if hsize == 0 || hsize > 2 * 1024 * 1024 {
            return Err(RarError::Format(format!(
                "implausible encrypted header size: {hsize}"
            )));
        }
        let total_raw = 4 + vint_len + hsize as usize;
        let enc_size = total_raw.div_ceil(16) * 16;
        let mut full_ct = vec![0u8; enc_size];
        full_ct[..16].copy_from_slice(&first_ct);
        if enc_size > 16 {
            reader.read_exact(&mut full_ct[16..])?;
        }
        let full_pt = encryption::decrypt_data(&full_ct, &key, &iv)?;

        let stored_crc = u32::from_le_bytes(full_pt[..4].try_into().unwrap());
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&full_pt[4..total_raw]);
        let computed = hasher.finalize();
        if computed != stored_crc {
            return Err(RarError::Crc {
                expected: stored_crc,
                actual: computed,
                context: "encrypted block header".into(),
            });
        }

        let mut offset = 4 + vint_len;
        let (block_type, n) = vint::decode_from_slice(&full_pt, offset)
            .map_err(|e| RarError::Format(format!("block type: {e}")))?;
        offset += n;
        let (flags, n) = vint::decode_from_slice(&full_pt, offset)
            .map_err(|e| RarError::Format(format!("block flags: {e}")))?;
        offset += n;
        let mut extra_size = 0u64;
        if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            let (v, n) = vint::decode_from_slice(&full_pt, offset)
                .map_err(|e| RarError::Format(format!("extra size: {e}")))?;
            extra_size = v;
            offset += n;
        }
        let mut data_size = 0u64;
        if flags & BLOCK_FLAG_DATA_AREA != 0 {
            let (v, n) = vint::decode_from_slice(&full_pt, offset)
                .map_err(|e| RarError::Format(format!("data size: {e}")))?;
            data_size = v;
            offset += n;
        }
        let _ = (extra_size, offset);

        let data_offset = block_start + 16 + enc_size as u64;
        let data_end = data_offset + data_size;
        let mut header_bytes = Vec::with_capacity(16 + enc_size);
        header_bytes.extend_from_slice(&iv);
        header_bytes.extend_from_slice(&full_ct);
        let raw = RawBlock {
            header_crc: stored_crc,
            header_data: full_pt[4 + vint_len..total_raw].to_vec(),
            data_size,
            data_offset,
            block_type,
            flags,
        };
        Ok(Some(BlockMeta {
            block_type,
            flags,
            block_start,
            data_offset,
            data_end,
            header_bytes,
            hsize_vint_len: vint_len,
            raw,
        }))
    }

    // ── Public API: creation ───────────────────────────────────────────────

    /// Add a file from the filesystem to the archive.
    pub fn add(&mut self, path: impl AsRef<Path>, compression_level: u8) -> RarResult<()> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(RarError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("path not found: {}", path.display()),
            )));
        }

        if path.is_dir() {
            self.add_directory(path, None, true, compression_level)
        } else {
            self.add_file(path, None, compression_level)
        }
    }

    /// Add a file or directory to the archive under a custom archive name.
    ///
    /// `arcname` overrides the entry name in the archive. For directories the
    /// children keep the same relative layout beneath `arcname`.
    pub fn add_as(
        &mut self,
        path: impl AsRef<Path>,
        arcname: &str,
        compression_level: u8,
    ) -> RarResult<()> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(RarError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("path not found: {}", path.display()),
            )));
        }

        let arcname = arcname.replace('\\', "/");
        let arcname = arcname.trim_start_matches('/').to_string();

        if path.is_dir() {
            self.add_directory(path, Some(&arcname), true, compression_level)
        } else {
            self.add_file(path, Some(&arcname), compression_level)
        }
    }

    fn add_file(&mut self, path: &Path, arcname: Option<&str>, level: u8) -> RarResult<()> {
        let meta = fs::metadata(path)?;
        if !meta.is_file() {
            return Err(RarError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a file: {}", path.display()),
            )));
        }
        let file_size = meta.len();
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;
        let mtime_ns = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let time_extra = (mtime_ns != 0).then(|| file_time_extra_record(mtime as u64, mtime_ns));

        #[cfg(unix)]
        let attrs = {
            use std::os::unix::fs::MetadataExt;
            meta.mode() as u64
        };
        #[cfg(not(unix))]
        let attrs = 0o100644u64;

        let name = arcname
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().into_owned());
        let name = name.replace('\\', "/");

        if let Some(cb) = self.progress_callback.as_deref_mut() {
            cb(0, file_size);
        }

        let method = level_to_method(level);
        let probe_incompressible = method != COMP_METHOD_STORE
            && file_size >= (SAMPLE_PROBE_HEAD as u64) * 4
            && sample_is_incompressible_file(path, file_size, method)?;

        if method == COMP_METHOD_STORE || probe_incompressible {
            // STORE is written by streaming the file directly: bounded
            // memory regardless of file size. Encrypted STORE stays
            // buffered (CBC padding over the whole member).
            self.reset_solid_chain();
            let (plain_crc, plain_blake) = hash_file(path, file_size, self.blake2)?;
            let (header_crc, mut extra_data, stored_hash, encr_params) =
                RarArchive::payload_extra_and_crc(
                    self.password.as_deref(),
                    plain_crc,
                    plain_blake,
                )?;
            if let Some(ref t) = time_extra {
                extra_data.extend_from_slice(t);
            }
            if self.password.is_some() {
                let raw_data = fs::read(path)?;
                let packed_data = RarArchive::encrypt_payload_with(
                    self.password.as_deref(),
                    encr_params.as_ref(),
                    &raw_data,
                )?;
                self.write_file_entry(
                    &name,
                    file_size,
                    &packed_data,
                    header_crc,
                    COMP_METHOD_STORE,
                    0,
                    &extra_data,
                    attrs,
                    mtime,
                    false,
                    stored_hash,
                )?;
            } else {
                self.write_stored_file(
                    &name,
                    file_size,
                    header_crc,
                    attrs,
                    mtime,
                    File::open(path)?,
                    &extra_data,
                    stored_hash,
                )?;
            }
            if let Some(cb) = self.progress_callback.as_deref_mut() {
                cb(file_size, file_size);
            }
            return Ok(());
        }

        // Compressed path: read and compress in bounded chunks with a
        // persistent encoder state (solid archives share the LZ window).
        let dsl = dict_size_for_data(file_size as usize, method);
        let chain_solid = self.solid_mode && self.encoder_state.is_some();
        if self.solid_mode {
            self.encoder_state.get_or_insert_with(Default::default);
        }

        let mut crc_hasher = crc32fast::Hasher::new();
        let mut blake_hasher = if self.blake2 {
            Some(crate::blake2sp::Hasher::new())
        } else {
            None
        };
        let mut packed = Vec::new();
        let mut bytes_read = 0u64;
        {
            let mut file = io::BufReader::with_capacity(1 << 20, File::open(path)?);
            let mut buf = vec![0u8; crate::codec::DEFAULT_CHUNK_SIZE];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                bytes_read += n as u64;
                crc_hasher.update(&buf[..n]);
                if let Some(h) = blake_hasher.as_mut() {
                    h.update(&buf[..n]);
                }
                let state = self.encoder_state.as_mut();
                let compressed = compression::compress_chunked(
                    &buf[..n],
                    method,
                    dsl,
                    crate::codec::DEFAULT_CHUNK_SIZE,
                    state,
                    n < buf.len(),
                    None,
                )
                .map_err(RarError::Unsupported)?;
                packed.extend(compressed);
                if let Some(cb) = self.progress_callback.as_deref_mut() {
                    cb(bytes_read, file_size);
                }
                if packed.len() as u64 >= file_size {
                    break;
                }
            }
        }

        let plain_crc = crc_hasher.finalize();
        let plain_blake = blake_hasher.map(|h| h.finalize());

        if packed.len() as u64 >= file_size {
            // Compression is a net loss: fall back to streaming STORE.
            self.reset_solid_chain();
            let (header_crc, mut extra_data, stored_hash, encr_params) =
                RarArchive::payload_extra_and_crc(
                    self.password.as_deref(),
                    plain_crc,
                    plain_blake,
                )?;
            if let Some(ref t) = time_extra {
                extra_data.extend_from_slice(t);
            }
            if self.password.is_some() {
                let raw_data = fs::read(path)?;
                let packed_data = RarArchive::encrypt_payload_with(
                    self.password.as_deref(),
                    encr_params.as_ref(),
                    &raw_data,
                )?;
                self.write_file_entry(
                    &name,
                    file_size,
                    &packed_data,
                    header_crc,
                    COMP_METHOD_STORE,
                    0,
                    &extra_data,
                    attrs,
                    mtime,
                    false,
                    stored_hash,
                )?;
            } else {
                self.write_stored_file(
                    &name,
                    file_size,
                    header_crc,
                    attrs,
                    mtime,
                    File::open(path)?,
                    &extra_data,
                    stored_hash,
                )?;
            }
            if let Some(cb) = self.progress_callback.as_deref_mut() {
                cb(file_size, file_size);
            }
            return Ok(());
        }

        let (header_crc, mut extra_data, stored_hash, encr_params) =
            RarArchive::payload_extra_and_crc(self.password.as_deref(), plain_crc, plain_blake)?;
        if let Some(ref t) = time_extra {
            extra_data.extend_from_slice(t);
        }
        let packed_data = RarArchive::encrypt_payload_with(
            self.password.as_deref(),
            encr_params.as_ref(),
            &packed,
        )?;
        self.write_file_entry(
            &name,
            file_size,
            &packed_data,
            header_crc,
            method,
            dsl,
            &extra_data,
            attrs,
            mtime,
            chain_solid,
            stored_hash,
        )?;

        if let Some(cb) = self.progress_callback.as_deref_mut() {
            cb(file_size, file_size);
        }

        Ok(())
    }

    /// Add a file redirection entry (symlink, hardlink or file copy) to
    /// the archive (like `rar a -ol` / `-oh`).
    ///
    /// The entry carries no data; `redir_type` is 1 (Unix symlink),
    /// 2 (Windows symlink), 3 (Windows junction), 4 (hardlink) or
    /// 5 (file copy) and `target` is the referenced member name.
    pub fn add_redirect(&mut self, name: &str, redir_type: u64, target: &str) -> RarResult<()> {
        if self.mode != Mode::Write && self.mode != Mode::Append {
            return Err(RarError::Format(
                "add_redirect requires an archive being written".into(),
            ));
        }
        self.reset_solid_chain();
        let fh = FileHeader {
            name: name.replace('\\', "/"),
            unpacked_size: 0,
            packed_size: 0,
            crc32_val: Some(0),
            file_flags: FILE_FLAG_CRC32,
            extra_data: redirect_extra_bytes(redir_type, target),
            ..Default::default()
        };
        let hdr_bytes = fh.to_bytes();
        self.write_block_header(&hdr_bytes)?;
        self.entries.push(ArchiveEntry {
            header: fh,
            chunks: Vec::new(),
        });
        Ok(())
    }

    /// Add a directory entry only (no recursion).
    ///
    /// Writes the directory header without traversing children. Callers that
    /// enumerate files themselves (e.g. with exclusion filtering) use this to
    /// keep empty directories and the directory structure in the archive.
    pub fn add_directory_only(&mut self, path: impl AsRef<Path>, arcname: &str) -> RarResult<()> {
        let path = path.as_ref();
        self.reset_solid_chain();
        let name = arcname.replace('\\', "/").trim_end_matches('/').to_string() + "/";

        let meta = fs::metadata(path)?;
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        #[cfg(unix)]
        let attrs = {
            use std::os::unix::fs::MetadataExt;
            meta.mode() as u64
        };
        #[cfg(not(unix))]
        let attrs = 0o040755u64;

        let fh = FileHeader {
            name: name.clone(),
            attributes: attrs,
            mtime,
            host_os: OS_UNIX,
            file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_DIRECTORY,
            is_directory: true,
            ..Default::default()
        };

        let hdr_bytes = fh.to_bytes();
        self.write_block_header(&hdr_bytes)?;
        self.entries.push(ArchiveEntry {
            header: fh,
            chunks: Vec::new(),
        });

        Ok(())
    }

    fn add_directory(
        &mut self,
        path: &Path,
        arcname: Option<&str>,
        recursive: bool,
        level: u8,
    ) -> RarResult<()> {
        self.reset_solid_chain();
        let name = arcname
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().into_owned());
        let name = name.replace('\\', "/").trim_end_matches('/').to_string() + "/";

        let meta = fs::metadata(path)?;
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        #[cfg(unix)]
        let attrs = {
            use std::os::unix::fs::MetadataExt;
            meta.mode() as u64
        };
        #[cfg(not(unix))]
        let attrs = 0o040755u64;

        let fh = FileHeader {
            name: name.clone(),
            attributes: attrs,
            mtime,
            host_os: OS_UNIX,
            file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_DIRECTORY,
            is_directory: true,
            ..Default::default()
        };

        let hdr_bytes = fh.to_bytes();
        self.write_block_header(&hdr_bytes)?;
        self.entries.push(ArchiveEntry {
            header: fh,
            chunks: Vec::new(),
        });

        if recursive {
            let mut children: Vec<_> = fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
            children.sort_by_key(|e| e.file_name());

            for child in children {
                let child_path = child.path();
                let child_name = format!("{}{}", name, child.file_name().to_string_lossy());
                if child_path.is_dir() {
                    self.add_directory(&child_path, Some(&child_name), true, level)?;
                } else {
                    self.add_file(&child_path, Some(&child_name), level)?;
                }
            }
        }

        Ok(())
    }

    /// Add raw bytes as a named file in the archive.
    pub fn add_bytes(
        &mut self,
        arcname: &str,
        data: &[u8],
        compression_level: u8,
    ) -> RarResult<()> {
        let name = arcname.replace('\\', "/");
        let plain_crc = {
            let mut h = crc32fast::Hasher::new();
            h.update(data);
            h.finalize()
        };
        let plain_blake = if self.blake2 {
            Some(crate::blake2sp::hash(data))
        } else {
            None
        };
        let mtime = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        let method = level_to_method(compression_level);
        if let Some(cb) = self.progress_callback.as_deref_mut() {
            cb(0, data.len() as u64);
        }
        if method == COMP_METHOD_STORE || sample_is_incompressible(data, method) {
            self.reset_solid_chain();
            let (header_crc, extra_data, stored_hash, encr_params) =
                RarArchive::payload_extra_and_crc(
                    self.password.as_deref(),
                    plain_crc,
                    plain_blake,
                )?;
            let packed_data = RarArchive::encrypt_payload_with(
                self.password.as_deref(),
                encr_params.as_ref(),
                data,
            )?;
            self.write_file_entry(
                &name,
                data.len() as u64,
                &packed_data,
                header_crc,
                COMP_METHOD_STORE,
                0,
                &extra_data,
                0o100644,
                mtime,
                false,
                stored_hash,
            )?;
        } else {
            let dsl = dict_size_for_data(data.len(), method);
            let chain_solid = self.solid_mode && self.encoder_state.is_some();
            if self.solid_mode {
                self.encoder_state.get_or_insert_with(Default::default);
            }
            let mut progress: Option<&mut dyn FnMut(u64, u64)> = None;
            if let Some(cb) = self.progress_callback.as_deref_mut() {
                let cb: &mut dyn FnMut(u64, u64) = cb;
                progress = Some(cb);
            }
            let packed = compression::compress_chunked(
                data,
                method,
                dsl,
                crate::codec::DEFAULT_CHUNK_SIZE,
                self.encoder_state.as_mut(),
                true,
                progress,
            )
            .map_err(RarError::Unsupported)?;
            if packed.len() >= data.len() {
                self.reset_solid_chain();
                let (header_crc, extra_data, stored_hash, encr_params) =
                    RarArchive::payload_extra_and_crc(
                        self.password.as_deref(),
                        plain_crc,
                        plain_blake,
                    )?;
                let packed_data = RarArchive::encrypt_payload_with(
                    self.password.as_deref(),
                    encr_params.as_ref(),
                    data,
                )?;
                self.write_file_entry(
                    &name,
                    data.len() as u64,
                    &packed_data,
                    header_crc,
                    COMP_METHOD_STORE,
                    0,
                    &extra_data,
                    0o100644,
                    mtime,
                    false,
                    stored_hash,
                )?;
            } else {
                let (header_crc, extra_data, stored_hash, encr_params) =
                    RarArchive::payload_extra_and_crc(
                        self.password.as_deref(),
                        plain_crc,
                        plain_blake,
                    )?;
                let packed_data = RarArchive::encrypt_payload_with(
                    self.password.as_deref(),
                    encr_params.as_ref(),
                    &packed,
                )?;
                self.write_file_entry(
                    &name,
                    data.len() as u64,
                    &packed_data,
                    header_crc,
                    method,
                    dsl,
                    &extra_data,
                    0o100644,
                    mtime,
                    chain_solid,
                    stored_hash,
                )?;
            }
        }

        if let Some(cb) = self.progress_callback.as_deref_mut() {
            cb(data.len() as u64, data.len() as u64);
        }

        Ok(())
    }

    // ── Batch addition ─────────────────────────────────────────────────────

    /// Add multiple entries at once.
    ///
    /// With the `parallel` feature enabled this compresses eligible
    /// members (non-solid archives, file/bytes entries up to 64 MiB) in
    /// Rayon waves and writes them in the original order. For unencrypted
    /// archives the resulting archive is byte-identical to calling the
    /// individual `add*` methods sequentially; files over
    /// [`PARALLEL_COMPRESS_MAX_MEMBER`] are compressed in parallel chunks
    /// (non-solid only), while directories and solid archives fall back to
    /// the sequential path. Without the feature this is a plain sequential
    /// loop over the same `add*` calls.
    pub fn add_batch(&mut self, entries: &[BatchEntry<'_>]) -> RarResult<()> {
        #[cfg(feature = "parallel")]
        {
            if !self.solid_mode && !entries.is_empty() {
                return self.add_batch_parallel(entries);
            }
        }
        for entry in entries {
            self.add_batch_entry_sequential(entry)?;
        }
        Ok(())
    }

    fn add_batch_entry_sequential(&mut self, entry: &BatchEntry<'_>) -> RarResult<()> {
        match *entry {
            BatchEntry::Bytes { name, data, level } => self.add_bytes(name, data, level),
            BatchEntry::File { path, name, level } => match name {
                Some(name) => self.add_as(path, name, level),
                None => self.add(path, level),
            },
            BatchEntry::Directory { path, name } => {
                let name = match name {
                    Some(name) => name.to_string(),
                    None => path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                };
                self.add_directory_only(path, &name)
            }
        }
    }

    #[cfg(feature = "parallel")]
    fn add_batch_parallel(&mut self, entries: &[BatchEntry<'_>]) -> RarResult<()> {
        let mut i = 0usize;
        while i < entries.len() {
            // Collect a consecutive run of eligible members into one wave
            // (bounded total input). Directories and oversized files break
            // the wave and are handled sequentially at their original
            // position, preserving archive order.
            let mut wave: Vec<(usize, BatchEntry<'_>)> = Vec::new();
            let mut wave_bytes = 0u64;
            while i < entries.len() {
                let size = match entries[i] {
                    BatchEntry::Bytes { data, .. } => Some(data.len() as u64),
                    BatchEntry::File { path, .. } => {
                        let size = fs::metadata(path)?.len();
                        (size <= PARALLEL_COMPRESS_MAX_MEMBER).then_some(size)
                    }
                    BatchEntry::Directory { .. } => None,
                };
                let Some(size) = size else { break };
                if wave_bytes + size > PARALLEL_COMPRESS_WAVE_BUDGET && !wave.is_empty() {
                    break;
                }
                wave_bytes += size;
                wave.push((i, entries[i]));
                i += 1;
            }

            if !wave.is_empty() {
                let prepared = self.prepare_batch_wave(&wave)?;
                for (_, entry) in prepared {
                    let size = entry.unpacked_size;
                    if let Some(cb) = self.progress_callback.as_deref_mut() {
                        cb(0, size);
                    }
                    self.write_prepared_entry(entry)?;
                    if let Some(cb) = self.progress_callback.as_deref_mut() {
                        cb(size, size);
                    }
                }
            }

            if i < entries.len() {
                if let BatchEntry::File { path, name, level } = entries[i] {
                    let size = fs::metadata(path)?.len();
                    if size > PARALLEL_COMPRESS_MAX_MEMBER {
                        if let Some(prepared) =
                            self.prepare_large_file_parallel(path, name, level)?
                        {
                            let member_size = prepared.unpacked_size;
                            if let Some(cb) = self.progress_callback.as_deref_mut() {
                                cb(0, member_size);
                            }
                            self.write_prepared_entry(prepared)?;
                            if let Some(cb) = self.progress_callback.as_deref_mut() {
                                cb(member_size, member_size);
                            }
                        } else {
                            // STORE / probe-incompressible fallback: the
                            // sequential path streams the member directly.
                            self.add_batch_entry_sequential(&entries[i])?;
                        }
                        i += 1;
                        continue;
                    }
                }
                self.add_batch_entry_sequential(&entries[i])?;
                i += 1;
            }
        }
        Ok(())
    }

    #[cfg(feature = "parallel")]
    fn prepare_batch_wave(
        &self,
        wave: &[(usize, BatchEntry<'_>)],
    ) -> RarResult<Vec<(usize, PreparedEntry)>> {
        use rayon::prelude::*;

        let ctx = BatchPrepareCtx {
            password: self.password.as_deref(),
            blake2: self.blake2,
        };
        let results: Vec<RarResult<(usize, PreparedEntry)>> = compression_pool().install(|| {
            wave.par_iter()
                .map(|&(idx, entry)| {
                    let _guard = BatchWorkerGuard::new();
                    let prepared = match entry {
                        BatchEntry::Bytes { name, data, level } => {
                            let mtime = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as u32;
                            Self::prepare_data_entry(
                                &ctx, name, data, level, 0o100644, mtime, None, false,
                            )
                        }
                        BatchEntry::File { path, name, level } => {
                            Self::prepare_file_entry(&ctx, path, name, level)
                        }
                        BatchEntry::Directory { .. } => {
                            unreachable!("directories never enter a compression wave")
                        }
                    };
                    prepared.map(|p| (idx, p))
                })
                .collect()
        });

        let mut out = Vec::with_capacity(results.len());
        for result in results {
            out.push(result?);
        }
        out.sort_by_key(|(idx, _)| *idx);
        Ok(out)
    }

    /// Hash, filter/compress (or STORE) and encrypt one in-memory member
    /// without touching the archive stream. `file_origin` selects the exact
    /// sequential encoding used by [`Self::add_file`] (fresh encoder window
    /// per chunk) instead of [`Self::add_bytes`] (one shared window pass).
    #[cfg(feature = "parallel")]
    fn prepare_data_entry(
        ctx: &BatchPrepareCtx<'_>,
        name: &str,
        data: &[u8],
        level: u8,
        attrs: u64,
        mtime: u32,
        time_extra: Option<Vec<u8>>,
        file_origin: bool,
    ) -> RarResult<PreparedEntry> {
        let plain_crc = crc32fast::hash(data);
        let plain_blake = if ctx.blake2 {
            Some(crate::blake2sp::hash(data))
        } else {
            None
        };
        let method = level_to_method(level);

        if method == COMP_METHOD_STORE || sample_is_incompressible(data, method) {
            return Self::prepared_from_payload(
                ctx,
                name,
                data.len(),
                attrs,
                mtime,
                time_extra,
                plain_crc,
                plain_blake,
                COMP_METHOD_STORE,
                0,
                data.to_vec(),
            );
        }

        let dsl = dict_size_for_data(data.len(), method);
        let packed = if file_origin {
            // Mirror add_file's streaming loop exactly: each chunk is
            // compressed with a fresh encoder window (non-solid archives),
            // so the batch archive stays byte-identical to the sequential
            // path.
            let mut packed = Vec::new();
            for chunk in data.chunks(crate::codec::DEFAULT_CHUNK_SIZE) {
                let is_final = chunk.len() < crate::codec::DEFAULT_CHUNK_SIZE;
                let compressed = compression::compress_chunked(
                    chunk,
                    method,
                    dsl,
                    crate::codec::DEFAULT_CHUNK_SIZE,
                    None,
                    is_final,
                    None,
                )
                .map_err(RarError::Unsupported)?;
                packed.extend(compressed);
                if packed.len() >= data.len() {
                    break;
                }
            }
            packed
        } else {
            compression::compress_chunked(
                data,
                method,
                dsl,
                crate::codec::DEFAULT_CHUNK_SIZE,
                None,
                true,
                None,
            )
            .map_err(RarError::Unsupported)?
        };

        if packed.len() >= data.len() {
            return Self::prepared_from_payload(
                ctx,
                name,
                data.len(),
                attrs,
                mtime,
                time_extra,
                plain_crc,
                plain_blake,
                COMP_METHOD_STORE,
                0,
                data.to_vec(),
            );
        }
        Self::prepared_from_payload(
            ctx,
            name,
            data.len(),
            attrs,
            mtime,
            time_extra,
            plain_crc,
            plain_blake,
            method,
            dsl,
            packed,
        )
    }

    #[cfg(feature = "parallel")]
    fn prepare_file_entry(
        ctx: &BatchPrepareCtx<'_>,
        path: &Path,
        arcname: Option<&str>,
        level: u8,
    ) -> RarResult<PreparedEntry> {
        let meta = fs::metadata(path)?;
        if !meta.is_file() {
            return Err(RarError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a file: {}", path.display()),
            )));
        }
        let file_size = meta.len();
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;
        let mtime_ns = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let time_extra = (mtime_ns != 0).then(|| file_time_extra_record(mtime as u64, mtime_ns));

        #[cfg(unix)]
        let attrs = {
            use std::os::unix::fs::MetadataExt;
            meta.mode() as u64
        };
        #[cfg(not(unix))]
        let attrs = 0o100644u64;

        let name = arcname
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().into_owned());
        let name = name.replace('\\', "/");
        let name = name.trim_start_matches('/').to_string();

        let data = fs::read(path)?;
        if data.len() as u64 != file_size {
            return Err(RarError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "file changed size while being archived: expected {file_size} bytes, read {}",
                    data.len()
                ),
            )));
        }
        Self::prepare_data_entry(ctx, &name, &data, level, attrs, mtime, time_extra, true)
    }

    /// Prepare a large file (over [`PARALLEL_COMPRESS_MAX_MEMBER`]) by
    /// compressing its 4 MiB chunks in parallel and concatenating them in
    /// file order.
    ///
    /// Non-solid archives encode each chunk with a fresh encoder window, so
    /// the packed stream is byte-identical to the sequential `add_file`
    /// path. Raw input is never buffered whole: each Rayon worker reads and
    /// compresses one chunk at a time, bounding memory to roughly
    /// `threads × chunk size + packed output`.
    ///
    /// Returns `None` when the member should go through the sequential
    /// path instead (STORE / sample-probe incompressible, or compression
    /// would not shrink the payload).
    #[cfg(feature = "parallel")]
    fn prepare_large_file_parallel(
        &self,
        path: &Path,
        arcname: Option<&str>,
        level: u8,
    ) -> RarResult<Option<PreparedEntry>> {
        use rayon::prelude::*;

        let meta = fs::metadata(path)?;
        if !meta.is_file() {
            return Err(RarError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a file: {}", path.display()),
            )));
        }
        let file_size = meta.len();
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        #[cfg(unix)]
        let attrs = {
            use std::os::unix::fs::MetadataExt;
            meta.mode() as u64
        };
        #[cfg(not(unix))]
        let attrs = 0o100644u64;

        let name = arcname
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().into_owned());
        let name = name.replace('\\', "/");
        let name = name.trim_start_matches('/').to_string();

        let method = level_to_method(level);
        let probe_incompressible = method != COMP_METHOD_STORE
            && file_size >= (SAMPLE_PROBE_HEAD as u64) * 4
            && sample_is_incompressible_file(path, file_size, method)?;
        if method == COMP_METHOD_STORE || probe_incompressible {
            return Ok(None);
        }

        let dsl = dict_size_for_data(file_size as usize, method);
        let (plain_crc, plain_blake) = hash_file(path, file_size, self.blake2)?;
        let mtime_ns = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let time_extra = (mtime_ns != 0).then(|| file_time_extra_record(mtime as u64, mtime_ns));

        let chunk_size = crate::codec::DEFAULT_CHUNK_SIZE as u64;
        let chunk_count = file_size.div_ceil(chunk_size) as usize;
        let results: Vec<RarResult<(usize, Vec<u8>)>> = large_file_pool().install(|| {
            (0..chunk_count)
                .into_par_iter()
                .map(|idx| {
                    let _guard = BatchWorkerGuard::new();
                    let start = idx as u64 * chunk_size;
                    let len = file_size.saturating_sub(start).min(chunk_size) as usize;
                    let mut buf = vec![0u8; len];
                    {
                        use std::io::{Read, Seek, SeekFrom};
                        let mut f = fs::File::open(path)?;
                        f.seek(SeekFrom::Start(start))?;
                        f.read_exact(&mut buf)?;
                    }
                    let is_final = len < chunk_size as usize;
                    let packed = compression::compress_chunked(
                        &buf,
                        method,
                        dsl,
                        crate::codec::DEFAULT_CHUNK_SIZE,
                        None,
                        is_final,
                        None,
                    )
                    .map_err(RarError::Unsupported)?;
                    Ok((idx, packed))
                })
                .collect()
        });

        let mut packed = Vec::new();
        for result in results {
            let (_, chunk_packed) = result?;
            packed.extend(chunk_packed);
            if packed.len() as u64 >= file_size {
                break;
            }
        }
        if packed.len() as u64 >= file_size {
            // Compression is a net loss; the sequential path streams STORE.
            return Ok(None);
        }

        let (header_crc, mut extra_data, stored_hash, encr_params) =
            RarArchive::payload_extra_and_crc(self.password.as_deref(), plain_crc, plain_blake)?;
        if let Some(t) = time_extra {
            extra_data.extend_from_slice(&t);
        }
        let payload = RarArchive::encrypt_payload_with(
            self.password.as_deref(),
            encr_params.as_ref(),
            &packed,
        )?;
        Ok(Some(PreparedEntry {
            name,
            unpacked_size: file_size,
            attrs,
            mtime,
            file_crc: header_crc,
            method,
            dict_size_log: dsl,
            extra_data,
            stored_hash,
            payload,
        }))
    }

    /// Turn a plaintext payload (raw data or compressed stream) into a
    /// [`PreparedEntry`], deriving the header checksum/extra records and
    /// applying encryption exactly like the sequential `add*` paths.
    #[cfg(feature = "parallel")]
    #[allow(clippy::too_many_arguments)] // mirrors the existing write_file_entry signature
    fn prepared_from_payload(
        ctx: &BatchPrepareCtx<'_>,
        name: &str,
        data_len: usize,
        attrs: u64,
        mtime: u32,
        time_extra: Option<Vec<u8>>,
        plain_crc: u32,
        plain_blake: Option<[u8; 32]>,
        method: u8,
        dict_size_log: u8,
        payload: Vec<u8>,
    ) -> RarResult<PreparedEntry> {
        let (header_crc, mut extra_data, stored_hash, encr) =
            RarArchive::payload_extra_and_crc(ctx.password, plain_crc, plain_blake)?;
        if let Some(t) = time_extra {
            extra_data.extend_from_slice(&t);
        }
        let payload = RarArchive::encrypt_payload_with(ctx.password, encr.as_ref(), &payload)?;
        Ok(PreparedEntry {
            name: name.to_string(),
            unpacked_size: data_len as u64,
            attrs,
            mtime,
            file_crc: header_crc,
            method,
            dict_size_log,
            extra_data,
            stored_hash,
            payload,
        })
    }

    #[cfg(feature = "parallel")]
    fn write_prepared_entry(&mut self, entry: PreparedEntry) -> RarResult<()> {
        self.write_file_entry(
            &entry.name,
            entry.unpacked_size,
            &entry.payload,
            entry.file_crc,
            entry.method,
            entry.dict_size_log,
            &entry.extra_data,
            entry.attrs,
            entry.mtime,
            false,
            entry.stored_hash,
        )
    }

    /// Write a file entry, splitting across volumes if needed.
    #[allow(clippy::too_many_arguments)]
    fn write_file_entry(
        &mut self,
        name: &str,
        unpacked_size: u64,
        packed_data: &[u8],
        file_crc: u32,
        method: u8,
        dict_size_log: u8,
        extra_data: &[u8],
        attrs: u64,
        mtime: u32,
        solid: bool,
        hash_value: Option<[u8; 32]>,
    ) -> RarResult<()> {
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
            host_os: OS_UNIX,
            file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
            extra_data: extra_data.to_vec(),
            ..Default::default()
        };

        if self.volume_size.is_none() {
            // Single-volume
            let hdr_bytes = fh_base.to_bytes();
            if self.quick_open {
                let pos = self.stream.as_ref().unwrap().stream_position()?;
                self.quick_open_entries.push((pos, hdr_bytes.clone()));
            }
            self.write_block_header(&hdr_bytes)?;
            let stream = self.stream.as_mut().unwrap();
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
        let volume_size = self.volume_size.unwrap();
        let eoa_size: u64 = 7; // approximate end-of-archive block size
        let total_packed = packed_data.len() as u64;

        // Check if it fits in current volume
        let hdr_bytes = fh_base.to_bytes();
        let total_needed = hdr_bytes.len() as u64 + total_packed + eoa_size;
        let remaining = volume_size.saturating_sub(self.volume_bytes_written);

        if total_needed <= remaining {
            // Fits entirely
            let stream = self.stream.as_mut().unwrap();
            stream.write_all(&hdr_bytes)?;
            stream.write_all(packed_data)?;
            self.volume_bytes_written += hdr_bytes.len() as u64 + total_packed;
            let data_offset = stream.stream_position()? - total_packed;
            let chunk = DataChunk {
                volume_index: self.current_volume - 1,
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

        // Need to split across volumes
        let mut offset = 0u64;
        let mut chunks = Vec::new();
        let mut is_first = true;

        while offset < total_packed {
            let remaining_vol = volume_size.saturating_sub(self.volume_bytes_written);

            // Build chunk flags
            let mut block_flags: u64 = 0;
            if !is_first {
                block_flags |= BLOCK_FLAG_DATA_CONTINUES;
            }

            // Estimate header size
            let chunk_fh = FileHeader {
                name: name.to_string(),
                unpacked_size,
                packed_size: remaining_vol.max(1),
                attributes: attrs,
                mtime,
                crc32_val: Some(0),
                comp_method: method,
                comp_solid: solid,
                comp_dict_size: dict_size_log,
                host_os: OS_UNIX,
                flags: block_flags | BLOCK_FLAG_DATA_CONTINUE_TO,
                file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
                extra_data: if is_first {
                    extra_data.to_vec()
                } else {
                    Vec::new()
                },
                ..Default::default()
            };
            let hdr_size = chunk_fh.to_bytes().len() as u64;

            let bytes_for_data = remaining_vol.saturating_sub(hdr_size + eoa_size);
            if bytes_for_data == 0 {
                self.start_next_volume()?;
                is_first = false;
                continue;
            }

            let chunk_size = bytes_for_data.min(total_packed - offset);
            let is_last = offset + chunk_size >= total_packed;
            let chunk_packed = &packed_data[offset as usize..(offset + chunk_size) as usize];

            // Set final flags
            if is_last {
                block_flags &= !BLOCK_FLAG_DATA_CONTINUE_TO;
            } else {
                block_flags |= BLOCK_FLAG_DATA_CONTINUE_TO;
            }

            let chunk_crc = if is_last {
                file_crc
            } else {
                let mut h = crc32fast::Hasher::new();
                h.update(chunk_packed);
                h.finalize()
            };

            let final_fh = FileHeader {
                name: name.to_string(),
                unpacked_size,
                packed_size: chunk_size,
                attributes: attrs,
                mtime,
                crc32_val: Some(chunk_crc),
                comp_method: method,
                comp_solid: solid,
                comp_dict_size: dict_size_log,
                host_os: OS_UNIX,
                flags: block_flags,
                file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
                extra_data: if is_first {
                    extra_data.to_vec()
                } else {
                    Vec::new()
                },
                ..Default::default()
            };

            let final_hdr = final_fh.to_bytes();
            let stream = self.stream.as_mut().unwrap();
            stream.write_all(&final_hdr)?;
            stream.write_all(chunk_packed)?;
            self.volume_bytes_written += final_hdr.len() as u64 + chunk_size;

            let data_offset = stream.stream_position()? - chunk_size;
            chunks.push(DataChunk {
                volume_index: self.current_volume - 1,
                data_offset,
                packed_size: chunk_size,
                crc32_val: Some(chunk_crc),
                is_final: is_last,
                extra_data: if is_first {
                    extra_data.to_vec()
                } else {
                    Vec::new()
                },
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

    /// Drop the solid-chain encoder state (call after any member that does
    /// not participate in the LZ window: directories, STORE files, empty
    /// files, or when compression fell back to STORE).
    fn reset_solid_chain(&mut self) {
        self.encoder_state = None;
    }

    /// Build the header CRC, extra-area records (encryption + BLAKE2sp)
    /// and stored hash value for a member, plus the encryption parameters
    /// to reuse for the actual payload encryption (one KDF/salt per
    /// member). For encrypted members the checksums are MAC'd with the
    /// hash key, matching WinRAR.
    #[allow(clippy::type_complexity)]
    fn payload_extra_and_crc(
        password: Option<&str>,
        plain_crc: u32,
        plain_blake: Option<[u8; 32]>,
    ) -> RarResult<(
        u32,
        Vec<u8>,
        Option<[u8; 32]>,
        Option<encryption::EncryptionParams>,
    )> {
        if let Some(password) = password {
            let params =
                encryption::EncryptionParams::generate_for_password(password, ENCR_PBKDF2_ITER_LOG);
            let header_crc = params.mac_crc32(plain_crc, password)?;
            let stored_hash = match plain_blake {
                Some(h) => Some(params.mac_hash32(h, password)?),
                None => None,
            };
            let mut extra = params.to_extra_bytes();
            if let Some(h) = stored_hash {
                extra.extend(crate::headers::hash_extra_record(h));
            }
            Ok((header_crc, extra, stored_hash, Some(params)))
        } else {
            let mut extra = Vec::new();
            if let Some(h) = plain_blake {
                extra.extend(crate::headers::hash_extra_record(h));
            }
            Ok((plain_crc, extra, plain_blake, None))
        }
    }

    /// Encrypt a member payload with the parameters returned by
    /// [`Self::payload_extra_and_crc`] (must match the member's stored
    /// salt).
    fn encrypt_payload_with(
        password: Option<&str>,
        params: Option<&encryption::EncryptionParams>,
        plaintext: &[u8],
    ) -> RarResult<Vec<u8>> {
        match (password, params) {
            (Some(password), Some(params)) => Ok(params.encrypt(plaintext, password)),
            (None, None) => Ok(plaintext.to_vec()),
            _ => Err(RarError::Format(
                "internal error: encryption parameters mismatch".into(),
            )),
        }
    }

    /// Stream a STORE member directly from a reader (bounded memory).
    ///
    /// Handles single-volume and multi-volume splitting. The plaintext CRC
    /// must be supplied (it is part of the header, written before data).
    #[allow(clippy::too_many_arguments)]
    fn write_stored_file(
        &mut self,
        name: &str,
        unpacked_size: u64,
        file_crc: u32,
        attrs: u64,
        mtime: u32,
        mut reader: impl Read + Seek,
        extra_data: &[u8],
        hash_value: Option<[u8; 32]>,
    ) -> RarResult<()> {
        let fh_base = FileHeader {
            name: name.to_string(),
            unpacked_size,
            packed_size: unpacked_size,
            attributes: attrs,
            mtime,
            crc32_val: Some(file_crc),
            hash_type: if hash_value.is_some() { 0 } else { u8::MAX },
            hash_value,
            comp_method: COMP_METHOD_STORE,
            host_os: OS_UNIX,
            file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
            extra_data: extra_data.to_vec(),
            ..Default::default()
        };

        if self.volume_size.is_none() {
            let hdr_bytes = fh_base.to_bytes();
            if self.quick_open {
                let pos = self.stream.as_ref().unwrap().stream_position()?;
                self.quick_open_entries.push((pos, hdr_bytes.clone()));
            }
            self.write_block_header(&hdr_bytes)?;
            let stream = self.stream.as_mut().unwrap();
            let mut buf = vec![0u8; 1 << 20];
            let mut written = 0u64;
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                stream.write_all(&buf[..n])?;
                written += n as u64;
                if let Some(cb) = self.progress_callback.as_deref_mut() {
                    cb(written, unpacked_size);
                }
            }
            if written != unpacked_size {
                return Err(RarError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "file changed size while being archived: expected {unpacked_size} bytes, read {written}"
                    ),
                )));
            }
            let data_offset = stream.stream_position()? - unpacked_size;
            self.entries.push(ArchiveEntry {
                header: FileHeader {
                    data_offset,
                    ..fh_base
                },
                chunks: vec![DataChunk {
                    volume_index: 0,
                    data_offset,
                    packed_size: unpacked_size,
                    crc32_val: Some(file_crc),
                    is_final: true,
                    extra_data: extra_data.to_vec(),
                }],
            });
            return Ok(());
        }

        // Multi-volume streaming split.
        let volume_size = self.volume_size.unwrap();
        let eoa_size: u64 = 7; // approximate end-of-archive block size
        let mut offset = 0u64;
        let mut chunks = Vec::new();
        let mut is_first = true;
        while offset < unpacked_size {
            let remaining_vol = volume_size.saturating_sub(self.volume_bytes_written);
            let mut block_flags: u64 = 0;
            if !is_first {
                block_flags |= BLOCK_FLAG_DATA_CONTINUES;
            }
            let chunk_fh = FileHeader {
                name: name.to_string(),
                unpacked_size,
                packed_size: remaining_vol.max(1),
                attributes: attrs,
                mtime,
                crc32_val: Some(0),
                comp_method: COMP_METHOD_STORE,
                host_os: OS_UNIX,
                flags: block_flags | BLOCK_FLAG_DATA_CONTINUE_TO,
                file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
                extra_data: if is_first {
                    extra_data.to_vec()
                } else {
                    Vec::new()
                },
                hash_type: if is_first && hash_value.is_some() {
                    0
                } else {
                    u8::MAX
                },
                hash_value: if is_first { hash_value } else { None },
                ..Default::default()
            };
            let hdr_size = chunk_fh.to_bytes().len() as u64;
            let bytes_for_data = remaining_vol.saturating_sub(hdr_size + eoa_size);
            if bytes_for_data == 0 {
                self.start_next_volume()?;
                is_first = false;
                continue;
            }
            let chunk_size = bytes_for_data.min(unpacked_size - offset);
            let is_last = offset + chunk_size >= unpacked_size;
            if is_last {
                block_flags &= !BLOCK_FLAG_DATA_CONTINUE_TO;
            } else {
                block_flags |= BLOCK_FLAG_DATA_CONTINUE_TO;
            }

            // For non-final chunks the header carries the CRC of this
            // chunk's on-disk bytes. Compute it in a first pass and seek
            // back before the copy pass (bounded memory).
            let chunk_crc = if is_last {
                file_crc
            } else {
                let chunk_start = reader.stream_position()?;
                let mut h = crc32fast::Hasher::new();
                let mut remaining = chunk_size;
                let mut probe = vec![0u8; 1 << 20];
                while remaining > 0 {
                    let want = probe.len().min(remaining as usize);
                    let n = reader.read(&mut probe[..want])?;
                    if n == 0 {
                        return Err(RarError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "file changed size while being archived (CRC pass)",
                        )));
                    }
                    h.update(&probe[..n]);
                    remaining -= n as u64;
                }
                reader.seek(SeekFrom::Start(chunk_start))?;
                h.finalize()
            };

            let final_fh = FileHeader {
                name: name.to_string(),
                unpacked_size,
                packed_size: chunk_size,
                attributes: attrs,
                mtime,
                crc32_val: Some(chunk_crc),
                comp_method: COMP_METHOD_STORE,
                host_os: OS_UNIX,
                flags: block_flags,
                file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
                extra_data: if is_first {
                    extra_data.to_vec()
                } else {
                    Vec::new()
                },
                hash_type: if is_first && hash_value.is_some() {
                    0
                } else {
                    u8::MAX
                },
                hash_value: if is_first { hash_value } else { None },
                ..Default::default()
            };
            let final_hdr = final_fh.to_bytes();
            let stream = self.stream.as_mut().unwrap();
            stream.write_all(&final_hdr)?;

            // Copy exactly `chunk_size` bytes.
            let mut remaining = chunk_size;
            let mut buf = vec![0u8; 1 << 20];
            while remaining > 0 {
                let want = buf.len().min(remaining as usize);
                let n = reader.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(RarError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "file changed size while being archived: still missing {remaining} bytes"
                        ),
                    )));
                }
                stream.write_all(&buf[..n])?;
                remaining -= n as u64;
                if let Some(cb) = self.progress_callback.as_deref_mut() {
                    cb(unpacked_size - remaining, unpacked_size);
                }
            }

            self.volume_bytes_written += final_hdr.len() as u64 + chunk_size;
            let data_offset = stream.stream_position()? - chunk_size;
            chunks.push(DataChunk {
                volume_index: self.current_volume - 1,
                data_offset,
                packed_size: chunk_size,
                crc32_val: Some(chunk_crc),
                is_final: is_last,
                extra_data: if is_first {
                    extra_data.to_vec()
                } else {
                    Vec::new()
                },
            });
            offset += chunk_size;
            is_first = false;
            if !is_last {
                self.start_next_volume()?;
            }
        }

        self.entries.push(ArchiveEntry {
            header: FileHeader {
                packed_size: unpacked_size,
                ..fh_base
            },
            chunks,
        });
        Ok(())
    }
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

/// Parse the type, flags and data size out of a plaintext block header
/// body (the fields after the header size vint).
fn parse_raw_block_fields(body: &[u8]) -> RarResult<(u64, u64, u64)> {
    let mut offset = 0usize;
    let (block_type, n) = vint::decode_from_slice(body, offset)
        .map_err(|e| RarError::Format(format!("block type: {e}")))?;
    offset += n;
    let (flags, n) = vint::decode_from_slice(body, offset)
        .map_err(|e| RarError::Format(format!("block flags: {e}")))?;
    offset += n;
    if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
        let (_, n) = vint::decode_from_slice(body, offset)
            .map_err(|e| RarError::Format(format!("extra size: {e}")))?;
        offset += n;
    }
    let mut data_size = 0u64;
    if flags & BLOCK_FLAG_DATA_AREA != 0 {
        let (v, n) = vint::decode_from_slice(body, offset)
            .map_err(|e| RarError::Format(format!("data size: {e}")))?;
        data_size = v;
        offset += n;
    }
    let _ = offset;
    Ok((block_type, flags, data_size))
}

/// Locate the quick-open and recovery offset fields inside an existing
/// main archive header (plaintext-relative offsets, used to patch the
/// locator in place when appending).
fn main_header_locator_fields(meta: &BlockMeta) -> RarResult<(Option<usize>, Option<usize>)> {
    const LOCATOR_TYPE: u64 = 0x01;
    const LOCATOR_FLAG_QUICK_OPEN: u64 = 0x0001;
    const LOCATOR_FLAG_RECOVERY: u64 = 0x0002;
    let data = &meta.raw.header_data;
    let mut offset = 0usize;
    let (_, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::Format(format!("block type: {e}")))?;
    offset += n;
    let (flags, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::Format(format!("block flags: {e}")))?;
    offset += n;
    let mut extra_size = 0usize;
    if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
        let (v, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("extra size: {e}")))?;
        extra_size = v as usize;
        offset += n;
    }
    if flags & BLOCK_FLAG_DATA_AREA != 0 {
        let (_, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("data size: {e}")))?;
        offset += n;
    }
    let (_, n) = vint::decode_from_slice(data, offset)
        .map_err(|e| RarError::Format(format!("archive flags: {e}")))?;
    offset += n;
    let extra = &data[offset..offset + extra_size];
    // Header layout: [crc 4][size vint][body ...][extra area].
    let extra_base = 4 + meta.hsize_vint_len + offset;

    let mut e = 0usize;
    while e < extra.len() {
        let (rec_size, n) = vint::decode_from_slice(extra, e)
            .map_err(|e| RarError::Format(format!("extra record: {e}")))?;
        e += n;
        let rec_start = e;
        let (rec_type, n) = vint::decode_from_slice(extra, e)
            .map_err(|e| RarError::Format(format!("extra record type: {e}")))?;
        e += n;
        if rec_type == LOCATOR_TYPE {
            let (loc_flags, n) = vint::decode_from_slice(extra, e)
                .map_err(|e| RarError::Format(format!("locator flags: {e}")))?;
            e += n;
            let mut qo = None;
            if loc_flags & LOCATOR_FLAG_QUICK_OPEN != 0 {
                qo = Some(extra_base + e);
                let (_, qn) = vint::decode_from_slice(extra, e)
                    .map_err(|e| RarError::Format(format!("quick-open offset: {e}")))?;
                e += qn;
            }
            let mut rr = None;
            if loc_flags & LOCATOR_FLAG_RECOVERY != 0 {
                rr = Some(extra_base + e);
            }
            return Ok((qo, rr));
        }
        e = rec_start + rec_size as usize;
    }
    Ok((None, None))
}

/// RAR5 file redirection (EXTRA_FILE_REDIRECT) record: symlink, hardlink
/// or file copy target reference.
struct RedirectSpec {
    redir_type: u64,
    target: String,
}

/// Serialize a file redirection (EXTRA_FILE_REDIRECT) extra record.
fn redirect_extra_bytes(redir_type: u64, target: &str) -> Vec<u8> {
    let mut record = Vec::new();
    record.extend(vint::encode(redir_type));
    record.extend(vint::encode(0u64)); // flags
    record.extend(vint::encode(target.len() as u64));
    record.extend_from_slice(target.as_bytes());
    let mut out = Vec::new();
    out.extend(vint::encode((1 + record.len()) as u64));
    out.extend(vint::encode(0x05u64)); // EXTRA_FILE_REDIRECT
    out.extend(record);
    out
}

/// Parse the file redirection record out of an entry's extra area.
fn parse_redirect_record(extra: &[u8]) -> Option<RedirectSpec> {
    let mut offset = 0usize;
    while offset < extra.len() {
        let (rec_size, n) = vint::decode_from_slice(extra, offset).ok()?;
        offset += n;
        // The record size counts the type byte and everything after it.
        let rec_end = offset.checked_add(rec_size as usize)?;
        if rec_end > extra.len() {
            return None;
        }
        let (rec_type, tn) = vint::decode_from_slice(extra, offset).ok()?;
        let mut p = offset + tn;
        if rec_type == EXTRA_FILE_REDIRECT {
            let (redir_type, rn) = vint::decode_from_slice(extra, p).ok()?;
            p += rn;
            let (flags, fn_len) = vint::decode_from_slice(extra, p).ok()?;
            p += fn_len;
            let (name_len, nn) = vint::decode_from_slice(extra, p).ok()?;
            p += nn;
            let name_start = p;
            let name_end = name_start.checked_add(name_len as usize)?;
            if name_end != rec_end {
                return None;
            }
            let _ = flags;
            return Some(RedirectSpec {
                redir_type,
                target: String::from_utf8_lossy(&extra[name_start..name_end]).into_owned(),
            });
        }
        offset = rec_end;
    }
    None
}

/// Serialize a nanosecond modification time extra record
/// (`EXTRA_FILE_TIME`), matching the official `rar` format:
/// `[flags 0x13][seconds u32][nanoseconds u32]`.
fn file_time_extra_record(secs: u64, ns: u32) -> Vec<u8> {
    let mut record = Vec::with_capacity(10);
    record.push(0x13); // flags: modification time with nanoseconds
    record.extend_from_slice(&(secs as u32).to_le_bytes());
    record.extend_from_slice(&ns.to_le_bytes());
    let mut out = Vec::with_capacity(12);
    out.extend(vint::encode((1 + record.len()) as u64));
    out.extend(vint::encode(EXTRA_FILE_TIME));
    out.extend(record);
    out
}

/// Serialize a "CMT" archive comment service block (type 3, name "CMT",
/// comment bytes in the data area), matching the official `rar c` format.
fn build_comment_block(comment: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(vint::encode(BLOCK_TYPE_SERVICE_HEADER));
    body.extend(vint::encode(BLOCK_FLAG_DATA_AREA));
    body.extend(vint::encode(comment.len() as u64));
    body.extend(vint::encode(FILE_FLAG_CRC32));
    body.extend(vint::encode(comment.len() as u64)); // unpacked size
    body.extend(vint::encode(0u64)); // attributes
    body.extend(crc32fast::hash(comment).to_le_bytes());
    body.extend(vint::encode(0u64)); // compression info (store)
    body.extend(vint::encode(OS_UNIX));
    body.extend(vint::encode(3u64)); // name length
    body.extend(b"CMT");

    let size_bytes = vint::encode(body.len() as u64);
    let mut header_content = Vec::with_capacity(size_bytes.len() + body.len());
    header_content.extend(&size_bytes);
    header_content.extend(&body);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&header_content);
    let crc = hasher.finalize();

    let mut block = Vec::with_capacity(4 + header_content.len() + comment.len());
    block.extend(crc.to_le_bytes());
    block.extend(header_content);
    block.extend_from_slice(comment);
    block
}

/// Split a main archive header's extra area into the locator record
/// contents (`had_qo`, `had_rr`) and the remaining records verbatim.
fn split_main_extra(extra: &[u8]) -> RarResult<(bool, bool, Vec<u8>)> {
    const LOCATOR_TYPE: u64 = 0x01;
    const LOCATOR_FLAG_QUICK_OPEN: u64 = 0x0001;
    const LOCATOR_FLAG_RECOVERY: u64 = 0x0002;
    let mut had_qo = false;
    let mut had_rr = false;
    let mut rest = Vec::new();
    let mut off = 0usize;
    while off < extra.len() {
        let (rec_size, n) = vint::decode_from_slice(extra, off)
            .map_err(|e| RarError::Format(format!("main header extra record: {e}")))?;
        let rec_start = off + n;
        let (rec_type, tn) = vint::decode_from_slice(extra, rec_start)
            .map_err(|e| RarError::Format(format!("main header extra record type: {e}")))?;
        if rec_type == LOCATOR_TYPE {
            // The locator record size convention differs between writers
            // (WinRAR counts the type byte, rar-rs does not), so the record
            // boundary is derived from the parsed fields instead.
            let mut p = rec_start + tn;
            let (loc_flags, ln) = vint::decode_from_slice(extra, p)
                .map_err(|e| RarError::Format(format!("locator flags: {e}")))?;
            p += ln;
            if loc_flags & LOCATOR_FLAG_QUICK_OPEN != 0 {
                had_qo = true;
                let (_, qn) = vint::decode_from_slice(extra, p)
                    .map_err(|e| RarError::Format(format!("quick-open offset: {e}")))?;
                p += qn;
            }
            if loc_flags & LOCATOR_FLAG_RECOVERY != 0 {
                had_rr = true;
                let (_, rn) = vint::decode_from_slice(extra, p)
                    .map_err(|e| RarError::Format(format!("recovery offset: {e}")))?;
                p += rn;
            }
            off = p;
        } else {
            let rec_end = rec_start.checked_add(rec_size as usize).ok_or_else(|| {
                RarError::Format("main header extra record size overflows".into())
            })?;
            if rec_end > extra.len() || rec_end <= rec_start {
                return Err(RarError::Format("malformed main header extra area".into()));
            }
            rest.extend_from_slice(&extra[off..rec_end]);
            off = rec_end;
        }
    }
    Ok((had_qo, had_rr, rest))
}

fn dict_size_for_data(data_size: usize, level: u8) -> u8 {
    // Window caps per compression level (WinRAR-like, kept conservative for
    // speed): faster levels stay at 1 MiB; higher levels grow the window so
    // large files find longer-range matches. The decoder accepts any
    // dictionary up to MAX_DICT_SIZE_LOG (1 GiB).
    let cap = match level {
        1..=2 => 1024 * 1024,
        3 => 2 * 1024 * 1024,
        4 => 4 * 1024 * 1024,
        _ => 8 * 1024 * 1024, // 5
    };
    let base = 128 * 1024;
    let target = data_size.min(cap);
    let mut log = 0u8;
    while (base << log) < target && log < 6 {
        log += 1;
    }
    log
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

/// In-memory stride probe (used by `add_bytes`).
fn sample_is_incompressible(data: &[u8], method: u8) -> bool {
    if data.len() < 4 * SAMPLE_PROBE_HEAD {
        return false;
    }
    let mut bad = 0;
    if incompressible_sample(&data[..SAMPLE_PROBE_HEAD], method) {
        bad += 1;
    }
    for &pos in &[data.len() / 4, data.len() / 2, data.len() * 3 / 4] {
        if pos >= SAMPLE_PROBE_HEAD
            && pos + SAMPLE_PROBE_TAIL <= data.len()
            && incompressible_sample(&data[pos..pos + SAMPLE_PROBE_TAIL], method)
        {
            bad += 1;
        }
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
    }
    Ok(bad >= 2)
}

fn incompressible_sample(sample: &[u8], method: u8) -> bool {
    if sample.is_empty() {
        return false;
    }
    let packed = compression::compress(sample, method, 0).unwrap_or_default();
    packed.len() >= sample.len() * 9 / 10
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

/// Build a unique temporary sibling path for atomic extraction.
fn temp_sibling_path(dest_path: &Path) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let file_name = dest_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "entry".to_string());
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_name = format!(".{file_name}.rar5tmp-{}-{counter}", std::process::id());
    dest_path.with_file_name(tmp_name)
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

/// Encode `value` as a fixed 5-byte RAR5 vint (LSB-first, continuation bit
/// on every byte except the last). Valid for values < 2^35.
fn vint_fixed5(value: u64) -> [u8; 5] {
    let mut out = [0x80u8; 5];
    let mut v = value;
    for (i, byte) in out.iter_mut().enumerate() {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if i < 4 {
            b |= 0x80;
        }
        *byte = b;
    }
    out
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
        let msg = err.unwrap().to_string();
        assert!(msg.contains("password"), "unexpected error: {msg}");
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
