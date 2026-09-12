//! RAR 1.5-4.x (legacy) write path: member encoding, encryption,
//! emission and the parallel batch.
//!
//! Reached from the format-neutral member dispatchers in
//! `crate::format::shared::write_ops`; the wire-level primitives live in
//! the parent [`super`] module and the AES-128 range emitter in
//! [`super::cbc`].

use std::borrow::Cow;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::cbc::Rar30RangeEmitter;
use crate::archive::{ArchiveEntry, RarArchive, STREAM_COMPRESS_THRESHOLD};
#[cfg(feature = "parallel")]
use crate::archive::{BatchEntry, PARALLEL_COMPRESS_MAX_MEMBER, PARALLEL_COMPRESS_WAVE_BUDGET};
use crate::error::{RarError, RarResult};
use crate::format::shared::engine::{CountingWriter, CrcReader, SpillGuard, spill_path_for};
use crate::format::shared::stream_mut;
use crate::format::shared::write_ops::archive_name_from_path;
use crate::model::FileHeader;

/// Bytes a RAR4 segment reserves ahead of its payload in a volume: the fixed
/// FILE_HEAD, the encoded name, the optional salt and extended-time area,
/// plus the `-hp` `[8B salt][align16]` envelope. Budgeting a volume split
/// without this reserve lets every volume exceed `-v` by one header.
fn rar4_segment_header_reserve(
    encoded_name: &[u8],
    has_salt: bool,
    ext_time: Option<&[u8]>,
    header_encryption: bool,
) -> u64 {
    let mut reserve = u64::from(crate::format::rar4::write::FILE_HEADER_FIXED_SIZE)
        + encoded_name.len() as u64
        + if has_salt { 8 } else { 0 }
        + ext_time.map_or(0, |extra| extra.len() as u64);
    if header_encryption {
        reserve = 8 + reserve.next_multiple_of(16);
    }
    reserve
}

/// Write one RAR4 FILE_HEAD plus its segment data on the current volume.
/// `split_before` marks a continuation head and `split_after` a head whose
/// data continues on the next volume. Shared by the buffered and streaming
/// member paths.
#[allow(clippy::too_many_arguments)]
fn emit_rar4_segment(
    this: &mut crate::archive::RarArchive,
    encoded_name: &[u8],
    name_flags: u16,
    file_crc: u32,
    dos_time: u32,
    method: u8,
    packed_size: u32,
    unpacked_size: u32,
    data: &[u8],
    password: bool,
    salt: Option<[u8; 8]>,
    ext_time: Option<&[u8]>,
    solid_continuation: bool,
    is_dir: bool,
    comment: Option<Vec<u8>>,
    split_before: bool,
    split_after: bool,
) -> RarResult<(u64, u64)> {
    use crate::format::rar4::write::{
        FileHeaderParams, build_file_comment_block, build_file_header,
    };
    use crate::format::rar4::{
        FHD_COMMENT, FHD_EXTTIME, FHD_PASSWORD, FHD_SALT, FHD_SOLID, FHD_SPLIT_AFTER,
        FHD_SPLIT_BEFORE,
    };
    let mut fhd = name_flags;
    if split_before {
        fhd |= FHD_SPLIT_BEFORE;
    }
    if split_after {
        fhd |= FHD_SPLIT_AFTER;
    }
    if password {
        fhd |= FHD_PASSWORD;
    }
    if salt.is_some() {
        fhd |= FHD_SALT;
    }
    if ext_time.is_some() {
        fhd |= FHD_EXTTIME;
    }
    if solid_continuation {
        fhd |= FHD_SOLID;
    }
    if comment.is_some() {
        fhd |= FHD_COMMENT;
    }
    let params = FileHeaderParams {
        flags: fhd,
        packed_size,
        unpacked_size,
        host_os: 0,
        file_crc,
        file_time: dos_time,
        unp_ver: this.write_ctx().rar4_unp_ver,
        method,
        name: encoded_name,
        attr: if is_dir { 0x10 } else { 0x20 }, // directory bit : regular-file archive bit
        salt,
        ext_time,
        window_bits: 6, // 4 MiB dictionary
    };
    let mut hdr = build_file_header(&params)?;
    // Append the per-file comment subblock (COMM_HEAD 0x75) after the
    // extended-time area and fix the outer head size + head CRC.
    if let Some(comment) = &comment {
        let block = build_file_comment_block(comment);
        let new_head = u16::from_le_bytes([hdr[5], hdr[6]]) as usize + block.len();
        hdr[5..7].copy_from_slice(&(new_head as u16).to_le_bytes());
        hdr.extend_from_slice(&block);
        // A FILE_HEAD carrying a nested comment stops its CRC before
        // the trailing extended-time/comment area (matches the
        // reader's `header_crc_end`).
        let crc_end = crate::format::rar4::file_header_crc_end(&hdr);
        let crc = (crate::crc32::crc32(&hdr[2..crc_end]) & 0xFFFF) as u16;
        hdr[0..2].copy_from_slice(&crc.to_le_bytes());
    }
    let stream = stream_mut(&mut this.stream)?;
    // `-hp`: the file-header block is header-encrypted like every
    // other block after the main header. The member payload (data)
    // itself is NOT part of the ciphertext; it follows the encrypted
    // header on disk and is covered by member-level encryption (`-p`)
    // separately. The data offset is past the `[8B salt][align16]`
    // block, matching the read side's `block.header_end`.
    let (header_bytes, header_on_disk) = if this.header_encryption {
        let password = this
            .password
            .as_deref()
            .ok_or_else(|| RarError::Encrypted("header encryption requires a password".into()))?;
        crate::format::rar4::write::encrypt_block_header(&hdr, password)?
    } else {
        (hdr.clone(), hdr.len() as u64)
    };
    let data_offset = stream.stream_position()? + header_on_disk;
    stream.write_all(&header_bytes)?;
    stream.write_all(data)?;
    this.write_ctx_mut().volume_bytes_written += header_on_disk + data.len() as u64;
    Ok((data_offset, data.len() as u64))
}

/// Packed payload of a streamed RAR4 member: the bytes live in a file (the
/// source for STORE, the compression spill otherwise) and are read on demand,
/// encrypting on the fly for `-p` members.
enum Rar4PayloadSource {
    Plain {
        file: File,
    },
    Encrypted {
        file: File,
        plain_len: u64,
        emitter: Box<Rar30RangeEmitter>,
    },
}

impl Rar4PayloadSource {
    /// Read the on-disk payload range `[start, end)`.
    fn read_range(&mut self, start: u64, end: u64) -> RarResult<Vec<u8>> {
        match self {
            Self::Plain { file } => {
                let mut buf = vec![0u8; (end - start) as usize];
                file.seek(SeekFrom::Start(start))?;
                file.read_exact(&mut buf)?;
                Ok(buf)
            }
            Self::Encrypted {
                file,
                plain_len,
                emitter,
            } => {
                let mut buf = Vec::with_capacity((end - start) as usize);
                emitter.emit_to(file, *plain_len, start, end, &mut buf)?;
                Ok(buf)
            }
        }
    }
}

