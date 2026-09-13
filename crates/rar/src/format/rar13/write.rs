//! RAR 1.3/1.4 container write.
//!
//! Header layout and member emission follow `rars`' `rar13.rs` write half
//! (WTFPL; see NOTICE).
//!
//! Members are compressed with the `Unpack15` encoder under the RAR 1.4
//! token policy (old-distance tokens always considered, lazy matching only
//! at level 5), verified by decoding before they are accepted, and stored
//! when compression does not shrink them. A solid chain keeps one encoder
//! across members; the reference writer never stores inside a solid chain,
//! and neither does this writer.
//!
//! Multi-volume sets split members at exact volume boundaries: every
//! volume starts with the signature and a main header (`MHD_VOLUME`), and
//! a member spanning volumes repeats its file header per fragment with
//! `LHD_SPLIT_BEFORE`/`LHD_SPLIT_AFTER`. Intermediate fragments carry the
//! cumulative packed checksum (matching the historical STORE convention),
//! the final fragment the whole-member checksum; encrypted members restart
//! the RAR13 cipher at every fragment, like the reference decoder expects.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    DEFAULT_UNP_VER, LHD_COMMENT, LHD_PASSWORD, LHD_SOLID, LHD_SPLIT_AFTER, LHD_SPLIT_BEFORE,
    MAIN_HEAD_SIZE, METHOD_BEST, METHOD_STORE, MHD_ALWAYS_SET, MHD_COMMENT, MHD_PACK_COMMENT,
    MHD_SOLID, MHD_VOLUME,
};
use crate::archive::{ArchiveEntry, LegacySolidEncoder, RarArchive};
use crate::codec::legacy::rar15_encoder::{EncodeOptions, Unpack15Encoder};
use crate::error::{RarError, RarResult};
use crate::format::shared::stream_mut;
use crate::model::DataChunk;

/// The RAR 1.4 encoder policy for a compression level, mirroring the
/// reference writer: old-distance tokens are always in play, lazy matching
/// only at level 5, and levels 1–4 bound the long-match distance.
fn rar13_encode_options(level: u8) -> EncodeOptions {
    match level {
        0 => EncodeOptions::new()
            .with_lazy_matching(false)
            .with_stmode_literal_runs(false)
            .with_max_long_match_distance(0),
        1 => EncodeOptions::new()
            .with_lazy_matching(false)
            .with_stmode_literal_runs(false)
            .with_max_long_match_distance(4 * 1024),
        2 => EncodeOptions::new()
            .with_lazy_matching(false)
            .with_stmode_literal_runs(false)
            .with_max_long_match_distance(8 * 1024),
        3 => EncodeOptions::new()
            .with_lazy_matching(false)
            .with_max_long_match_distance(16 * 1024),
        4 => EncodeOptions::new()
            .with_lazy_matching(false)
            .with_max_long_match_distance(24 * 1024),
        _ => EncodeOptions::new(),
    }
}

/// Encode one standalone member, falling back through the reference
/// writer's option candidates. Returns `None` when no candidate both
/// encodes and round-trips.
fn encode_standalone_member(data: &[u8], level: u8) -> RarResult<Option<Vec<u8>>> {
    let options = rar13_encode_options(level);
    let mut candidates = vec![options, options.with_max_long_match_distance(24 * 1024)];
    let conservative = options
        .with_lazy_matching(false)
        .with_stmode_literal_runs(false)
        .with_max_long_match_distance(8 * 1024);
    if !candidates.contains(&conservative) {
        candidates.push(conservative);
    }
    for candidate in candidates {
        let packed =
            crate::codec::legacy::rar15_encoder::unpack15_encode_with_options(data, candidate)?;
        let decoded = crate::codec::legacy::rar15::Rar15Decoder::new().decode_member(
            &packed,
            data.len() as u64,
            false,
        );
        if decoded.is_ok_and(|decoded| decoded == data) {
            return Ok(Some(packed));
        }
    }
    Ok(None)
}

