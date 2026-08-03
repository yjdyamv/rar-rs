/// RarArchive — high-level RAR4/RAR5 archive interface.
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
use crate::rar4;
use crate::vint;

/// Maximum archive prefix buffered for inline recovery-record parity.
/// Streamed recovery records are not implemented yet; larger archives must
/// create recovery records without `recovery_percent`.
const MAX_RECOVERY_PREFIX_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// Maximum accepted RAR5 dictionary-size log (1 GiB, the WinRAR 5.x
/// maximum). Larger values are rejected at decode time to bound window
/// allocations.
const MAX_DICT_SIZE_LOG: u8 = 13;

/// A single entry in the archive (public API).
#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    pub header: FileHeader,
    pub chunks: Vec<DataChunk>,
}

/// Decrypted member payload plus the key material needed for integrity
/// verification.
struct DecryptedPayload {
    data: Vec<u8>,
    params: Option<encryption::EncryptionParams>,
    keys: Option<encryption::DerivedKeys>,
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

/// RAR4/RAR5 archive reader/writer.
pub struct RarArchive {
    path: PathBuf,
    mode: Mode,
    entries: Vec<ArchiveEntry>,
    stream: Option<File>,
    /// Archive format version (4 or 5).
    format_version: u8,
    /// Persistent decoder state for RAR5 solid archive chains.
    solid_state: Option<DecoderState>,
    /// Persistent decoder state for RAR4 solid archive chains.
    rar4_solid_state: Option<rar4::decoder::Rar4DecoderState>,
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
            stream: None,
            format_version: 5,
            solid_state: None,
            rar4_solid_state: None,
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
            stream: None,
            format_version: 5,
            solid_state: None,
            rar4_solid_state: None,
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
        if opts.encrypt_headers
            && opts.password.as_deref().is_none_or(|pw| pw.is_empty())
        {
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
            stream: None,
            format_version: 5,
            solid_state: None,
            rar4_solid_state: None,
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
            rand::Rng::fill(&mut rand::rng(), &mut iv);
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
        let padded_max = if max_len % 2 == 0 { max_len } else { max_len + 1 };

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
            let mut reader = std::fs::File::open(&self.path)?;
            reader.read_exact(&mut prefix)?;
        }

        let rr_data =
            crate::recovery::rar5::build_structural_inline_recovery_data(&prefix, percent)
                .map_err(|e| RarError::Format(format!("recovery record encode: {e}")))?;