/// Scalar member fields shared by the RAR4 multi-volume split drivers.
struct Rar4SplitParams<'a> {
    encoded_name: &'a [u8],
    name_flags: u16,
    file_crc: u32,
    dos_time: u32,
    method: u8,
    unpacked_size: u64,
    password: bool,
    salt: Option<[u8; 8]>,
    ext_time: Option<&'a [u8]>,
    solid_continuation: bool,
    is_dir: bool,
    comment: Option<Vec<u8>>,
}

/// Split `packed_size` on-disk bytes across volumes: one FILE_HEAD plus one
/// segment per volume, using the RAR4 split convention (a non-final head
/// carries its own segment's CRC, the final head the whole-file CRC; the
/// unpacked size is the full file size in every head). `segment(offset, len)`
/// yields the segment's on-disk (already encrypted) bytes; the closure may
/// report progress itself.
fn emit_rar4_split<'a>(
    this: &mut RarArchive,
    params: &Rar4SplitParams<'_>,
    volume_size: u64,
    packed_size: u64,
    mut segment: impl FnMut(&mut RarArchive, u64, u64) -> RarResult<Cow<'a, [u8]>>,
) -> RarResult<Vec<crate::model::DataChunk>> {
    let needed = 7 + rar4_segment_header_reserve(
        params.encoded_name,
        params.salt.is_some(),
        params.ext_time,
        this.header_encryption,
    );
    let mut chunks = Vec::new();
    let mut sent = 0u64;
    let mut vol_index = this.write_ctx().current_volume - 1;
    let mut split_before = false;
    while sent < packed_size {
        // Roll to a volume with room for the header and the EOA.
        let mut rolled = false;
        loop {
            let used = this.write_ctx().volume_bytes_written;
            if volume_size.saturating_sub(used) > needed {
                break;
            }
            if rolled {
                return Err(RarError::InvalidOption(format!(
                    "volume size {volume_size} is too small for a RAR4 member header"
                )));
            }
            this.start_next_volume()?;
            vol_index = this.write_ctx().current_volume - 1;
            rolled = true;
        }
        let used = this.write_ctx().volume_bytes_written;
        let available = volume_size - used - needed;
        let chunk_size = (packed_size - sent).min(available);
        let split_after = sent + chunk_size < packed_size;
        let data = segment(this, sent, chunk_size)?;
        let head_crc = if split_after {
            crate::crc32::crc32(&data)
        } else {
            params.file_crc
        };
        let (data_offset, _) = emit_rar4_segment(
            this,
            params.encoded_name,
            params.name_flags,
            head_crc,
            params.dos_time,
            params.method,
            chunk_size as u32,
            params.unpacked_size as u32,
            &data,
            params.password,
            params.salt,
            params.ext_time,
            params.solid_continuation,
            params.is_dir,
            params.comment.clone(),
            split_before,
            split_after,
        )?;
        chunks.push(crate::model::DataChunk {
            volume_index: vol_index,
            data_offset,
            packed_size: chunk_size,
            crc32_val: Some(head_crc),
            is_final: !split_after,
            extra_data: Vec::new(),
        });
        sent += chunk_size;
        split_before = true;
    }
    Ok(chunks)
}

impl RarArchive {
    /// RAR4 solid-chain bookkeeping for one member: returns whether the
    /// member continues the run and advances the run state. A stored member
    /// ends the run (dropping the carried encoder state the trial may have
    /// advanced); a non-empty compressed member marks the run as started.
    ///
    /// Pre-RAR3 writers (unp_ver < 29) never flag `FHD_SOLID`: the reader
    /// derives solid continuation from the archive-level `MHD_SOLID` and
    /// member position instead.
    fn track_rar4_solid_member(&mut self, method: u8, unpacked_size: u64) -> bool {
        let continuation = self.write_ctx().solid_mode
            && method != crate::format::rar4::RAR4_METHOD_STORE
            && self.write_ctx().rar4_solid_run_has_member
            && self.write_ctx().rar4_unp_ver == 29;
        if method == crate::format::rar4::RAR4_METHOD_STORE {
            self.write_ctx_mut().rar4_solid_encoder = None;
            self.write_ctx_mut().legacy_solid_encoder = None;
            self.write_ctx_mut().rar4_solid_run_has_member = false;
        } else if unpacked_size != 0 {
            self.write_ctx_mut().rar4_solid_run_has_member = true;
        }
        continuation
    }

    /// Push the catalog entry for one emitted RAR4 member. Shared by the
    /// buffered, streaming and parallel emission paths so the header fields
    /// (including the nanosecond mtime) stay in lockstep.
    #[allow(clippy::too_many_arguments)]
    fn push_rar4_entry(
        &mut self,
        name: String,
        unpacked_size: u64,
        packed_size: u64,
        file_crc: u32,
        mtime: u32,
        mtime_ns: u32,
        method: u8,
        password: bool,
        salt: Option<[u8; 8]>,
        ext_time: Option<Vec<u8>>,
        comment: Option<Vec<u8>>,
        is_dir: bool,
        data_offset: u64,
        chunks: Vec<crate::model::DataChunk>,
    ) {
        self.entries.push(crate::archive::ArchiveEntry {
            header: crate::model::FileHeader {
                name,
                unpacked_size,
                packed_size,
                crc32_val: Some(file_crc),
                mtime,
                mtime_ns: Some(mtime_ns),
                comp_method: method.wrapping_sub(crate::format::rar4::RAR4_METHOD_STORE),
                host_os: 0,
                format_version: 4,
                unp_ver: self.write_ctx().rar4_unp_ver,
                data_offset,
                is_directory: is_dir,
                flags: if password {
                    crate::format::rar4::FHD_PASSWORD as u64
                } else {
                    0
                },
                salt,
                extra_data: ext_time.unwrap_or_default(),
                comment,
                ..Default::default()
            },
            chunks,
        });
    }