/// Encode the archive-comment extension: `Unpack15`-compressed, encrypted
/// with the fixed comment key, prefixed by the packed and unpacked lengths.
pub(crate) fn build_archive_comment(comment: Option<&[u8]>) -> RarResult<Vec<u8>> {
    let Some(comment) = comment else {
        return Ok(Vec::new());
    };
    if comment.len() > u16::MAX as usize {
        return Err(RarError::InvalidOption(
            "RAR 1.3/1.4 archive comment is longer than 65535 bytes".into(),
        ));
    }
    let mut packed = crate::codec::legacy::rar15_encoder::unpack15_encode(comment)?;
    crate::crypto::Rar13Cipher::new_comment().encrypt_in_place(&mut packed);
    let packed_field_len = packed
        .len()
        .checked_add(2)
        .ok_or_else(|| RarError::InvalidOption("RAR 1.3/1.4 comment size overflows".into()))?;
    if packed_field_len > u16::MAX as usize {
        return Err(RarError::InvalidOption(
            "RAR 1.3/1.4 packed archive comment is longer than 65535 bytes".into(),
        ));
    }
    let mut out = Vec::with_capacity(4 + packed.len());
    out.extend_from_slice(&(packed_field_len as u16).to_le_bytes());
    out.extend_from_slice(&(comment.len() as u16).to_le_bytes());
    out.extend_from_slice(&packed);
    Ok(out)
}

/// Build the 7-byte main header plus its comment extension. `volume` marks
/// every main header of a multi-volume set (only the first volume carries
/// the comment extension).
pub(crate) fn build_main_header(
    solid: bool,
    archive_comment: Option<&[u8]>,
    volume: bool,
) -> RarResult<Vec<u8>> {
    let comment_extra = build_archive_comment(archive_comment)?;
    let mut flags = MHD_ALWAYS_SET;
    if volume {
        flags |= MHD_VOLUME;
    }
    if archive_comment.is_some() {
        flags |= MHD_COMMENT | MHD_PACK_COMMENT;
    }
    if solid {
        flags |= MHD_SOLID;
    }
    let head_size = MAIN_HEAD_SIZE + comment_extra.len();
    if head_size > u16::MAX as usize {
        return Err(RarError::InvalidOption(
            "RAR 1.3/1.4 main header is longer than 65535 bytes".into(),
        ));
    }
    let mut out = Vec::with_capacity(head_size);
    out.extend_from_slice(crate::detect::RAR13_SIGNATURE);
    out.extend_from_slice(&(head_size as u16).to_le_bytes());
    out.push(flags);
    out.extend_from_slice(&comment_extra);
    Ok(out)
}

/// Build the member-comment header extension: a 16-bit length + the raw
/// comment bytes.
pub(crate) fn build_file_comment(comment: Option<&[u8]>) -> RarResult<Vec<u8>> {
    let Some(comment) = comment else {
        return Ok(Vec::new());
    };
    if comment.len() > u16::MAX as usize {
        return Err(RarError::InvalidOption(
            "RAR 1.3/1.4 member comment is longer than 65535 bytes".into(),
        ));
    }
    let mut out = Vec::with_capacity(2 + comment.len());
    out.extend_from_slice(&(comment.len() as u16).to_le_bytes());
    out.extend_from_slice(comment);
    Ok(out)
}

/// Everything a RAR 1.3/1.4 file header carries; bundled so the header
/// builder and the catalog writer do not repeat the same field clump.
struct MemberHeader<'a> {
    name: &'a str,
    packed_size: u64,
    unpacked_size: u64,
    file_crc: u16,
    file_time: u32,
    file_attr: u8,
    flags: u8,
    method: u8,
    extra: Vec<u8>,
}

/// Build a 21-byte-base file header plus its name and header extension.
fn build_file_header(header: &MemberHeader<'_>) -> RarResult<Vec<u8>> {
    let name_bytes = header.name.as_bytes();
    if name_bytes.len() > u8::MAX as usize {
        return Err(RarError::InvalidOption(format!(
            "RAR 1.3/1.4 member names are limited to 255 bytes (got {})",
            name_bytes.len()
        )));
    }
    let head_size = super::FILE_HEAD_BASE_SIZE + name_bytes.len() + header.extra.len();
    if head_size > u16::MAX as usize {
        return Err(RarError::InvalidOption(
            "RAR 1.3/1.4 file header is longer than 65535 bytes".into(),
        ));
    }
    let mut out = Vec::with_capacity(head_size);
    out.extend_from_slice(&(header.packed_size as u32).to_le_bytes());
    out.extend_from_slice(&(header.unpacked_size as u32).to_le_bytes());
    out.extend_from_slice(&header.file_crc.to_le_bytes());
    out.extend_from_slice(&(head_size as u16).to_le_bytes());
    out.extend_from_slice(&header.file_time.to_le_bytes());
    out.push(header.file_attr);
    out.push(header.flags);
    out.push(DEFAULT_UNP_VER);
    out.push(name_bytes.len() as u8);
    out.push(header.method);
    out.extend_from_slice(name_bytes);
    out.extend_from_slice(&header.extra);
    Ok(out)
}

