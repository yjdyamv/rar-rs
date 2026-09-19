//! RAR4 volume and segment emission: the per-segment FILE_HEAD/CRC envelope,
//! the volume-split budget arithmetic and the payload sources the split
//! drivers read from.
//!
//! The wire-level header builders live in the parent [`super`] module.

use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use super::cbc::Rar4RangeEmitter;
use crate::engine::Engine;
use crate::error::{RarError, RarResult};
use crate::version::LegacyCodec;
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
pub(super) fn emit_rar4_segment(
    this: &mut dyn Engine,
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
    attr: u32,
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
    // RAR 3.x/4.x stores a member comment as a standalone COMM_HEAD block
    // after the member's data (the layout official UnRAR accepts); pre-RAR3
    // archives nest it inside the FILE_HEAD behind `FHD_COMMENT`, their
    // historical layout.
    let standalone_comment = comment.is_some()
        && LegacyCodec::from_unp_ver(this.write_ctx().solid.rar4_unp_ver)
            == Some(LegacyCodec::Rar29);
    if comment.is_some() && !standalone_comment {
        fhd |= FHD_COMMENT;
    }
    let params = FileHeaderParams {
        flags: fhd,
        packed_size,
        unpacked_size,
        host_os: 2,
        file_crc,
        file_time: dos_time,
        unp_ver: this.write_ctx().solid.rar4_unp_ver,
        method,
        name: encoded_name,
        attr,
        salt,
        ext_time,
        window_bits: 6, // 4 MiB dictionary
    };
    let mut hdr = build_file_header(&params)?;
    // Append the per-file comment subblock (COMM_HEAD 0x75) after the
    // extended-time area and fix the outer head size + head CRC.
    if let Some(comment) = &comment
        && !standalone_comment
    {
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
    // `-hp`: the file-header block is header-encrypted like every
    // other block after the main header. The member payload (data)
    // itself is NOT part of the ciphertext; it follows the encrypted
    // header on disk and is covered by member-level encryption (`-p`)
    // separately. The data offset is past the `[8B salt][align16]`
    // block, matching the read side's `block.header_end`.
    let header_encryption = this.header_encryption();
    // The password is taken before the stream borrow: `this.password()` and
    // `this.stream_mut()` both borrow the whole engine.
    let password = header_encryption
        .then(|| this.password().map(str::to_owned))
        .flatten();
    let (header_bytes, header_on_disk) = if header_encryption {
        let password = password
            .as_deref()
            .ok_or_else(|| RarError::Encrypted("header encryption requires a password".into()))?;
        crate::format::rar4::write::encrypt_block_header(&hdr, password)?
    } else {
        (hdr.clone(), hdr.len() as u64)
    };
    let stream = this.stream_mut()?;
    let data_offset = stream.stream_position()? + header_on_disk;
    stream.write_all(&header_bytes)?;
    stream.write_all(data)?;
    let mut written = header_on_disk + data.len() as u64;
    // Standalone member comment (RAR 3.x/4.x): a COMM_HEAD block right
    // after the member data. The split path passes the comment to every
    // segment, so only the final one carries it. Under `-hp` the whole
    // block is header-encrypted (`HEAD_SIZE` spans its payload, matching
    // the reader's `align16(head_size)` ciphertext block).
    if let Some(comment) = &comment
        && standalone_comment
        && !split_after
    {
        let block = build_file_comment_block(comment);
        let block_bytes = if header_encryption {
            let password = password.as_deref().ok_or_else(|| {
                RarError::Encrypted("header encryption requires a password".into())
            })?;
            crate::format::rar4::write::encrypt_block_header(&block, password)?.0
        } else {
            block
        };
        stream.write_all(&block_bytes)?;
        written += block_bytes.len() as u64;
    }
    this.add_bytes_written(written);
    Ok((data_offset, data.len() as u64))
}

/// Packed payload of a streamed RAR4 member: the bytes live in a file (the
/// source for STORE, the compression spill otherwise) and are read on demand,
/// encrypting on the fly for `-p` members.
pub(super) enum Rar4PayloadSource {
    Plain {
        file: File,
    },
    Encrypted {
        file: File,
        plain_len: u64,
        emitter: Box<dyn Rar4RangeEmitter>,
    },
}

impl Rar4PayloadSource {
    /// Read the on-disk payload range `[start, end)`.
    pub(super) fn read_range(&mut self, start: u64, end: u64) -> RarResult<Vec<u8>> {
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
pub(super) struct Rar4SplitParams<'a> {
    pub(super) encoded_name: &'a [u8],
    pub(super) name_flags: u16,
    pub(super) file_crc: u32,
    pub(super) dos_time: u32,
    pub(super) method: u8,
    pub(super) unpacked_size: u64,
    pub(super) password: bool,
    pub(super) salt: Option<[u8; 8]>,
    pub(super) ext_time: Option<&'a [u8]>,
    pub(super) solid_continuation: bool,
    pub(super) attr: u32,
    pub(super) comment: Option<Vec<u8>>,
}

/// Split `packed_size` on-disk bytes across volumes: one FILE_HEAD plus one
/// segment per volume, using the RAR4 split convention (a non-final head
/// carries its own segment's CRC, the final head the whole-file CRC; the
/// unpacked size is the full file size in every head). `segment(offset, len)`
/// yields the segment's on-disk (already encrypted) bytes; the closure may
/// report progress itcx.
pub(super) fn emit_rar4_split<'a>(
    this: &mut dyn Engine,
    params: &Rar4SplitParams<'_>,
    volume_size: u64,
    packed_size: u64,
    mut segment: impl FnMut(&mut dyn Engine, u64, u64) -> RarResult<Cow<'a, [u8]>>,
) -> RarResult<Vec<crate::model::DataChunk>> {
    let needed = 7 + rar4_segment_header_reserve(
        params.encoded_name,
        params.salt.is_some(),
        params.ext_time,
        this.header_encryption(),
    );
    let mut chunks = Vec::new();
    let mut sent = 0u64;
    let mut vol_index = this.current_volume_index();
    let mut split_before = false;
    while sent < packed_size {
        // Roll to a volume with room for the header and the EOA.
        let mut rolled = false;
        loop {
            let used = this.bytes_written();
            if volume_size.saturating_sub(used) > needed {
                break;
            }
            if rolled {
                return Err(RarError::InvalidOption(format!(
                    "volume size {volume_size} is too small for a RAR4 member header"
                )));
            }
            this.start_next_volume()?;
            vol_index = this.current_volume_index();
            rolled = true;
        }
        let used = this.bytes_written();
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
            params.attr,
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