    /// RAR4 STORE path: write a member in the legacy container — STORE or
    /// LZSS-compressed (m1–m5), optionally AES-encrypted with the member
    /// password. A single-volume member is one FILE_HEAD + data; in a
    /// multi-volume set the member data is split at volume boundaries,
    /// writing `FHD_SPLIT_AFTER` on every non-final head and
    /// `FHD_SPLIT_BEFORE` on every continuation head.
    pub(crate) fn add_file_rar4(
        &mut self,
        path: &Path,
        arcname: Option<&str>,
        level: u8,
    ) -> RarResult<()> {
        let meta = fs::metadata(path)?;
        let file_size = meta.len();
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;
        let name = match arcname {
            Some(s) => s.to_string(),
            None => archive_name_from_path(path)?,
        };
        let name = name.replace('\\', "/");

        if self.progress.is_some() {
            self.report_progress(0, file_size);
        }

        let mtime_ns = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();

        // Large members stream: the file is compressed (or copied) into a
        // spill file and then streamed into the archive, so the whole member
        // never enters memory. The old codecs (v15/v20) and the deferred
        // solid append keep the buffered path — their encoders need the whole
        // input.
        if file_size >= STREAM_COMPRESS_THRESHOLD
            && self.write_ctx().rar4_unp_ver == 29
            && !self.write_ctx().rar4_solid_append
        {
            return self.add_rar4_file_streaming(path, &name, file_size, mtime, mtime_ns, level);
        }

        // Read the whole member, then (for level >= 1) LZSS-compress it.
        let mut reader = File::open(path)?;
        let mut data = Vec::with_capacity(file_size as usize);
        std::io::Read::read_to_end(&mut reader, &mut data)?;
        self.add_rar4_data(name, data, level, mtime, mtime_ns, None)
    }