impl RarArchive {
    /// Emit the deferred main header before the first member (or for an
    /// empty archive at close). The archive comment must be known here; the
    /// RAR4 writer-comment queue (`set_rar4_writer_comment`) feeds it.
    pub(crate) fn emit_rar13_main_header(&mut self) -> RarResult<()> {
        if !self.write_ctx().output.rar13_header_pending {
            return Ok(());
        }
        let comment = self.write_ctx_mut().rar4.writer_comment.take();
        let volume = self.write_ctx().output.volume_size.is_some();
        let header = build_main_header(self.write_ctx().solid.mode, comment.as_deref(), volume)?;
        {
            let stream = stream_mut(&mut self.stream)?;
            stream.seek(std::io::SeekFrom::Start(0))?;
            stream.write_all(&header)?;
        }
        let ctx = self.write_ctx_mut();
        ctx.output.rar13_header_pending = false;
        ctx.output.bytes_written = ctx.output.bytes_written.saturating_add(header.len() as u64);
        Ok(())
    }

    /// Write one RAR 1.3/1.4 member (header + payload). `comment` becomes
    /// the `LHD_COMMENT` header extension. RAR 1.3/1.4 has no timestamps
    /// beyond the DOS `mtime`, so `mtime_ns` is ignored.
    pub(crate) fn add_rar13_data(
        &mut self,
        name: String,
        data: Vec<u8>,
        level: u8,
        mtime: u32,
        _mtime_ns: u32,
        comment: Option<Vec<u8>>,
    ) -> RarResult<()> {
        self.check_cancel()?;
        super::create::ensure_member_size(data.len() as u64)?;
        self.emit_rar13_main_header()?;

        let unpacked = data.len() as u64;
        let is_directory = name.ends_with('/');
        let file_time = crate::format::rar4::write::unix_to_dos_time(mtime);
        if is_directory {
            let member = MemberHeader {
                name: &name,
                packed_size: 0,
                unpacked_size: 0,
                file_crc: 0,
                file_time,
                file_attr: 0x10,
                flags: 0,
                method: METHOD_STORE,
                extra: Vec::new(),
            };
            return self.write_rar13_member(&member, &[]);
        }

        let file_crc = super::file_checksum(&data);
        let (payload, method) = if level == 0 || data.is_empty() {
            (data, METHOD_STORE)
        } else if self.write_ctx().solid.mode {
            // One shared encoder per chain; a solid run never stores (the
            // reference writer keeps the compressed output even when it
            // grows slightly, so the decoder's window stays in sync).
            let encoder = self
                .write_ctx_mut()
                .solid
                .legacy_encoder
                .get_or_insert_with(|| {
                    LegacySolidEncoder::Rar15(Box::new(Unpack15Encoder::with_options(
                        rar13_encode_options(level),
                    )))
                });
            let packed = match encoder {
                LegacySolidEncoder::Rar15(encoder) => encoder.encode_member(&data)?,
                LegacySolidEncoder::Rar20(_) => {
                    return Err(RarError::InvalidState(
                        "RAR 1.3/1.4 solid chain uses the Unpack15 encoder".into(),
                    ));
                }
            };
            (packed, METHOD_BEST)
        } else {
            match encode_standalone_member(&data, level)? {
                Some(packed) if packed.len() < data.len() => (packed, METHOD_BEST),
                _ => (data, METHOD_STORE),
            }
        };

        let mut flags = 0u8;
        if self.write_ctx().solid.mode && method == METHOD_BEST {
            flags |= LHD_SOLID;
        }
        if comment.is_some() {
            flags |= LHD_COMMENT;
        }
        if self.password.is_some() {
            flags |= LHD_PASSWORD;
        }
        let extra = build_file_comment(comment.as_deref())?;
        let member = MemberHeader {
            name: &name,
            packed_size: payload.len() as u64,
            unpacked_size: unpacked,
            file_crc,
            file_time,
            file_attr: 0x20,
            flags,
            method,
            extra,
        };
        self.write_rar13_member(&member, &payload)?;
        self.report_progress(unpacked, unpacked);
        Ok(())
    }

