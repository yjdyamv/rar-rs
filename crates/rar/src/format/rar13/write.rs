//! RAR 1.3/1.4 container write (single volume).
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

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    DEFAULT_UNP_VER, LHD_COMMENT, LHD_PASSWORD, LHD_SOLID, MAIN_HEAD_SIZE, METHOD_BEST,
    METHOD_STORE, MHD_ALWAYS_SET, MHD_COMMENT, MHD_PACK_COMMENT, MHD_SOLID,
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

/// Build the 7-byte main header plus its comment extension.
pub(crate) fn build_main_header(solid: bool, archive_comment: Option<&[u8]>) -> RarResult<Vec<u8>> {
    let comment_extra = build_archive_comment(archive_comment)?;
    let mut flags = MHD_ALWAYS_SET;
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

/// Build a 21-byte-base file header plus its name and header extension.
#[allow(clippy::too_many_arguments)]
fn build_file_header(
    name: &str,
    packed_size: u64,
    unpacked_size: u64,
    file_crc: u16,
    file_time: u32,
    file_attr: u8,
    flags: u8,
    method: u8,
    extra: &[u8],
) -> RarResult<Vec<u8>> {
    let name_bytes = name.as_bytes();
    if name_bytes.len() > u8::MAX as usize {
        return Err(RarError::InvalidOption(format!(
            "RAR 1.3/1.4 member names are limited to 255 bytes (got {})",
            name_bytes.len()
        )));
    }
    let head_size = super::FILE_HEAD_BASE_SIZE + name_bytes.len() + extra.len();
    if head_size > u16::MAX as usize {
        return Err(RarError::InvalidOption(
            "RAR 1.3/1.4 file header is longer than 65535 bytes".into(),
        ));
    }
    let mut out = Vec::with_capacity(head_size);
    out.extend_from_slice(&(packed_size as u32).to_le_bytes());
    out.extend_from_slice(&(unpacked_size as u32).to_le_bytes());
    out.extend_from_slice(&file_crc.to_le_bytes());
    out.extend_from_slice(&(head_size as u16).to_le_bytes());
    out.extend_from_slice(&file_time.to_le_bytes());
    out.push(file_attr);
    out.push(flags);
    out.push(DEFAULT_UNP_VER);
    out.push(name_bytes.len() as u8);
    out.push(method);
    out.extend_from_slice(name_bytes);
    out.extend_from_slice(extra);
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
        let header = build_main_header(self.write_ctx().solid.mode, comment.as_deref())?;
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
            let header = build_file_header(&name, 0, 0, 0, file_time, 0x10, 0, METHOD_STORE, &[])?;
            return self.write_rar13_header_and_payload(&header, &[], &name, 0, 0, METHOD_STORE);
        }

        let file_crc = super::file_checksum(&data);
        let (mut payload, method) = if level == 0 || data.is_empty() {
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
        let extra = build_file_comment(comment.as_deref())?;
        if let Some(password) = self.password.as_deref() {
            crate::crypto::Rar13Cipher::new(password.as_bytes()).encrypt_in_place(&mut payload);
            flags |= LHD_PASSWORD;
        }
        let header = build_file_header(
            &name,
            payload.len() as u64,
            unpacked,
            file_crc,
            file_time,
            0x20,
            flags,
            method,
            &extra,
        )?;
        self.write_rar13_header_and_payload(&header, &payload, &name, unpacked, file_crc, method)?;
        self.report_progress(unpacked, unpacked);
        Ok(())
    }

    fn write_rar13_header_and_payload(
        &mut self,
        header: &[u8],
        payload: &[u8],
        name: &str,
        unpacked_size: u64,
        file_crc: u16,
        method: u8,
    ) -> RarResult<()> {
        let data_offset = {
            let stream = stream_mut(&mut self.stream)?;
            stream.write_all(header)?;
            let offset = stream.stream_position()?;
            stream.write_all(payload)?;
            offset
        };
        self.entries.push(ArchiveEntry {
            header: crate::model::FileHeader {
                name: name.to_string(),
                unpacked_size,
                packed_size: payload.len() as u64,
                crc32_val: Some(u32::from(file_crc)),
                comp_method: method,
                is_directory: name.ends_with('/'),
                data_offset,
                format_version: 3,
                unp_ver: 15,
                ..Default::default()
            },
            chunks: vec![DataChunk {
                volume_index: 0,
                data_offset,
                packed_size: payload.len() as u64,
                crc32_val: None,
                is_final: true,
                extra_data: Vec::new(),
            }],
        });
        let ctx = self.write_ctx_mut();
        ctx.output.bytes_written = ctx
            .output
            .bytes_written
            .saturating_add(header.len() as u64)
            .saturating_add(payload.len() as u64);
        Ok(())
    }

    /// RAR 1.3/1.4 filesystem-file member writer (single volume).
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