    /// Create one large RAR4 member with bounded memory: compress the source
    /// into a spill file (falling back to STORE when compression does not
    /// help), optionally encrypting the payload on the fly, then emit the
    /// FILE_HEAD and stream the payload across volumes.
    ///
    /// Only the RAR29 codec streams; the v15/v20 encoders need the whole
    /// input and are handled by the buffered path in [`Self::add_file_rar4`].
    /// Large members use the LZ engine only (the PPMd trial and the
    /// automatic VM filters need whole-member buffers).
    fn add_rar4_file_streaming(
        &mut self,
        path: &Path,
        name: &str,
        file_size: u64,
        mtime: u32,
        mtime_ns: u32,
        level: u8,
    ) -> RarResult<()> {
        self.check_cancel()?;
        crate::format::rar4::create::ensure_member_size(file_size)?;
        self.emit_pending_rar4_comment()?;
        if self.write_ctx().solid_mode {
            self.maybe_reset_solid_for_extension(name);
        }

        let password = self.password.as_deref().is_some_and(|pw| !pw.is_empty());
        let spill = spill_path_for(&self.path);
        let _guard = SpillGuard(spill.clone());

        // ── Compress into the spill (level 0 skips straight to STORE) and
        // hash the plaintext in the same pass. ──
        let file_crc;
        let packed_len;
        let method;
        if level == 0 {
            let mut reader = File::open(path)?;
            let mut hasher = crc32fast::Hasher::new();
            let mut buf = vec![0u8; 1 << 20];
            let mut copied = 0u64;
            loop {
                self.check_cancel()?;
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                copied += n as u64;
                self.report_progress(copied, file_size);
            }
            if copied != file_size {
                return Err(RarError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "file changed size while being archived: expected {file_size} bytes, read {copied}"
                    ),
                )));
            }
            file_crc = hasher.finalize();
            packed_len = file_size;
            method = crate::format::rar4::RAR4_METHOD_STORE;
        } else {
            let mut source = CrcReader {
                inner: File::open(path)?,
                hasher: crc32fast::Hasher::new(),
            };
            let mut spill_file = crate::fs::atomic::read_write_create(&spill)?;
            let mut counter = CountingWriter::new(&mut spill_file);
            {
                let options = crate::codec::legacy::rar29_encoder::options_for_level(level);
                let progress = self.progress.clone();
                let cancel = self.cancel.clone();
                let member = self.progress_member;
                let mut report = |position: usize| -> bool {
                    if cancel
                        .as_ref()
                        .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
                    {
                        return false;
                    }
                    if let Some(progress) = &progress {
                        progress.lock().expect("progress lock").report(
                            member,
                            position as u64,
                            file_size,
                        );
                    }
                    true
                };
                if self.write_ctx().solid_mode {
                    let encoder =
                        self.write_ctx_mut()
                            .rar4_solid_encoder
                            .get_or_insert_with(|| {
                                crate::codec::legacy::rar29_encoder::Unpack29Encoder::with_options(
                                    options,
                                )
                            });
                    encoder.encode_member_streaming(
                        &mut source,
                        &mut counter,
                        Some(&mut report),
                    )?;
                } else {
                    let mut encoder =
                        crate::codec::legacy::rar29_encoder::Unpack29Encoder::with_options(options);
                    encoder.encode_member_streaming(
                        &mut source,
                        &mut counter,
                        Some(&mut report),
                    )?;
                }
            }
            let read = source.hasher.finalize();
            file_crc = read;
            let compressed = counter.written();
            if compressed < file_size {
                packed_len = compressed;
                method = crate::format::rar4::RAR4_METHOD_STORE + level;
            } else {
                // Compression is a net loss: stream STORE from the source
                // instead (the spill is dropped by its guard).
                packed_len = file_size;
                method = crate::format::rar4::RAR4_METHOD_STORE;
            }
        }

        // The payload source is the source file for STORE, the spill for a
        // compressed member; a password wraps either in the RAR29 cipher.
        let plain_len = packed_len;
        let source_path = if method == crate::format::rar4::RAR4_METHOD_STORE {
            path.to_path_buf()
        } else {
            spill.clone()
        };
        let mut salt = None;
        let mut source = if password {
            let mut salt_bytes = [0u8; 8];
            rand::fill(&mut salt_bytes);
            let cipher = crate::crypto::Rar30Cipher::new(
                self.password
                    .as_deref()
                    .expect("password checked above")
                    .as_bytes(),
                Some(salt_bytes),
            )
            .map_err(|e| RarError::Format(format!("RAR4 member key setup: {e:?}")))?;
            salt = Some(salt_bytes);
            Rar4PayloadSource::Encrypted {
                file: File::open(&source_path)?,
                plain_len,
                emitter: Box::new(Rar30RangeEmitter::new(cipher)),
            }
        } else {
            Rar4PayloadSource::Plain {
                file: File::open(&source_path)?,
            }
        };
        let packed_size = if password {
            plain_len.next_multiple_of(16)
        } else {
            plain_len
        };
        crate::format::rar4::create::ensure_member_size(packed_size)?;

        // Solid-chain bookkeeping mirrors the buffered path: STORE ends the
        // run (and drops the encoder state the trial may have advanced).
        let solid_continuation = self.track_rar4_solid_member(method, file_size);

        let ext_time = crate::format::rar4::write::build_ext_time(Some(mtime_ns));
        let dos_time = crate::format::rar4::write::unix_to_dos_time(mtime);
        let (encoded_name, name_flags) = crate::format::rar4::write::encode_file_name(name);
        let unpacked_size = file_size;
        let mut chunks = Vec::<crate::model::DataChunk>::new();

        match self.write_ctx().volume_size {
            None => {
                // Header first (packed size is known), then stream the
                // payload in bounded chunks.
                let (data_offset, _) = emit_rar4_segment(
                    self,
                    &encoded_name,
                    name_flags,
                    file_crc,
                    dos_time,
                    method,
                    packed_size as u32,
                    unpacked_size as u32,
                    &[],
                    password,
                    salt,
                    ext_time.as_deref(),
                    solid_continuation,
                    false,
                    None,
                    false,
                    false,
                )?;
                const COPY: u64 = 1 << 20;
                let mut pos = 0u64;
                while pos < packed_size {
                    self.check_cancel()?;
                    let end = (pos + COPY).min(packed_size);
                    let chunk = source.read_range(pos, end)?;
                    stream_mut(&mut self.stream)?.write_all(&chunk)?;
                    self.write_ctx_mut().volume_bytes_written += chunk.len() as u64;
                    pos = end;
                    self.report_progress(pos, file_size);
                }
                chunks.push(crate::model::DataChunk {
                    volume_index: 0,
                    data_offset,
                    packed_size,
                    crc32_val: Some(file_crc),
                    is_final: true,
                    extra_data: Vec::new(),
                });
            }
            Some(volume_size) => {
                chunks = {
                    let params = Rar4SplitParams {
                        encoded_name: &encoded_name,
                        name_flags,
                        file_crc,
                        dos_time,
                        method,
                        unpacked_size: file_size,
                        password,
                        salt,
                        ext_time: ext_time.as_deref(),
                        solid_continuation,
                        is_dir: false,
                        comment: None,
                    };
                    emit_rar4_split(
                        self,
                        &params,
                        volume_size,
                        packed_size,
                        |this, offset, len| {
                            let chunk = source.read_range(offset, offset + len)?;
                            this.report_progress(offset + len, file_size);
                            Ok(Cow::Owned(chunk))
                        },
                    )?
                };
            }
        }

        self.push_rar4_entry(
            name.to_string(),
            unpacked_size,
            packed_size,
            file_crc,
            mtime,
            mtime_ns,
            method,
            password,
            salt,
            ext_time,
            None,
            false,
            0,
            chunks,
        );
        self.report_progress(file_size, file_size);
        Ok(())
    }

    /// Encode one RAR4 member from in-memory bytes: CRC, then the smallest
    /// of LZ / PPMd (m4+) / auto-filter candidates / STORE, per-member
    /// encryption, and the FILE_HEAD + payload emission (single-volume or
    /// split across volumes). Shared by the file path (`add_file_rar4`,
    /// which reads the member first) and the bytes path (`add_bytes`).
    /// Emit a queued RAR4 archive comment (`rar4_writer_comment`) as a
    /// NEWSUB `CMT` block at the current stream position, then clear the
    /// queue. Only the 35-byte CMT header is header-encrypted under `-hp`;
    /// the comment payload follows as plaintext data (the same rule as
    /// FILE members).
    fn emit_pending_rar4_comment(&mut self) -> RarResult<()> {
        let Some(text) = self.write_ctx_mut().rar4_writer_comment.take() else {
            return Ok(());
        };
        if text.is_empty() {
            return Ok(());
        }
        const CMT_HEAD: usize = crate::archive::rar4_edit::CMT_HEAD_SIZE;
        let (payload, unicode) = crate::archive::rar4_edit::encode_comment_text(&text);
        let block = crate::archive::rar4_edit::build_comment_block(&payload, unicode);
        let stream = stream_mut(&mut self.stream)?;
        if self.header_encryption {
            let password = self.password.as_deref().ok_or_else(|| {
                RarError::Encrypted("header encryption requires a password".into())
            })?;
            let (ciphertext, on_disk) =
                crate::format::rar4::write::encrypt_block_header(&block[..CMT_HEAD], password)?;
            stream.write_all(&ciphertext)?;
            stream.write_all(&block[CMT_HEAD..])?;
            self.write_ctx_mut().volume_bytes_written += on_disk + (block.len() - CMT_HEAD) as u64;
        } else {
            stream.write_all(&block)?;
            self.write_ctx_mut().volume_bytes_written += block.len() as u64;
        }
        Ok(())
    }

    /// Queue the archive comment for a RAR4 create/repack writer (emitted
    /// before the first member).
    pub(crate) fn set_rar4_writer_comment(&mut self, text: Option<Vec<u8>>) {
        self.write_ctx_mut().rar4_writer_comment = text;
    }

    pub(crate) fn add_rar4_data(
        &mut self,
        name: String,
        data: Vec<u8>,
        level: u8,
        mtime: u32,
        mtime_ns: u32,
        comment: Option<Vec<u8>>,
    ) -> RarResult<()> {
        self.check_cancel()?;
        crate::format::rar4::create::ensure_member_size(data.len() as u64)?;
        // Deferred solid-append: the member cannot be streamed after an
        // existing solid chain; buffer it and let close() repack the whole
        // archive (surviving members + these additions).
        if self.write_ctx().rar4_solid_append {
            self.write_ctx_mut().rar4_solid_append_entries.push(
                crate::archive::rar4_edit::SolidAppendEntry {
                    name,
                    data,
                    level,
                    mtime,
                    mtime_ns,
                },
            );
            return Ok(());
        }
        // A queued archive comment is emitted right before the first member
        // (it must precede every member; the queue is consumed once).
        self.emit_pending_rar4_comment()?;
        // A directory member is written as a zero-byte placeholder whose name
        // ends in `/`; its on-disk attribute is the directory bit (0x10) rather
        // than the regular-file archive bit (0x20).
        let is_dir = name.ends_with('/');
        let file_size = data.len() as u64;
        let file_crc = crate::crc32::crc32(&data);

        // Compress with the RAR29 LZSS encoder (m1–m5).  If compressing does
        // not shrink the data, fall back to STORE.  On m4/m5 (non-solid,
        // non-empty members) a PPMd pass is tried too and the smallest of
        // LZ / PPMd / STORE wins; PPMd is where RAR4's text-level ratio
        // advantage over LZ comes from, matching the pre-6.x WinRARs that
        // could still produce PPMd blocks.  `method` is the on-disk byte
        // (0x30 = store, 0x31–0x35 = m1–m5); `packed` is what the write
        // pipeline emits; `unpacked_size` is always the original size.
        if self.write_ctx().solid_mode {
            self.maybe_reset_solid_for_extension(&name);
        }
        let (mut packed, method) = self.encode_rar4_member(&data, level)?;
        let unpacked_size = file_size;

        // Solid-chain bookkeeping (mirrors rars' `solid_run_has_member`
        // logic): a member is a chain continuation when it compresses and the
        // run has already emitted a member; storing a member rebuilds the
        // encoder and ends the run. The reader keeps its window/tables across
        // members flagged `FHD_SOLID`, so the flags and the encoder must stay
        // in lockstep.
        let solid_continuation = self.track_rar4_solid_member(method, unpacked_size);

        let ext_time = crate::format::rar4::write::build_ext_time(Some(mtime_ns));

        // Member-level encryption (WinRAR `-p`), dispatched on the cipher
        // generation (see `rar4_member_encrypt`). The header carries the
        // RAR29 salt (`FHD_SALT`); the old codecs encrypt without one but
        // still flag `FHD_PASSWORD`. `packed_size` covers the padded
        // ciphertext; the header CRC stays the plaintext CRC and is checked
        // after decryption.
        let password_encrypted = self.password.as_deref().is_some_and(|pw| !pw.is_empty());
        let mut salt = None;
        if password_encrypted {
            salt = self.rar4_member_encrypt(&mut packed)?;
        }
        let packed_size = packed.len() as u64;

        let dos_time = crate::format::rar4::write::unix_to_dos_time(mtime);
        let (encoded_name, name_flags) = crate::format::rar4::write::encode_file_name(&name);

        match self.write_ctx().volume_size {
            None => {
                // ── Single-volume ──
                let (data_offset, _) = emit_rar4_segment(
                    self,
                    &encoded_name,
                    name_flags,
                    file_crc,
                    dos_time,
                    method,
                    packed_size as u32,
                    unpacked_size as u32,
                    &packed,
                    password_encrypted,
                    salt,
                    ext_time.as_deref(),
                    solid_continuation,
                    is_dir,
                    comment.clone(),
                    false,
                    false,
                )?;
                self.push_rar4_entry(
                    name,
                    unpacked_size,
                    packed_size,
                    file_crc,
                    mtime,
                    mtime_ns,
                    method,
                    password_encrypted,
                    salt,
                    ext_time,
                    comment,
                    is_dir,
                    data_offset,
                    vec![crate::model::DataChunk {
                        volume_index: 0,
                        data_offset,
                        packed_size,
                        crc32_val: Some(file_crc),
                        is_final: true,
                        extra_data: Vec::new(),
                    }],
                );
                self.report_progress(file_size, file_size);
                Ok(())
            }
            Some(volume_size) => {
                // ── Multi-volume: split the packed member across volumes ──
                let chunks = {
                    let params = Rar4SplitParams {
                        encoded_name: &encoded_name,
                        name_flags,
                        file_crc,
                        dos_time,
                        method,
                        unpacked_size,
                        password: password_encrypted,
                        salt,
                        ext_time: ext_time.as_deref(),
                        solid_continuation,
                        is_dir,
                        comment: comment.clone(),
                    };
                    emit_rar4_split(self, &params, volume_size, packed_size, |_, offset, len| {
                        Ok(Cow::Borrowed(
                            &packed[offset as usize..(offset + len) as usize],
                        ))
                    })?
                };
                self.push_rar4_entry(
                    name,
                    unpacked_size,
                    packed_size,
                    file_crc,
                    mtime,
                    mtime_ns,
                    method,
                    password_encrypted,
                    salt,
                    ext_time,
                    comment,
                    is_dir,
                    0,
                    chunks,
                );
                self.report_progress(file_size, file_size);
                Ok(())
            }
        }
    }
    /// Encode one RAR4 member payload, dispatching on the archive's legacy
    /// member version (`rar4_unp_ver`). RAR29 (the default) keeps the full
    /// engine set — LZSS with auto VM filters and a PPMd trial on m4/m5.
    /// RAR 1.5/2.x members (`v15`/`v20`) mirror the `rars` legacy writers'
    /// level ladders, and STORE wins whenever the configured codec cannot
    /// shrink the data. In solid archives the legacy encoder instance is
    /// reused across the members of a run so its adaptive tables (and the
    /// RAR 2.x window) carry over — historical WinRAR produced solid
    /// RAR 1.5/2.x archives this way too.
    fn encode_rar4_member(&mut self, data: &[u8], level: u8) -> RarResult<(Vec<u8>, u8)> {
        let unp_ver = self.write_ctx().rar4_unp_ver;
        if unp_ver == 29 {
            return self.encode_rar29_member(data, level);
        }
        if !(1..=5).contains(&level) {
            return Ok((data.to_vec(), crate::format::rar4::RAR4_METHOD_STORE));
        }
        let method = crate::format::rar4::RAR4_METHOD_STORE + level;
        let packed = if self.write_ctx().solid_mode {
            if self.write_ctx().legacy_solid_encoder.is_none() {
                let encoder = build_legacy_solid_encoder(unp_ver, level)?;
                self.write_ctx_mut().legacy_solid_encoder = Some(encoder);
            }
            use crate::archive::LegacySolidEncoder;
            match self.write_ctx_mut().legacy_solid_encoder.as_mut().unwrap() {
                LegacySolidEncoder::Rar15(encoder) => encoder.encode_member(data)?,
                LegacySolidEncoder::Rar20(encoder) => encoder.encode_member(data)?,
            }
        } else {
            encode_legacy_codec_member(data, level, unp_ver)?
        };
        if packed.len() < data.len() {
            Ok((packed, method))
        } else {
            Ok((data.to_vec(), crate::format::rar4::RAR4_METHOD_STORE))
        }
    }

    /// Member-level encryption (WinRAR `-p`), dispatched on the member's
    /// cipher generation. RAR29 members get the RAR30 AES-128-CBC cipher
    /// with a fresh per-member 8-byte salt (flagged `FHD_SALT`); RAR 2.x
    /// members use the RAR20 block cipher (16-byte padded, no salt); RAR
    /// 1.5 members the RAR15 stream XOR (no padding, no salt). Returns the
    /// salt for RAR29 (written into the file header) and `None` for the
    /// saltless old codecs.
    fn rar4_member_encrypt(&mut self, packed: &mut Vec<u8>) -> RarResult<Option<[u8; 8]>> {
        let Some(pw) = self.password.as_deref().filter(|pw| !pw.is_empty()) else {
            return Ok(None);
        };
        match self.write_ctx().rar4_unp_ver {
            15 => {
                crate::crypto::Rar15Cipher::new(pw.as_bytes()).crypt_in_place(packed);
                Ok(None)
            }
            20 => {
                let pad = (16 - packed.len() % 16) % 16;
                packed.resize(packed.len() + pad, 0);
                crate::crypto::Rar20Cipher::new(pw.as_bytes())
                    .encrypt_in_place(packed)
                    .map_err(|e| RarError::Format(format!("RAR4 member (RAR20) encrypt: {e}")))?;
                Ok(None)
            }
            29 => {
                let mut salt = [0u8; 8];
                rand::fill(&mut salt);
                let mut cipher = crate::crypto::Rar30Cipher::new(pw.as_bytes(), Some(salt))
                    .map_err(|e| RarError::Format(format!("RAR4 member key setup: {e:?}")))?;
                let pad = (16 - packed.len() % 16) % 16;
                packed.resize(packed.len() + pad, 0);
                cipher
                    .encrypt_in_place(packed)
                    .map_err(|e| RarError::Format(format!("RAR4 member encrypt: {e:?}")))?;
                Ok(Some(salt))
            }
            other => Err(RarError::Unsupported(format!(
                "RAR4 write encryption: unp_ver {other} has no cipher"
            ))),
        }
    }

    fn encode_rar29_member(&mut self, data: &[u8], level: u8) -> RarResult<(Vec<u8>, u8)> {
        // Compress with the RAR29 LZSS encoder (m1–m5). If compressing does
        // not shrink the data, fall back to STORE. Non-solid members also try
        // the automatic VM filters and, on m4/m5, a PPMd pass; the smallest
        // candidate wins (see `best_rar29_member`).
        if !(1..=5).contains(&level) {
            return Ok((data.to_vec(), crate::format::rar4::RAR4_METHOD_STORE));
        }
        if !self.write_ctx().solid_mode {
            return Ok(match best_rar29_member(data, level)? {
                Some(best) => best,
                None => (data.to_vec(), crate::format::rar4::RAR4_METHOD_STORE),
            });
        }
        // Solid: reuse the persistent encoder so its sliding window, Huffman
        // table and PPMd model state carry across the members of the run
        // (this is what makes a real -ms archive compress better than
        // independent members). Each compressed member is measured both ways
        // (LZ and PPMd, continuing the model when the run is already in PPMd)
        // and the smaller wins. Filters stay out of solid runs (a filtered
        // member's window holds the transformed bytes).
        use crate::codec::legacy::rar29_encoder::{Unpack29Encoder, options_for_level};
        let encoder = self
            .write_ctx_mut()
            .rar4_solid_encoder
            .get_or_insert_with(|| Unpack29Encoder::with_options(options_for_level(level)));
        let lz = if data.is_empty() {
            encoder.encode_member(data)?
        } else {
            encoder.encode_solid_member(data)?
        };
        if lz.len() < data.len() {
            Ok((lz, crate::format::rar4::RAR4_METHOD_STORE + level))
        } else {
            Ok((data.to_vec(), crate::format::rar4::RAR4_METHOD_STORE))
        }
    }

    /// Write one RAR4 directory FILE_HEAD member (WinRAR convention: zero
    /// packed/unpacked sizes, CRC 0, `attr = 0x10`, `unp_ver 20`, name
    /// without a trailing slash; directories carry no data payload).
    pub(crate) fn write_rar4_dir_entry(
        &mut self,
        name: &str,
        mtime_secs: u32,
        mtime_ns: u32,
    ) -> RarResult<()> {
        use crate::format::rar4::write::{
            FileHeaderParams, build_ext_time, build_file_header, encode_file_name, unix_to_dos_time,
        };
        // A queued archive comment must precede the first member, whichever
        // kind it is; directories reached before any file flush it here.
        self.emit_pending_rar4_comment()?;
        let (encoded_name, name_flags) = encode_file_name(name);
        let ext_time = build_ext_time(Some(mtime_ns));
        let mut flags = name_flags;
        if ext_time.is_some() {
            flags |= crate::format::rar4::FHD_EXTTIME;
        }
        let params = FileHeaderParams {
            flags,
            packed_size: 0,
            unpacked_size: 0,
            host_os: 0,
            file_crc: 0,
            file_time: unix_to_dos_time(mtime_secs),
            unp_ver: 20,
            method: crate::format::rar4::RAR4_METHOD_STORE,
            name: &encoded_name,
            attr: 0x10,
            // All window bits set: the RAR4 directory marker that UnRAR and
            // WinRAR use to classify a member as a directory (files carry a
            // 0..=6 dictionary-size value instead).
            window_bits: 7,
            salt: None,
            ext_time: ext_time.as_deref(),
        };
        let hdr = build_file_header(&params)?;
        // Multi-volume: roll to a volume with room for this head plus the
        // 7-byte end-of-archive block (same rule as file members). A volume
        // too small for even a fresh header must error instead of rolling
        // forever.
        if let Some(volume_size) = self.write_ctx().volume_size {
            let mut rolled = false;
            loop {
                let used = self.write_ctx().volume_bytes_written;
                if volume_size.saturating_sub(used) > 7 + hdr.len() as u64 {
                    break;
                }
                if rolled {
                    return Err(RarError::InvalidOption(format!(
                        "volume size {volume_size} is too small for a RAR4 directory header"
                    )));
                }
                self.start_next_volume()?;
                rolled = true;
            }
        }
        let stream = stream_mut(&mut self.stream)?;
        stream.write_all(&hdr)?;
        self.write_ctx_mut().volume_bytes_written += hdr.len() as u64;
        let head_crc = u16::from_le_bytes([hdr[0], hdr[1]]);
        self.entries.push(ArchiveEntry {
            header: FileHeader {
                name: name.to_string(),
                unpacked_size: 0,
                packed_size: 0,
                attributes: 0x10,
                mtime: mtime_secs,
                mtime_ns: ext_time.is_some().then_some(mtime_ns),
                crc32_val: Some(0),
                comp_method: 0,
                host_os: 0,
                format_version: 4,
                unp_ver: 20,
                legacy_head_crc: Some(head_crc),
                is_directory: true,
                extra_data: ext_time.unwrap_or_default(),
                ..Default::default()
            },
            chunks: Vec::new(),
        });
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  RAR4 parallel batch compression (feature `parallel`): independent
//  non-solid members are compressed on the pool, then emitted in archive
//  order on the writing thread. Byte-identical to the sequential path
//  (each member uses a fresh engine, exactly like add_file_rar4 non-solid).
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "parallel")]
pub(crate) struct Rar4PreparedMember {
    pub name: String,
    pub mtime: u32,
    pub mtime_ns: u32,
    pub file_size: u64,
    pub file_crc: u32,
    pub packed: Vec<u8>,
    pub method: u8,
}