    /// Emit one member: a single fragment when the archive is single-volume,
    /// otherwise the volume split driver. Encrypted payloads are encrypted
    /// whole before the split (the RAR13 cipher is one stream over the
    /// member's packed data; volume fragments continue it).
    fn write_rar13_member(&mut self, member: &MemberHeader<'_>, payload: &[u8]) -> RarResult<()> {
        let mut data = payload.to_vec();
        if member.flags & LHD_PASSWORD != 0 {
            let password = self.password.as_deref().ok_or_else(|| {
                RarError::Encrypted("encrypted member, no password provided".into())
            })?;
            crate::crypto::Rar13Cipher::new(password.as_bytes()).encrypt_in_place(&mut data);
        }
        match self.write_ctx().output.volume_size {
            Some(volume_size) => self.write_rar13_split_member(member, &data, volume_size),
            None => {
                let header = build_file_header(member)?;
                let (volume_index, data_offset) = self.write_rar13_bytes(&header, &data)?;
                self.push_rar13_entry(
                    member,
                    vec![DataChunk {
                        volume_index,
                        data_offset,
                        packed_size: data.len() as u64,
                        crc32_val: None,
                        is_final: true,
                        extra_data: Vec::new(),
                    }],
                )
            }
        }
    }

    /// Split a member across a volume set. The first fragment repeats the
    /// caller's header fields (comment extension included); continuation
    /// fragments carry `LHD_SPLIT_BEFORE` and no comment. `packed` holds the
    /// member's final on-disk bytes; intermediate fragments store the
    /// cumulative packed checksum, the final fragment the whole-member
    /// checksum.
    fn write_rar13_split_member(
        &mut self,
        member: &MemberHeader<'_>,
        packed: &[u8],
        volume_size: u64,
    ) -> RarResult<()> {
        let continuation = MemberHeader {
            name: member.name,
            packed_size: 0,
            unpacked_size: member.unpacked_size,
            file_crc: member.file_crc,
            file_time: member.file_time,
            file_attr: member.file_attr,
            flags: (member.flags | LHD_SPLIT_BEFORE) & !LHD_COMMENT,
            method: member.method,
            extra: Vec::new(),
        };
        let continuation_len = build_file_header(&continuation)?.len() as u64;
        let first_len = build_file_header(member)?.len() as u64;
        let total = packed.len() as u64;

        if total == 0 {
            // Header-only member (directories, empty files): it moves to the
            // next volume when the header no longer fits.
            let mut rolled = false;
            loop {
                let used = self.write_ctx().output.bytes_written;
                if volume_size.saturating_sub(used) >= first_len {
                    break;
                }
                if rolled {
                    return Err(RarError::InvalidOption(format!(
                        "volume size {volume_size} is too small for a RAR 1.3/1.4 member header"
                    )));
                }
                self.start_next_volume_rar13()?;
                rolled = true;
            }
            let header = build_file_header(member)?;
            let (volume_index, data_offset) = self.write_rar13_bytes(&header, &[])?;
            return self.push_rar13_entry(
                member,
                vec![DataChunk {
                    volume_index,
                    data_offset,
                    packed_size: 0,
                    crc32_val: None,
                    is_final: true,
                    extra_data: Vec::new(),
                }],
            );
        }

        let mut chunks = Vec::new();
        let mut sent = 0u64;
        let mut running: u16 = 0;
        let mut split_before = false;
        while sent < total {
            let header_len = if split_before {
                continuation_len
            } else {
                first_len
            };
            let mut rolled = false;
            loop {
                let used = self.write_ctx().output.bytes_written;
                if volume_size.saturating_sub(used) > header_len {
                    break;
                }
                if rolled {
                    return Err(RarError::InvalidOption(format!(
                        "volume size {volume_size} is too small for a RAR 1.3/1.4 member header"
                    )));
                }
                self.start_next_volume_rar13()?;
                rolled = true;
            }
            let used = self.write_ctx().output.bytes_written;
            let available = volume_size - used - header_len;
            let chunk_len = (total - sent).min(available);
            let split_after = sent + chunk_len < total;
            let chunk = &packed[sent as usize..(sent + chunk_len) as usize];
            for &byte in chunk {
                running = running.wrapping_add(u16::from(byte)).rotate_left(1);
            }
            let split_flags = match (split_before, split_after) {
                (true, true) => LHD_SPLIT_BEFORE | LHD_SPLIT_AFTER,
                (true, false) => LHD_SPLIT_BEFORE,
                (false, true) => LHD_SPLIT_AFTER,
                (false, false) => 0,
            };
            let fragment = MemberHeader {
                name: member.name,
                packed_size: chunk_len,
                unpacked_size: member.unpacked_size,
                file_crc: if split_after {
                    running
                } else {
                    member.file_crc
                },
                file_time: member.file_time,
                file_attr: member.file_attr,
                flags: (member.flags & !LHD_COMMENT) | split_flags,
                method: member.method,
                extra: if split_before {
                    Vec::new()
                } else {
                    member.extra.clone()
                },
            };
            let header = build_file_header(&fragment)?;
            let (volume_index, data_offset) = self.write_rar13_bytes(&header, chunk)?;
            chunks.push(DataChunk {
                volume_index,
                data_offset,
                packed_size: chunk_len,
                crc32_val: None,
                is_final: !split_after,
                extra_data: Vec::new(),
            });
            sent += chunk_len;
            split_before = true;
        }
        self.push_rar13_entry(member, chunks)
    }