        // RR service header: type 3, name "RR", SubData = percent byte.
        let mut body = Vec::new();
        body.extend(vint::encode(0x03u64)); // service header
        body.extend(vint::encode(
            (BLOCK_FLAG_EXTRA_DATA | BLOCK_FLAG_DATA_AREA | BLOCK_FLAG_SKIP_IF_UNKNOWN) as u64,
        ));
        let subdata = {
            let mut rec = Vec::new();
            rec.push(percent as u8); // recovery percent (single byte, <= 100)
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
        body.extend(vint::encode(OS_UNIX as u64));
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
            (BLOCK_FLAG_EXTRA_DATA | BLOCK_FLAG_DATA_AREA | BLOCK_FLAG_SKIP_IF_UNKNOWN) as u64,
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
        body.extend(vint::encode(OS_UNIX as u64));
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
            let enc_size = ((total_raw + 15) / 16) * 16;
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
            let field = self
                .qo_offset_field_pos
                .ok_or_else(|| RarError::Format("quick-open locator field position unknown".into()))?
                as usize;
            let patched = vint_fixed5(qo.saturating_sub(RAR5_SIGNATURE.len() as u64));
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
            let patched = vint_fixed5(rr.saturating_sub(RAR5_SIGNATURE.len() as u64));
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
            rand::Rng::fill(&mut rand::rng(), &mut iv);
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
    /// currently being added. A final `(total, total)` is reported once the
    /// file entry has been written, so callers can drive percent-done UIs.
    pub fn set_progress_callback(&mut self, callback: Option<Box<dyn FnMut(u64, u64) + Send>>) {
        self.progress_callback = callback;
    }

    // ── Signature ──────────────────────────────────────────────────────────

    fn verify_signature(&mut self) -> RarResult<()> {
        let stream = self.stream.as_mut().unwrap();
        let mut sig = [0u8; 8];
        let n = stream.read(&mut sig)?;
        if n < 7 {
            return Err(RarError::Format(format!(
                "file too short to be a RAR archive ({n} bytes read)"
            )));
        }
        if sig == *RAR5_SIGNATURE {
            self.format_version = 5;
            return Ok(());
        }
        if sig[..7] == *RAR4_SIGNATURE {
            self.format_version = 4;
            // RAR4 signature is 7 bytes; seek back 1 byte since we read 8
            if n == 8 {
                stream.seek(SeekFrom::Current(-1))?;
            }
            return Ok(());
        }
        Err(RarError::Format(format!(
            "not a RAR archive (bad signature: {sig:?})"
        )))
    }

    fn write_signature(&mut self) -> RarResult<()> {
        let stream = self.stream.as_mut().unwrap();
        stream.write_all(RAR5_SIGNATURE)?;
        Ok(())
    }

    // ── Block scanning ─────────────────────────────────────────────────────

    fn scan_blocks(&mut self) -> RarResult<()> {
        self.entries.clear();

        if self.format_version == 4 {
            return self.scan_rar4_blocks();
        }

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
            let enc_size = ((total_raw + 15) / 16) * 16;

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

    /// Scan a RAR4 archive's blocks, building entries.
    fn scan_rar4_blocks(&mut self) -> RarResult<()> {
        use rar4::constants::*;
        use rar4::headers::*;

        let stream = self.stream.as_mut().unwrap();

        loop {
            let header_start = stream.stream_position()?;
            let common = match Rar4CommonHeader::read_from(stream) {
                Ok(c) => c,
                Err(RarError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            };

            match common.header_type {
                RAR4_HEAD_MARK => {
                    // Signature marker — already verified, skip to end of header
                    let end = header_start + common.header_size as u64;
                    stream.seek(SeekFrom::Start(end))?;
                }
                RAR4_HEAD_MAIN => {
                    let main_hdr = Rar4MainHeader::parse(&common, stream, header_start)?;
                    if main_hdr.is_encrypted {
                        return Err(RarError::Unsupported(
                            "RAR4 encrypted archives not yet supported".into(),
                        ));
                    }
                }
                RAR4_HEAD_FILE | RAR4_HEAD_NEWSUB => {
                    // Seek back to right after the 7-byte common header
                    stream.seek(SeekFrom::Start(header_start + 7))?;
                    let (fh, chunk) = parse_rar4_file_header(&common, stream, header_start)?;

                    // Skip past packed data
                    let data_end = fh.data_offset + fh.packed_size;
                    stream.seek(SeekFrom::Start(data_end))?;

                    if common.header_type == RAR4_HEAD_NEWSUB {
                        // Sub-blocks (service data) — skip
                        continue;
                    }

                    self.entries.push(ArchiveEntry {
                        header: fh,
                        chunks: vec![chunk],
                    });
                }
                RAR4_HEAD_ENDARC => break,
                _ => {
                    // Skip unknown or unneeded headers (COMM, AV, SUB, PROTECT, SIGN)
                    let end = header_start + common.header_size as u64 + common.add_size as u64;
                    stream.seek(SeekFrom::Start(end))?;
                }
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
            vint::encode(BLOCK_FLAG_EXTRA_DATA as u64),
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
            + vint::encoded_size(BLOCK_FLAG_EXTRA_DATA as u64) as u64
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
        let mut total_unpacked = 0u64;
        let entries: Vec<_> = self.entries.clone();
        for entry in &entries {
            total_unpacked = total_unpacked
                .checked_add(entry.header.unpacked_size)
                .ok_or_else(|| RarError::LimitExceeded {
                    limit: opts.max_total_unpacked_bytes.unwrap_or(u64::MAX),
                    context: "total unpacked size overflow".into(),
                })?;
            if let Some(limit) = opts.max_total_unpacked_bytes {
                if total_unpacked > limit {
                    return Err(RarError::LimitExceeded {
                        limit,
                        context: format!(
                            "total unpacked size {total_unpacked} exceeds limit while extracting {}",
                            entry.name()
                        ),
                    });
                }
            }
            self.extract_entry(entry, dest)?;
        }
        Ok(())
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
        if let Some(limit) = self.extract_options.max_unpacked_bytes {
            if hdr.unpacked_size > limit {
                return Err(RarError::LimitExceeded {
                    limit,
                    context: format!(
                        "{}: unpacked size {} exceeds limit",
                        hdr.name, hdr.unpacked_size
                    ),
                });
            }
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

        // Restore mtime (best-effort)
        if entry.header.mtime != 0 {
            let mtime = UNIX_EPOCH + std::time::Duration::from_secs(entry.header.mtime as u64);
            let times = std::fs::FileTimes::new().set_modified(mtime);
            let _ = std::fs::File::options()
                .write(true)
                .open(&dest_path)
                .and_then(|f| f.set_times(times));
        }

        Ok(dest_path)
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
        if self.extract_options.safe_paths {
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)?;
                let canon_dest = dest_dir.canonicalize()?;
                let canon_parent = parent.canonicalize()?;
                if !canon_parent.starts_with(&canon_dest) {
                    return Err(RarError::Security(format!(
                        "entry {name:?} resolves outside the destination directory"
                    )));
                }
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
            self.rar4_solid_state = None;
            self.solid_decoded_through = -1;
        } else {
            // Starting fresh
            self.solid_state = None;
            self.rar4_solid_state = None;
            self.solid_decoded_through = -1;
        }

        let is_rar4 = self.entries[chain_start].header.format_version == 4;

        // Determine dict_size from the first compressed entry in the chain
        if is_rar4 {
            if self.rar4_solid_state.is_none() {
                self.rar4_solid_state = Some(rar4::decoder::Rar4DecoderState::new(
                    rar4::constants::RAR4_DEFAULT_DICT_SIZE,
                ));
            }
        } else if self.solid_state.is_none() {
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

            let data = if is_rar4 {
                // RAR4 solid: decode_file_at picks up rar4_solid_state directly
                self.decode_file_at(i, None)?
            } else {
                // RAR5 solid: temporarily take state to satisfy borrow checker
                let mut state = self.solid_state.take().unwrap();
                let data = self.decode_file_at(i, Some(&mut state))?;
                self.solid_state = Some(state);
                data
            };

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
        } else if self.solid_decoded_through >= target_idx as isize {
            self.solid_state = None;
            self.rar4_solid_state = None;
            self.solid_decoded_through = -1;
        } else {
            self.solid_state = None;
            self.rar4_solid_state = None;
            self.solid_decoded_through = -1;
        }

        let is_rar4 = self.entries[chain_start].header.format_version == 4;
        if is_rar4 {
            if self.rar4_solid_state.is_none() {
                self.rar4_solid_state = Some(rar4::decoder::Rar4DecoderState::new(
                    rar4::constants::RAR4_DEFAULT_DICT_SIZE,
                ));
            }
        } else if self.solid_state.is_none() {
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
            let written = if is_rar4 {
                self.decode_file_to(i, sink, None)?
            } else {
                let mut state = self.solid_state.take().unwrap();
                let w = self.decode_file_to(i, sink, Some(&mut state))?;
                self.solid_state = Some(state);
                w
            };
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
            total_packed = total_packed.checked_add(c.packed_size).ok_or_else(|| {
                RarError::LimitExceeded {
                    limit: max_packed,
                    context: format!("{}: packed size overflow", hdr.name),
                }
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
        packed_data.try_reserve_exact(total_packed as usize).map_err(|_| {
            RarError::LimitExceeded {
                limit: max_packed,
                context: format!("{}: cannot allocate packed data", hdr.name),
            }
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
            if !chunk.is_final {
                if let Some(expected_crc) = chunk.crc32_val {
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
        } else if hdr.format_version == 4 {
            // RAR4 decompression
            if hdr.comp_method >= 4 {
                return Err(RarError::Unsupported(
                    "RAR4 PPMd compression not yet supported".into(),
                ));
            }
            rar4::decoder::rar4_decompress(
                &payload.data,
                hdr.unpacked_size,
                self.rar4_solid_state.as_mut(),
            )
            .map_err(|e| RarError::Unsupported(e))?
        } else {
            compression::decompress(
                &payload.data,
                hdr.comp_method,
                hdr.unpacked_size,
                hdr.comp_dict_size,
                state,
            )
            .map_err(|e| RarError::Unsupported(e))?
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
            sink.write_all(&payload.data).map_err(|e| RarError::Io(e))?;
            payload.data.len() as u64
        } else if hdr.format_version == 4 {
            if hdr.comp_method >= 4 {
                return Err(RarError::Unsupported(
                    "RAR4 PPMd compression not yet supported".into(),
                ));
            }
            let raw = rar4::decoder::rar4_decompress(
                &payload.data,
                hdr.unpacked_size,
                self.rar4_solid_state.as_mut(),
            )
            .map_err(|e| RarError::Unsupported(e))?;
            sink.write_all(&raw).map_err(|e| RarError::Io(e))?;
            raw.len() as u64
        } else {
            crate::codec::decode_to_writer(
                &payload.data,
                hdr.unpacked_size,
                hdr.comp_dict_size,
                state,
                &mut sink,
            )
            .map_err(|e| RarError::Unsupported(e))?
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
            let (header_crc, extra_data, stored_hash, encr_params) =
                self.payload_extra_and_crc(plain_crc, plain_blake)?;
            if self.password.is_some() {
                let raw_data = fs::read(path)?;
                let packed_data = self.encrypt_payload_with(encr_params.as_ref(), &raw_data)?;
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
        let dsl = dict_size_for_data(file_size as usize);
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
        {
            let mut file = io::BufReader::with_capacity(1 << 20, File::open(path)?);
            let mut buf = vec![0u8; crate::codec::DEFAULT_CHUNK_SIZE];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                crc_hasher.update(&buf[..n]);
                if let Some(h) = blake_hasher.as_mut() {
                    h.update(&buf[..n]);
                }
                let mut progress: Option<&mut dyn FnMut(u64, u64)> = None;
                if let Some(cb) = self.progress_callback.as_deref_mut() {
                    let cb: &mut dyn FnMut(u64, u64) = cb;
                    progress = Some(cb);
                }
                let state = self.encoder_state.as_mut();
                let compressed = compression::compress_chunked(
                    &buf[..n],
                    method,
                    dsl,
                    crate::codec::DEFAULT_CHUNK_SIZE,
                    state,
                    n < buf.len(),
                    progress,
                )
                .map_err(|e| RarError::Unsupported(e))?;
                packed.extend(compressed);
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
            let (header_crc, extra_data, stored_hash, encr_params) =
                self.payload_extra_and_crc(plain_crc, plain_blake)?;
            if self.password.is_some() {
                let raw_data = fs::read(path)?;
                let packed_data = self.encrypt_payload_with(encr_params.as_ref(), &raw_data)?;
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

        let (header_crc, extra_data, stored_hash, encr_params) =
            self.payload_extra_and_crc(plain_crc, plain_blake)?;
        let packed_data = self.encrypt_payload_with(encr_params.as_ref(), &packed)?;
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
        if method == COMP_METHOD_STORE || sample_is_incompressible(data, method) {
            self.reset_solid_chain();
            let (header_crc, extra_data, stored_hash, encr_params) =
                self.payload_extra_and_crc(plain_crc, plain_blake)?;
            let packed_data = self.encrypt_payload_with(encr_params.as_ref(), data)?;
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
            let dsl = dict_size_for_data(data.len());
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
            .map_err(|e| RarError::Unsupported(e))?;
            if packed.len() >= data.len() {
                self.reset_solid_chain();
                let (header_crc, extra_data, stored_hash, encr_params) =
                    self.payload_extra_and_crc(plain_crc, plain_blake)?;
                let packed_data = self.encrypt_payload_with(encr_params.as_ref(), data)?;
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
                    self.payload_extra_and_crc(plain_crc, plain_blake)?;
                let packed_data = self.encrypt_payload_with(encr_params.as_ref(), &packed)?;
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

    /// Write a file entry, splitting across volumes if needed.
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
    fn payload_extra_and_crc(
        &self,
        plain_crc: u32,
        plain_blake: Option<[u8; 32]>,
    ) -> RarResult<(
        u32,
        Vec<u8>,
        Option<[u8; 32]>,
        Option<encryption::EncryptionParams>,
    )> {
        if let Some(password) = self.password.as_deref() {
            let params = encryption::EncryptionParams::generate_for_password(
                password,
                ENCR_PBKDF2_ITER_LOG,
            );
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
        &self,
        params: Option<&encryption::EncryptionParams>,
        plaintext: &[u8],
    ) -> RarResult<Vec<u8>> {
        match (self.password.as_deref(), params) {
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

fn dict_size_for_data(data_size: usize) -> u8 {
    // Cap the window at 1 MiB: larger windows lengthen the hash-chain walk on
    // incompressible data (each position traverses every in-window candidate),
    // which dominates compression time. 1 MiB covers virtually all real-world
    // match distances (WinRAR's default dictionary is 4 MiB).
    let base = 128 * 1024;
    let mut log = 0u8;
    while (base << log) < data_size && log < 3 {
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
        if pos >= SAMPLE_PROBE_HEAD && pos + SAMPLE_PROBE_TAIL <= data.len() {
            if incompressible_sample(&data[pos..pos + SAMPLE_PROBE_TAIL], method) {
                bad += 1;
            }
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
    for i in 0..5 {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if i < 4 {
            b |= 0x80;
        }
        out[i] = b;
    }
    out
}

fn get_volume_base(path: &Path) -> String {
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
        let rev = std::fs::read(&revs[0]).unwrap();
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
        assert!(vols.len() >= 1);

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
            assert!(sanitize_archive_path(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn sanitize_archive_path_normalizes_safe_names() {
        assert_eq!(sanitize_archive_path("a/b.txt").unwrap(), "a/b.txt");
        assert_eq!(sanitize_archive_path("a\\b.txt").unwrap(), "a/b.txt");
        assert_eq!(sanitize_archive_path("./a//b/./c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(sanitize_archive_path("dir/").unwrap(), "dir");
    }
}