/// RAR 2.x ladder: candidate counts 16/64/256/512/1024 + lazy matching +
/// lookahead 2 + optimal parse on m4/m5 + audio encoding on m2–m5
/// (mirrors rars' `Unpack20Encoder` write ladder exactly).
fn legacy_rar20_options(level: u8) -> crate::codec::legacy::rar20_encoder::EncodeOptions {
    use crate::codec::legacy::rar20_encoder::EncodeOptions;
    let candidates = match level {
        1 => 16,
        2 => 64,
        3 => 256,
        4 => 512,
        _ => 1024,
    };
    EncodeOptions::new(candidates)
        .with_lazy_matching(true)
        .with_lazy_lookahead(2)
        .with_optimal_parse(level >= 4)
        .with_try_audio(level > 1)
}

/// RAR 1.5 ladder: old-distance tokens off on m1/m2, st-mode literal runs
/// off on m1/m2, max long-match distance 4/8/16/24 KiB / unbounded, lazy
/// matching off at every level (mirrors rars' `Unpack15Encoder` ladder).
fn legacy_rar15_options(level: u8) -> crate::codec::legacy::rar15_encoder::EncodeOptions {
    use crate::codec::legacy::rar15_encoder::EncodeOptions;
    match level {
        1 => EncodeOptions::new()
            .with_old_distance_tokens(false)
            .with_lazy_matching(false)
            .with_stmode_literal_runs(false)
            .with_max_long_match_distance(4 * 1024),
        2 => EncodeOptions::new()
            .with_old_distance_tokens(false)
            .with_lazy_matching(false)
            .with_stmode_literal_runs(false)
            .with_max_long_match_distance(8 * 1024),
        3 => EncodeOptions::new()
            .with_lazy_matching(false)
            .with_max_long_match_distance(16 * 1024),
        4 => EncodeOptions::new()
            .with_lazy_matching(false)
            .with_max_long_match_distance(24 * 1024),
        _ => EncodeOptions::new().with_lazy_matching(false),
    }
}

