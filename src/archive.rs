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

/// A single entry in the archive (public API).
#[derive(Clone, Debug)]
pub struct ArchiveEntry {
    pub header: FileHeader,
    pub chunks: Vec<DataChunk>,
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
            volume_paths: Vec::new(),
            volume_size: None,
            current_volume: 0,
            volume_bytes_written: 0,
            progress_callback: None,
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
            volume_paths: Vec::new(),
            volume_size: None,
            current_volume: 0,
            volume_bytes_written: 0,
            progress_callback: None,
        };
        archive.open_read()?;
        Ok(archive)
    }

    /// Set the password for decryption.
    pub fn set_password(&mut self, password: &str) {
        self.password = Some(password.to_string());
    }

    /// Create a new RAR5 archive (overwrites existing file).
    pub fn create(path: impl AsRef<Path>) -> RarResult<Self> {
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
            password: None,
            header_encryption: false,
            archive_encr: None,
            volume_paths: Vec::new(),
            volume_size: None,
            current_volume: 0,
            volume_bytes_written: 0,
            progress_callback: None,
        };
        archive.open_write()?;
        Ok(archive)
    }

    /// Create a new multi-volume RAR5 archive.
    pub fn create_multivolume(path: impl AsRef<Path>, volume_size: u64) -> RarResult<Self> {
        let path = path.as_ref().to_path_buf();
        let volume_base = get_volume_base(&path);
        let vol_path = volume_path(path.parent().unwrap_or(Path::new(".")), &volume_base, 1);
        let mut archive = RarArchive {
            path,
            mode: Mode::Write,
            entries: Vec::new(),
            stream: None,
            format_version: 5,
            solid_state: None,
            rar4_solid_state: None,
            solid_decoded_through: -1,
            password: None,
            header_encryption: false,
            archive_encr: None,
            volume_paths: vec![vol_path.clone()],
            volume_size: Some(volume_size),
            current_volume: 1,
            volume_bytes_written: 0,
            progress_callback: None,
        };
        let f = File::create(&vol_path)?;
        archive.stream = Some(f);
        archive.write_signature()?;
        archive.write_archive_header_vol(None)?;
        archive.volume_bytes_written = archive.stream.as_ref().unwrap().stream_position()?;
        Ok(archive)
    }

    /// Create a new encrypted RAR5 archive (overwrites existing file).
    pub fn create_with_password(path: impl AsRef<Path>, password: &str) -> RarResult<Self> {
        let mut archive = Self::create(path)?;
        archive.password = Some(password.to_string());
        Ok(archive)
    }

    /// Create a new RAR5 archive with encrypted headers (overwrites existing
    /// file). Hides file names and the whole archive structure: the main
    /// archive header is followed by an archive-level encryption header and
    /// every subsequent block header is AES-256-CBC encrypted.
    ///
    /// Not supported for multi-volume archives.
    pub fn create_with_password_headers(path: impl AsRef<Path>, password: &str) -> RarResult<Self> {
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
            password: Some(password.to_string()),
            header_encryption: true,
            archive_encr: None,
            volume_paths: Vec::new(),
            volume_size: None,
            current_volume: 0,
            volume_bytes_written: 0,
            progress_callback: None,
        };
        archive.open_write()?;
        Ok(archive)
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
        let f = File::create(&self.path)?;
        self.stream = Some(f);
        self.write_signature()?;
        if self.header_encryption {
            // The archive-level encryption header precedes the main archive
            // header; every header after it (main, file, end) is encrypted.
            let password = self.password.as_ref().ok_or_else(|| {
                RarError::Encrypted("header encryption requires a password".into())
            })?;
            if self.volume_size.is_some() {
                return Err(RarError::Unsupported(
                    "header encryption is not supported for multi-volume archives".into(),
                ));
            }
            let encr = encryption::EncryptionParams::generate_for_password(
                password,
                ENCR_PBKDF2_ITER_LOG,
            );
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
            self.write_end_block()?;
            self.mode = Mode::Read; // prevent double-write
        }
        self.stream = None;
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
        let hdr = ArchiveHeader {
            flags: 0,
            extra_data: Vec::new(),
            volume_number: None,
        };
        let hdr_bytes = hdr.to_bytes();
        self.write_block_header(&hdr_bytes)
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
        let entry = self
            .entries
            .iter()
            .find(|e| e.name() == name)
            .ok_or_else(|| RarError::Format(format!("member not found: {name:?}")))?
            .clone();
        self.decode_single_file(&entry)
    }

    /// Extract all archive contents to `dest_dir`.
    pub fn extract_all(&mut self, dest_dir: impl AsRef<Path>) -> RarResult<()> {
        let dest = dest_dir.as_ref();
        let entries: Vec<_> = self.entries.clone();
        for entry in &entries {
            self.extract_entry(entry, dest)?;
        }
        Ok(())
    }

    /// Extract a single entry to `dest_dir`.
    pub fn extract(&mut self, name: &str, dest_dir: impl AsRef<Path>) -> RarResult<PathBuf> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.name() == name)
            .ok_or_else(|| RarError::Format(format!("member not found: {name:?}")))?
            .clone();
        self.extract_entry(&entry, dest_dir.as_ref())
    }

    fn extract_entry(&mut self, entry: &ArchiveEntry, dest_dir: &Path) -> RarResult<PathBuf> {
        let dest_path = dest_dir.join(&entry.header.name);

        if entry.is_dir() {
            fs::create_dir_all(&dest_path)?;
            return Ok(dest_path);
        }

        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let data = self.decode_single_file(entry)?;
        fs::write(&dest_path, &data)?;

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

    fn decode_single_file(&mut self, entry: &ArchiveEntry) -> RarResult<Vec<u8>> {
        // Find the index of this entry
        let target_idx = self
            .entries
            .iter()
            .position(|e| e.header.data_offset == entry.header.data_offset)
            .unwrap_or(0);

        // Check if this entry is part of a solid chain
        if self.is_solid_chain_member(target_idx) {
            return self.decode_solid_through(target_idx);
        }

        self.decode_file_at(target_idx, None)
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
        // Find the start of the solid chain (first non-directory file
        // at or before target_idx that isn't solid, followed by solid files)
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
                self.rar4_solid_state = Some(
                    rar4::decoder::Rar4DecoderState::new(rar4::constants::RAR4_DEFAULT_DICT_SIZE),
                );
            }
        } else if self.solid_state.is_none() {
            let dict_log = self.entries[chain_start].header.comp_dict_size;
            let dict_log32 = dict_log.max(0) as u32;
            let mut dict_size = 128 * 1024 * (1usize << dict_log32);
            if !dict_size.is_power_of_two() {
                dict_size = dict_size.next_power_of_two();
            }
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

    /// Read packed data for an entry, potentially across multiple volumes.
    fn read_packed_data(&mut self, idx: usize) -> RarResult<(Vec<u8>, bool)> {
        let entry = &self.entries[idx];
        let hdr = &entry.header;
        let chunks = &entry.chunks;

        if chunks.len() <= 1 {
            // Single chunk — read from primary stream or the chunk's volume
            let chunk = chunks.first();
            let (offset, size) = if let Some(c) = chunk {
                (c.data_offset, c.packed_size)
            } else {
                (hdr.data_offset, hdr.packed_size)
            };

            let vol_idx = chunk.map_or(0, |c| c.volume_index);
            let packed_data = if vol_idx == 0 {
                let stream = self.stream.as_mut().unwrap();
                stream.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; size as usize];
                stream.read_exact(&mut buf)?;
                buf
            } else {
                let mut f = File::open(&self.volume_paths[vol_idx])?;
                f.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; size as usize];
                f.read_exact(&mut buf)?;
                buf
            };

            // Decrypt if encrypted
            let encr_params = if !hdr.extra_data.is_empty() {
                encryption::parse_encryption_extra(&hdr.extra_data)?
            } else {
                None
            };
            let is_encrypted = encr_params.is_some();
            let mut packed_data = packed_data;
            if let Some(ref params) = encr_params {
                let password = self.password.as_ref().ok_or_else(|| {
                    RarError::Encrypted(format!("{}: encrypted, no password set", hdr.name))
                })?;
                if !params.verify_password(password) {
                    return Err(RarError::Encrypted("wrong password".into()));
                }
                packed_data = params.decrypt(&packed_data, password)?;
            }
            if is_encrypted && hdr.comp_method == COMP_METHOD_STORE {
                packed_data.truncate(hdr.unpacked_size as usize);
            }
            return Ok((packed_data, is_encrypted));
        }

        // Multi-volume: read and concatenate chunks
        let chunks_clone: Vec<DataChunk> = chunks.clone();
        let mut parts = Vec::new();
        for chunk in &chunks_clone {
            let mut f = File::open(&self.volume_paths[chunk.volume_index])?;
            f.seek(SeekFrom::Start(chunk.data_offset))?;
            let mut buf = vec![0u8; chunk.packed_size as usize];
            f.read_exact(&mut buf)?;

            // Verify intermediate chunk CRC (packed data CRC)
            if !chunk.is_final {
                if let Some(expected_crc) = chunk.crc32_val {
                    let mut hasher = crc32fast::Hasher::new();
                    hasher.update(&buf);
                    let actual_crc = hasher.finalize();
                    if actual_crc != expected_crc {
                        return Err(RarError::Crc {
                            expected: expected_crc,
                            actual: actual_crc,
                            context: format!(
                                "{} vol {}",
                                hdr.name, chunk.volume_index
                            ),
                        });
                    }
                }
            }
            parts.push(buf);
        }

        let packed_data: Vec<u8> = parts.into_iter().flatten().collect();

        // Handle encryption for multi-volume
        let encr_params = if !self.entries[idx].header.extra_data.is_empty() {
            encryption::parse_encryption_extra(&self.entries[idx].header.extra_data)?
        } else {
            None
        };
        let is_encrypted = encr_params.is_some();
        let mut packed_data = packed_data;
        if let Some(ref params) = encr_params {
            let password = self.password.as_ref().ok_or_else(|| {
                RarError::Encrypted(format!(
                    "{}: encrypted, no password set",
                    self.entries[idx].header.name
                ))
            })?;
            if !params.verify_password(password) {
                return Err(RarError::Encrypted("wrong password".into()));
            }
            packed_data = params.decrypt(&packed_data, password)?;
        }
        if is_encrypted && self.entries[idx].header.comp_method == COMP_METHOD_STORE {
            packed_data.truncate(self.entries[idx].header.unpacked_size as usize);
        }

        Ok((packed_data, is_encrypted))
    }

    /// Decode a single file, optionally with a shared DecoderState.
    fn decode_file_at(
        &mut self,
        idx: usize,
        state: Option<&mut DecoderState>,
    ) -> RarResult<Vec<u8>> {
        let hdr = &self.entries[idx].header;

        // Empty files / directories
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(Vec::new());
        }

        let (packed_data, is_encrypted) = self.read_packed_data(idx)?;
        let hdr = &self.entries[idx].header;

        let raw_data = if hdr.comp_method == COMP_METHOD_STORE {
            packed_data
        } else if hdr.format_version == 4 {
            // RAR4 decompression
            if hdr.comp_method >= 4 {
                return Err(RarError::Unsupported(
                    "RAR4 PPMd compression not yet supported".into(),
                ));
            }
            rar4::decoder::rar4_decompress(
                &packed_data,
                hdr.unpacked_size,
                self.rar4_solid_state.as_mut(),
            )
            .map_err(|e| RarError::Unsupported(e))?
        } else {
            compression::decompress(
                &packed_data,
                hdr.comp_method,
                hdr.unpacked_size,
                hdr.comp_dict_size,
                state,
            )
            .map_err(|e| RarError::Unsupported(e))?
        };

        // Verify CRC (skip for encrypted files — CRC is password-dependent)
        if !is_encrypted {
            if let Some(expected_crc) = self.entries[idx].header.crc32_val {
                let mut hasher = crc32fast::Hasher::new();
                hasher.update(&raw_data);
                let actual_crc = hasher.finalize();
                if actual_crc != expected_crc {
                    return Err(RarError::Crc {
                        expected: expected_crc,
                        actual: actual_crc,
                        context: self.entries[idx].header.name.clone(),
                    });
                }
            }
        }

        Ok(raw_data)
    }

    // ── Public API: creation ───────────────────────────────────────────────

    /// Add a file from the filesystem to the archive.
    pub fn add(
        &mut self,
        path: impl AsRef<Path>,
        compression_level: u8,
    ) -> RarResult<()> {
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

    fn add_file(
        &mut self,
        path: &Path,
        arcname: Option<&str>,
        level: u8,
    ) -> RarResult<()> {
        let raw_data = fs::read(path)?;
        let file_crc = {
            let mut h = crc32fast::Hasher::new();
            h.update(&raw_data);
            h.finalize()
        };

        let method = level_to_method(level);
        let (mut packed_data, actual_method, dict_size_log) = if method == COMP_METHOD_STORE {
            (raw_data.clone(), COMP_METHOD_STORE, 0u8)
        } else if sample_is_incompressible(&raw_data, method) {
            // Sample-probe large files: media/archives/random data would waste
            // minutes of match-finding to end up STORE anyway. Compressing a
            // 512 KiB head is ~20 ms and reliably flags incompressible input.
            (raw_data.clone(), COMP_METHOD_STORE, 0u8)
        } else {
            let dsl = dict_size_for_data(raw_data.len());
            let mut progress: Option<&mut dyn FnMut(u64, u64)> = None;
            if let Some(cb) = self.progress_callback.as_deref_mut() {
                let cb: &mut dyn FnMut(u64, u64) = cb;
                progress = Some(cb);
            }
            let compressed =
                compression::compress_with_progress(&raw_data, method, dsl, progress)
                    .map_err(|e| RarError::Unsupported(e))?;
            if compressed.len() >= raw_data.len() {
                (raw_data.clone(), COMP_METHOD_STORE, 0u8)
            } else {
                (compressed, method, dsl)
            }
        };

        let name = arcname
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().into_owned());
        let name = name.replace('\\', "/");

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
        let attrs = 0o100644u64;

        // Encrypt if password is set
        let extra_data = if let Some(ref password) = self.password {
            let enc_params = encryption::EncryptionParams::generate_for_password(
                password, ENCR_PBKDF2_ITER_LOG,
            );
            packed_data = enc_params.encrypt(&packed_data, password);
            enc_params.to_extra_bytes()
        } else {
            Vec::new()
        };

        self.write_file_entry(
            &name,
            raw_data.len() as u64,
            &packed_data,
            file_crc,
            actual_method,
            dict_size_log,
            &extra_data,
            attrs,
            mtime,
        )?;

        if let Some(cb) = self.progress_callback.as_deref_mut() {
            cb(raw_data.len() as u64, raw_data.len() as u64);
        }

        Ok(())
    }

    /// Add a directory entry only (no recursion).
    ///
    /// Writes the directory header without traversing children. Callers that
    /// enumerate files themselves (e.g. with exclusion filtering) use this to
    /// keep empty directories and the directory structure in the archive.
    pub fn add_directory_only(
        &mut self,
        path: impl AsRef<Path>,
        arcname: &str,
    ) -> RarResult<()> {
        let path = path.as_ref();
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
            let mut children: Vec<_> = fs::read_dir(path)?
                .filter_map(|e| e.ok())
                .collect();
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
        let file_crc = {
            let mut h = crc32fast::Hasher::new();
            h.update(data);
            h.finalize()
        };

        let method = level_to_method(compression_level);
        let mtime = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        let (mut packed_data, actual_method, dict_size_log) = if method == COMP_METHOD_STORE {
            (data.to_vec(), COMP_METHOD_STORE, 0u8)
        } else if sample_is_incompressible(data, method) {
            (data.to_vec(), COMP_METHOD_STORE, 0u8)
        } else {
            let dsl = dict_size_for_data(data.len());
            let mut progress: Option<&mut dyn FnMut(u64, u64)> = None;
            if let Some(cb) = self.progress_callback.as_deref_mut() {
                let cb: &mut dyn FnMut(u64, u64) = cb;
                progress = Some(cb);
            }
            let compressed = compression::compress_with_progress(data, method, dsl, progress)
                .map_err(|e| RarError::Unsupported(e))?;
            if compressed.len() >= data.len() {
                (data.to_vec(), COMP_METHOD_STORE, 0u8)
            } else {
                (compressed, method, dsl)
            }
        };

        // Encrypt if password is set
        let extra_data = if let Some(ref password) = self.password {
            let enc_params = encryption::EncryptionParams::generate_for_password(
                password, ENCR_PBKDF2_ITER_LOG,
            );
            packed_data = enc_params.encrypt(&packed_data, password);
            enc_params.to_extra_bytes()
        } else {
            Vec::new()
        };

        let name = arcname.replace('\\', "/");
        self.write_file_entry(
            &name,
            data.len() as u64,
            &packed_data,
            file_crc,
            actual_method,
            dict_size_log,
            &extra_data,
            0o100644,
            mtime,
        )?;

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
    ) -> RarResult<()> {
        let fh_base = FileHeader {
            name: name.to_string(),
            unpacked_size,
            packed_size: packed_data.len() as u64,
            attributes: attrs,
            mtime,
            crc32_val: Some(file_crc),
            comp_method: method,
            comp_dict_size: dict_size_log,
            host_os: OS_UNIX,
            file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
            extra_data: extra_data.to_vec(),
            ..Default::default()
        };

        if self.volume_size.is_none() {
            // Single-volume
            let hdr_bytes = fh_base.to_bytes();
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
            let chunk_packed =
                &packed_data[offset as usize..(offset + chunk_size) as usize];

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
/// Compressing the first 512 KiB with the same method costs ~20 ms and
/// reliably identifies media/archives/random data, which would otherwise
/// spend minutes in the match finder only to end up STORE anyway. The 90%
/// threshold is conservative: genuinely compressible inputs (text, code,
/// structured binary) compress the sample far below it.
fn sample_is_incompressible(data: &[u8], method: u8) -> bool {
    const SAMPLE: usize = 512 * 1024;
    if data.len() < 4 * SAMPLE {
        return false;
    }
    let sample = &data[..SAMPLE];
    let packed = compression::compress(sample, method, 0).unwrap_or_default();
    packed.len() >= SAMPLE * 9 / 10
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
            ar.add_bytes("c.bin", &(0..=255u8).collect::<Vec<_>>(), 0).unwrap();
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
        assert!(!raw.windows(b"secret/name.txt".len()).any(|w| w == b"secret/name.txt"));
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
        assert_eq!(vols2[0].file_name().unwrap().to_str().unwrap(), "disc.part1.rar");
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
}