    /// Append `header + payload` to the current volume and report the data
    /// position within the set (0-based volume index and offset).
    fn write_rar13_bytes(&mut self, header: &[u8], payload: &[u8]) -> RarResult<(usize, u64)> {
        let volume_index = self.write_ctx().output.current_volume.saturating_sub(1);
        let data_offset = {
            let stream = stream_mut(&mut self.stream)?;
            stream.write_all(header)?;
            let offset = stream.stream_position()?;
            stream.write_all(payload)?;
            offset
        };
        let ctx = self.write_ctx_mut();
        ctx.output.bytes_written = ctx
            .output
            .bytes_written
            .saturating_add(header.len() as u64)
            .saturating_add(payload.len() as u64);
        Ok((volume_index, data_offset))
    }

    /// Catalog one emitted member (all of its volume fragments).
    fn push_rar13_entry(
        &mut self,
        member: &MemberHeader<'_>,
        chunks: Vec<DataChunk>,
    ) -> RarResult<()> {
        let data_offset = chunks.first().map_or(0, |chunk| chunk.data_offset);
        let packed_size = chunks.iter().map(|chunk| chunk.packed_size).sum();
        self.entries.push(ArchiveEntry {
            header: crate::model::FileHeader {
                name: member.name.to_string(),
                unpacked_size: member.unpacked_size,
                packed_size,
                crc32_val: Some(u32::from(member.file_crc)),
                comp_method: member.method,
                is_directory: member.name.ends_with('/'),
                data_offset,
                format_version: 3,
                unp_ver: 15,
                ..Default::default()
            },
            chunks,
        });
        Ok(())
    }

    /// RAR 1.3/1.4 filesystem-file member writer.
    pub(crate) fn add_file_rar13(
        &mut self,
        path: &Path,
        arcname: Option<&str>,
        level: u8,
    ) -> RarResult<()> {
        self.check_cancel()?;
        let meta = fs::metadata(path)?;
        if !meta.is_file() {
            return Err(RarError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("not a file: {}", path.display()),
            )));
        }
        let file_size = meta.len();
        super::create::ensure_member_size(file_size)?;
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;
        let name = match arcname {
            Some(name) => name.to_string(),
            None => crate::format::shared::write_ops::archive_name_from_path(path)?,
        };
        let name = name.replace('\\', "/");
        let data = fs::read(path)?;
        self.add_rar13_data(name, data, level, mtime, 0, None)
    }

    /// Write a zero-byte directory entry (no recursion).
    pub(crate) fn write_rar13_dir_entry(
        &mut self,
        name: &str,
        mtime: u32,
        mtime_ns: u32,
    ) -> RarResult<()> {
        let mut name = name.trim_end_matches('/').to_string();
        name.push('/');
        self.add_rar13_data(name, Vec::new(), 0, mtime, mtime_ns, None)
    }
}