/// Encode a RAR4 member with the legacy codecs (unp_ver 15/20) using a
/// fresh instance (non-solid semantics). Shared by the sequential member
/// writer (`encode_rar4_member`) and the parallel batch preparation
/// (`prepare_rar4_file_member`). STORE fallback stays with the callers'
/// size comparison.
fn encode_legacy_codec_member(data: &[u8], level: u8, unp_ver: u8) -> RarResult<Vec<u8>> {
    match unp_ver {
        20 => Ok(
            crate::codec::legacy::rar20_encoder::unpack20_encode_auto_with_options(
                data,
                legacy_rar20_options(level),
            )?,
        ),
        15 => Ok(
            crate::codec::legacy::rar15_encoder::Unpack15Encoder::with_options(
                legacy_rar15_options(level),
            )
            .encode_member(data)?,
        ),
        other => Err(RarError::Unsupported(format!(
            "RAR4 write dispatch: unp_ver {other} has no encoder"
        ))),
    }
}

/// Build the persistent encoder for a solid RAR 1.5/2.x run; the same
/// instance is reused for every member of the run (rars' `solid_encoder`
/// reuse). The run's level defaults to the first member's ladder options,
/// matching the RAR29 solid encoder's get-or-create semantics.
fn build_legacy_solid_encoder(
    unp_ver: u8,
    level: u8,
) -> RarResult<crate::archive::LegacySolidEncoder> {
    use crate::archive::LegacySolidEncoder;
    match unp_ver {
        20 => Ok(LegacySolidEncoder::Rar20(
            crate::codec::legacy::rar20_encoder::Unpack20Encoder::with_options(
                legacy_rar20_options(level),
            ),
        )),
        15 => Ok(LegacySolidEncoder::Rar15(Box::new(
            crate::codec::legacy::rar15_encoder::Unpack15Encoder::with_options(
                legacy_rar15_options(level),
            ),
        ))),
        other => Err(RarError::Unsupported(format!(
            "RAR4 write dispatch: unp_ver {other} has no encoder"
        ))),
    }
}

/// Non-solid RAR29 candidate selection: the smallest of LZ, the automatic VM
/// filters (E8/E8E9, delta, audio) and the PPMd trial on m4/m5. Returns
/// `None` when nothing shrinks the input, so an owning caller can fall back
/// to STORE without copying its buffer.
///
/// The solid-run path is separate: it reuses the persistent encoder and
/// measures LZ against the chain-continuing PPMd trial
/// (`Unpack29Encoder::encode_solid_member`).
fn best_rar29_member(data: &[u8], level: u8) -> RarResult<Option<(Vec<u8>, u8)>> {
    use crate::codec::legacy::rar29_encoder::{
        Rar29FilterKind, Unpack29Encoder, options_for_level,
    };
    if !(1..=5).contains(&level) {
        return Ok(None);
    }
    let options = options_for_level(level);
    let method = crate::format::rar4::RAR4_METHOD_STORE + level;
    let lz = Unpack29Encoder::with_options(options).encode_member(data)?;
    let mut best_len = lz.len();
    let mut best: (Vec<u8>, u8) = (lz, method);

    if !data.is_empty() {
        // Auto filters on binary members (any level): every candidate is
        // measured with its own throwaway encoder (no chain state). The RAR5
        // scanners gate the search — text never produces x86 clusters or
        // structured deltas.
        let mut candidates: Vec<(Rar29FilterKind, Vec<std::ops::Range<usize>>)> = Vec::new();
        let e8e9 = crate::codec::common::filters::auto_x86_filter_ranges(data, true);
        if !e8e9.is_empty() {
            candidates.push((Rar29FilterKind::E8E9, e8e9));
        }
        let e8 = crate::codec::common::filters::auto_x86_filter_ranges(data, false);
        if !e8.is_empty() {
            candidates.push((Rar29FilterKind::E8, e8));
        }
        if let Some(channels) = crate::codec::common::filters::auto_delta_filter_channels(data) {
            candidates.push((
                Rar29FilterKind::Delta {
                    channels: channels as usize,
                },
                std::iter::once(0..data.len()).collect(),
            ));
        }
        if let Some(channels) = crate::codec::common::filters::auto_audio_filter_channels(data) {
            candidates.push((
                Rar29FilterKind::Audio { channels },
                std::iter::once(0..data.len()).collect(),
            ));
        }
        for (kind, ranges) in candidates {
            let Ok(candidate) = Unpack29Encoder::with_options(options)
                .encode_member_with_filter_ranges(data, kind, &ranges)
            else {
                continue;
            };
            if candidate.len() < best_len {
                best_len = candidate.len();
                best = (candidate, method);
            }
        }
    }
    if level >= 4
        && !data.is_empty()
        && let Ok(ppmd) = Unpack29Encoder::with_options(options).encode_ppmd_member(data)
        && ppmd.len() < best_len
    {
        best_len = ppmd.len();
        best = (ppmd, method);
    }
    if best_len < data.len() {
        Ok(Some(best))
    } else {
        Ok(None)
    }
}

/// Compress one RAR4 file member (non-solid, independent engine state)
/// without touching the archive stream: read + CRC + the smallest of
/// LZ / PPMd (m4+) / auto-filter candidates / STORE. Runs on the pool;
/// emission happens later on the writing thread.
#[cfg(feature = "parallel")]
pub(crate) fn prepare_rar4_file_member(
    path: &Path,
    name: &str,
    level: u8,
    unp_ver: u8,
) -> RarResult<Rar4PreparedMember> {
    let meta = fs::metadata(path)?;
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
    let mut reader = File::open(path)?;
    let mut data = Vec::with_capacity(file_size as usize);
    std::io::Read::read_to_end(&mut reader, &mut data)?;
    let file_crc = crate::crc32::crc32(&data);

    let (packed, method) = if (1..=5).contains(&level) && unp_ver != 29 {
        let packed = encode_legacy_codec_member(&data, level, unp_ver)?;
        if packed.len() < data.len() {
            (packed, crate::format::rar4::RAR4_METHOD_STORE + level)
        } else {
            (data, crate::format::rar4::RAR4_METHOD_STORE)
        }
    } else if (1..=5).contains(&level) {
        match best_rar29_member(&data, level)? {
            Some(best) => best,
            None => (data, crate::format::rar4::RAR4_METHOD_STORE),
        }
    } else {
        (data, crate::format::rar4::RAR4_METHOD_STORE)
    };
    Ok(Rar4PreparedMember {
        name: name.to_string(),
        mtime,
        mtime_ns,
        file_size,
        file_crc,
        packed,
        method,
    })
}

#[cfg(feature = "parallel")]
impl RarArchive {
    /// Emit a compressed RAR4 member prepared on a worker thread: member
    /// encryption, header + payload (single or multi-volume split), entry
    /// bookkeeping and progress. Mirrors `add_file_rar4`'s emission half
    /// for non-solid members (no FHD_SOLID continuation).
    fn emit_rar4_prepared(&mut self, prepared: Rar4PreparedMember) -> RarResult<()> {
        let Rar4PreparedMember {
            name,
            mtime,
            mtime_ns,
            file_size,
            file_crc,
            mut packed,
            method,
        } = prepared;
        let ext_time = crate::format::rar4::write::build_ext_time(Some(mtime_ns));

        let password_encrypted = self.password.as_deref().is_some_and(|pw| !pw.is_empty());
        let mut salt = None;
        if password_encrypted {
            salt = self.rar4_member_encrypt(&mut packed)?;
        }
        let packed_size = packed.len() as u64;
        let unpacked_size = file_size;

        let dos_time = crate::format::rar4::write::unix_to_dos_time(mtime);
        let (encoded_name, name_flags) = crate::format::rar4::write::encode_file_name(&name);

        match self.write_ctx().volume_size {
            None => {
                let (data_offset, _) = emit_rar4_segment(
                    self,
                    &encoded_name,
                    name_flags,
                    file_crc,
                    dos_time,
                    method,
                    packed_size as u32,
                    unpacked_size as u32,
                    &packed,
                    password_encrypted,
                    salt,
                    ext_time.as_deref(),
                    false,
                    false,
                    None,
                    false,
                    false,
                )?;
                self.push_rar4_entry(
                    name,
                    unpacked_size,
                    packed_size,
                    file_crc,
                    mtime,
                    mtime_ns,
                    method,
                    password_encrypted,
                    salt,
                    ext_time,
                    None,
                    false,
                    0,
                    vec![crate::model::DataChunk {
                        volume_index: 0,
                        data_offset,
                        packed_size,
                        crc32_val: Some(file_crc),
                        is_final: true,
                        extra_data: Vec::new(),
                    }],
                );
                self.report_progress(file_size, file_size);
                Ok(())
            }
            Some(volume_size) => {
                let chunks = {
                    let params = Rar4SplitParams {
                        encoded_name: &encoded_name,
                        name_flags,
                        file_crc,
                        dos_time,
                        method,
                        unpacked_size,
                        password: password_encrypted,
                        salt,
                        ext_time: ext_time.as_deref(),
                        solid_continuation: false,
                        is_dir: false,
                        comment: None,
                    };
                    emit_rar4_split(self, &params, volume_size, packed_size, |_, offset, len| {
                        Ok(Cow::Borrowed(
                            &packed[offset as usize..(offset + len) as usize],
                        ))
                    })?
                };
                self.push_rar4_entry(
                    name,
                    unpacked_size,
                    packed_size,
                    file_crc,
                    mtime,
                    mtime_ns,
                    method,
                    password_encrypted,
                    salt,
                    ext_time,
                    None,
                    false,
                    0,
                    chunks,
                );
                self.report_progress(file_size, file_size);
                Ok(())
            }
        }
    }
}

#[cfg(feature = "parallel")]
impl RarArchive {
    /// Parallel RAR4 batch: waves of independent non-solid file members are
    /// compressed on the pool and emitted in archive order (byte-identical
    /// to the sequential path). Solid runs, directories and oversized
    /// members fall back to the sequential path at their original position.
    pub(crate) fn add_batch_parallel_rar4(&mut self, entries: &[BatchEntry<'_>]) -> RarResult<()> {
        use rayon::prelude::*;
        self.progress_set_batch_total(entries)?;
        let mut i = 0usize;
        while i < entries.len() {
            self.check_cancel()?;
            let mut wave: Vec<(usize, BatchEntry<'_>)> = Vec::new();
            let mut wave_bytes = 0u64;
            while i < entries.len() {
                let size = match entries[i] {
                    BatchEntry::File { path, .. } => fs::metadata(path)
                        .ok()
                        .filter(|m| m.len() <= PARALLEL_COMPRESS_MAX_MEMBER)
                        .map(|m| m.len()),
                    _ => None,
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
                let threads = self.effective_threads();
                let pool = crate::parallel::compression_pool_for(threads);
                let unp_ver = self.write_ctx().rar4_unp_ver;
                let prepared: Vec<RarResult<(usize, Rar4PreparedMember)>> = pool.install(|| {
                    wave.par_iter()
                        .map(|&(idx, entry)| {
                            let BatchEntry::File { path, name, level } = entry else {
                                unreachable!("wave holds only file members")
                            };
                            let name = match name {
                                Some(name) => name.to_string(),
                                None => path
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .into_owned(),
                            };
                            prepare_rar4_file_member(path, &name, level, unp_ver).map(|p| (idx, p))
                        })
                        .collect()
                });
                self.check_cancel()?;
                let mut ordered = Vec::with_capacity(prepared.len());
                for result in prepared {
                    ordered.push(result?);
                }
                ordered.sort_by_key(|(idx, _)| *idx);
                for (idx, member) in ordered {
                    self.progress_member = idx;
                    self.emit_rar4_prepared(member)?;
                }
            }
            if i < entries.len() {
                self.progress_member = i;
                self.add_batch_entry_sequential(&entries[i])?;
                i += 1;
            }
        }
        Ok(())
    }
}
