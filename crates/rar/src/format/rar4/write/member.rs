//! RAR4 member entry points: the filesystem-file, in-memory and directory
//! writers, the queued archive comment, the solid-chain bookkeeping and the
//! catalog entry pushed for every emitted member.
//!
//! Reached from the format-neutral dispatchers in
//! `crate::format::shared::write_ops`; compression lives in [`super::encode`],
//! emission in [`super::emit`] and the large-member path in [`super::stream`].

use std::borrow::Cow;
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::emit::{Rar4SplitParams, emit_rar4_segment, emit_rar4_split};
use super::encode::{encode_rar4_member, rar4_member_encrypt};
use super::stream::add_rar4_file_streaming;
use crate::engine::{ArchiveEntry, Engine, STREAM_COMPRESS_THRESHOLD};
use crate::error::{RarError, RarResult};
use crate::format::shared::write_ops::archive_name_from_path;
use crate::model::FileHeader;
use crate::version::LegacyCodec;
/// RAR4 solid-chain bookkeeping for one member: returns whether the
/// member continues the run and advances the run state. A stored member
/// ends the run (dropping the carried encoder state the trial may have
/// advanced); a non-empty compressed member marks the run as started.
///
/// RAR 1.5 (unp_ver 15) never flags `FHD_SOLID`: the reader derives
/// solid continuation from the archive-level `MHD_SOLID` and member
/// position instead. RAR 2.x (unp_ver 20/26) flags each continuation
/// exactly like RAR3+ — official UnRAR feeds `Arc.FileHead.Solid` to
/// `Unpack20` for those versions and resets its tables when the flag is
/// clear.
pub(super) fn track_rar4_solid_member(cx: &mut dyn Engine, method: u8, unpacked_size: u64) -> bool {
    let codec = LegacyCodec::from_unp_ver(cx.write_ctx().solid.rar4_unp_ver);
    let continuation = cx.write_ctx().solid.mode
        && method != crate::format::rar4::RAR4_METHOD_STORE
        && cx.write_ctx().solid.rar4_run_has_member
        && matches!(codec, Some(LegacyCodec::Rar20 | LegacyCodec::Rar29));
    if method == crate::format::rar4::RAR4_METHOD_STORE {
        // RAR3+ flags the break with FHD_SOLID, so a STORE member ends
        // the run. RAR 1.5 chains are position-derived and carry no
        // flag: the reader keeps the window across STORE members, so
        // the writer must keep the encoder alive for the next
        // compressed member. RAR 2.x also keeps the encoder alive
        // (official UnRAR skips `Unpack20` for STORE members and keeps
        // `TablesRead2`); only RAR3+'s flagged break resets it here.
        if codec == Some(LegacyCodec::Rar29) {
            cx.write_ctx_mut().solid.rar4_encoder = None;
            cx.write_ctx_mut().solid.legacy_encoder = None;
            cx.write_ctx_mut().solid.rar4_run_has_member = false;
        }
    } else if unpacked_size != 0 {
        cx.write_ctx_mut().solid.rar4_run_has_member = true;
    }
    continuation
}

/// Push the catalog entry for one emitted RAR4 member. Shared by the
/// buffered, streaming and parallel emission paths so the header fields
/// (including the nanosecond mtime) stay in lockstep.
#[allow(clippy::too_many_arguments)]
pub(super) fn push_rar4_entry(
    cx: &mut dyn Engine,
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
    attr: u32,
    data_offset: u64,
    chunks: Vec<crate::model::DataChunk>,
) {
    let unp_ver = cx.write_ctx().solid.rar4_unp_ver;
    cx.push_entry(crate::engine::ArchiveEntry {
        header: crate::model::FileHeader {
            name,
            unpacked_size,
            packed_size,
            crc32_val: Some(file_crc),
            mtime,
            mtime_ns: Some(mtime_ns),
            comp_method: method.wrapping_sub(crate::format::rar4::RAR4_METHOD_STORE),
            host_os: 2,
            format_version: 4,
            unp_ver,
            data_offset,
            is_directory: is_dir,
            attributes: u64::from(attr),
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
    cx: &mut dyn Engine,
    path: &Path,
    arcname: Option<&str>,
    level: u8,
) -> RarResult<()> {
    let meta = fs::metadata(path)?;
    let file_size = meta.len();
    // RAR4 stores the file's DOS attributes (read-only/hidden/system), like
    // WinRAR; the model entry carries the same byte so a later repack keeps it.
    let attr = crate::platform::rar4_file_attributes(&meta);
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

    if cx.progress_slot().is_some() {
        cx.report_progress(0, file_size);
    }

    let mtime_ns = meta
        .modified()
        .unwrap_or(SystemTime::now())
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();

    // Large members stream: the file is compressed (or copied) into a
    // spill file and then streamed into the archive, so the whole member
    // never enters memory. RAR29 streams with its LZ engine, RAR 2.x with
    // its windowed multi-block encoder, and RAR 1.5 (plus any member whose
    // streaming codec cannot compress here) as STORE — bounded memory, at
    // the cost of compression for the STORE cases. Each generation's own
    // cipher is emitted over the streamed payload, so a password no longer
    // forces the member into memory either. A deferred solid append still
    // buffers (close() repacks the archive).
    let streaming_codec = LegacyCodec::from_unp_ver(cx.write_ctx().solid.rar4_unp_ver);
    let password_encrypted = cx.password().is_some_and(|pw| !pw.is_empty());
    let solid_mode = cx.write_ctx().solid.mode;
    // A version whose cipher this pipeline does not implement keeps the
    // buffered path, where it reports the unsupported version.
    let streamable = !password_encrypted
        || matches!(
            streaming_codec,
            Some(LegacyCodec::Rar15 | LegacyCodec::Rar20 | LegacyCodec::Rar29)
        );
    if file_size >= STREAM_COMPRESS_THRESHOLD && !cx.write_ctx().rar4.solid_append && streamable {
        // A windowed member the sample probe calls incompressible would
        // emit literals for minutes only for the pipeline to fall back to
        // STORE, so probe first (the buffered path does the same through
        // `whole_member_is_incompressible`).
        let compressible = match streaming_codec {
            // RAR 3.x streams with its LZ engine (solid chains included).
            Some(LegacyCodec::Rar29) => true,
            // RAR 2.x streams as a sequence of LZ blocks. Its solid chains
            // have no streaming form yet, so those members stream STORE
            // instead (bounded memory, no ratio).
            Some(LegacyCodec::Rar20) if !solid_mode => {
                !crate::codec::common::incompressible::sample_is_incompressible_stream(
                    &mut fs::File::open(path)?,
                    file_size,
                    level,
                )?
            }
            // RAR 1.5's adaptive stream now encodes incrementally, so its
            // large members compress in bounded memory too, solid chain
            // included.
            Some(LegacyCodec::Rar15) => {
                !crate::codec::common::incompressible::sample_is_incompressible_stream(
                    &mut fs::File::open(path)?,
                    file_size,
                    level,
                )?
            }
            // Anything without a streaming encoder.
            _ => false,
        };
        let stream_level = if compressible { level } else { 0 };
        return add_rar4_file_streaming(
            cx,
            path,
            &name,
            file_size,
            mtime,
            mtime_ns,
            stream_level,
            attr,
        );
    }

    // Read the whole member, then (for level >= 1) LZSS-compress it.
    let mut reader = File::open(path)?;
    let mut data = Vec::with_capacity(file_size as usize);
    std::io::Read::read_to_end(&mut reader, &mut data)?;
    if data.len() as u64 != file_size {
        return Err(RarError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "file changed size while being archived: expected {file_size} bytes, read {}",
                data.len()
            ),
        )));
    }
    add_rar4_data(cx, name, data, level, mtime, mtime_ns, None, Some(attr))
}

/// Emit a queued RAR4 archive comment (`rar4_writer_comment`) as a
/// NEWSUB `CMT` block at the current stream position, then clear the
/// queue. Only the 35-byte CMT header is header-encrypted under `-hp`;
/// the comment payload follows as plaintext data (the same rule as
/// FILE members).
pub(super) fn emit_pending_rar4_comment(cx: &mut dyn Engine) -> RarResult<()> {
    let Some(text) = cx.write_ctx_mut().rar4.writer_comment.take() else {
        return Ok(());
    };
    if text.is_empty() {
        return Ok(());
    }
    const CMT_HEAD: usize = crate::format::rar4::comment::CMT_HEAD_SIZE;
    let (payload, unicode) = crate::format::rar4::comment::encode_comment_text(&text);
    let block = crate::format::rar4::comment::build_comment_block(&payload, unicode);
    let header_encryption = cx.header_encryption();
    // The password is taken before the stream borrow: `cx.password()` and
    // `cx.stream_mut()` both borrow the whole engine.
    let password = header_encryption
        .then(|| cx.password().map(str::to_owned))
        .flatten();
    let stream = cx.stream_mut()?;
    if header_encryption {
        let password = password
            .as_deref()
            .ok_or_else(|| RarError::Encrypted("header encryption requires a password".into()))?;
        let (ciphertext, on_disk) =
            crate::format::rar4::write::encrypt_block_header(&block[..CMT_HEAD], password)?;
        stream.write_all(&ciphertext)?;
        stream.write_all(&block[CMT_HEAD..])?;
        cx.add_bytes_written(on_disk + (block.len() - CMT_HEAD) as u64);
    } else {
        stream.write_all(&block)?;
        cx.add_bytes_written(block.len() as u64);
    }
    Ok(())
}

/// Queue the archive comment for a RAR4 create/repack writer (emitted
/// before the first member).
pub(crate) fn set_rar4_writer_comment(cx: &mut dyn Engine, text: Option<Vec<u8>>) {
    cx.write_ctx_mut().rar4.writer_comment = text;
}

/// Encode one RAR4 member from in-memory bytes: CRC, then the smallest
/// of LZ / PPMd (m4+) / auto-filter candidates / STORE, per-member
/// encryption, and the FILE_HEAD + payload emission (single-volume or
/// split across volumes). Shared by the file path (`add_file_rar4`,
/// which reads the member first) and the bytes path (`add_bytes`).
#[allow(clippy::too_many_arguments)] // one member's full descriptor
pub(crate) fn add_rar4_data(
    cx: &mut dyn Engine,
    name: String,
    data: Vec<u8>,
    level: u8,
    mtime: u32,
    mtime_ns: u32,
    comment: Option<Vec<u8>>,
    attr: Option<u32>,
) -> RarResult<()> {
    cx.check_cancel()?;
    crate::format::rar4::create::ensure_member_size(data.len() as u64)?;
    // Deferred solid-append: the member cannot be streamed after an
    // existing solid chain; buffer it and let close() repack the whole
    // archive (surviving members + these additions).
    if cx.write_ctx().rar4.solid_append {
        cx.write_ctx_mut()
            .rar4
            .solid_append_entries
            .push(crate::engine::SolidAppendEntry {
                name,
                data,
                level,
                mtime,
                mtime_ns,
                attr: attr.unwrap_or(0x20),
            });
        return Ok(());
    }
    // A queued archive comment is emitted right before the first member
    // (it must precede every member; the queue is consumed once).
    emit_pending_rar4_comment(cx)?;
    // A directory member is written as a zero-byte placeholder whose name
    // ends in `/`; its on-disk attribute is the directory bit (0x10)
    // rather than the regular-file archive bit (0x20). Callers that
    // rebuild a foreign archive pass the original attribute byte.
    let is_dir = name.ends_with('/');
    let attr = attr.unwrap_or(if is_dir { 0x10 } else { 0x20 });
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
    if cx.write_ctx().solid.mode {
        crate::format::shared::write_ops::maybe_reset_solid_for_extension(cx, &name);
    }
    let (mut packed, method) =
        if crate::format::shared::write_ops::whole_member_is_incompressible(&data, level) {
            // Random-data members would only build an O(input) token
            // vector: store them (the RAR5 path stores them too).
            (data, crate::format::rar4::RAR4_METHOD_STORE)
        } else {
            encode_rar4_member(cx, &data, level)?
        };
    let unpacked_size = file_size;

    // Solid-chain bookkeeping (mirrors rars' `solid_run_has_member`
    // logic): a member is a chain continuation when it compresses and the
    // run has already emitted a member; storing a member rebuilds the
    // encoder and ends the run. The reader keeps its window/tables across
    // members flagged `FHD_SOLID`, so the flags and the encoder must stay
    // in lockstep.
    let solid_continuation = track_rar4_solid_member(cx, method, unpacked_size);

    let ext_time = crate::format::rar4::write::build_member_ext_time(
        cx.write_ctx().solid.rar4_unp_ver,
        mtime,
        Some(mtime_ns),
    );

    // Member-level encryption (WinRAR `-p`), dispatched on the cipher
    // generation (see `rar4_member_encrypt`). The header carries the
    // RAR29 salt (`FHD_SALT`); the old codecs encrypt without one but
    // still flag `FHD_PASSWORD`. `packed_size` covers the padded
    // ciphertext; the header CRC stays the plaintext CRC and is checked
    // after decryption.
    let password_encrypted = cx.password().is_some_and(|pw| !pw.is_empty());
    let mut salt = None;
    if password_encrypted {
        salt = rar4_member_encrypt(cx, &mut packed)?;
    }
    let packed_size = packed.len() as u64;

    let dos_time = crate::format::rar4::write::unix_to_dos_time(mtime);
    let (encoded_name, name_flags) = crate::format::rar4::write::encode_file_name(&name);

    match cx.write_ctx().output.volume_size {
        None => {
            // ── Single-volume ──
            let (data_offset, _) = emit_rar4_segment(
                cx,
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
                attr,
                comment.clone(),
                false,
                false,
            )?;
            push_rar4_entry(
                cx,
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
                attr,
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
            cx.report_progress(file_size, file_size);
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
                    attr,
                    comment: comment.clone(),
                };
                emit_rar4_split(cx, &params, volume_size, packed_size, |_, offset, len| {
                    Ok(Cow::Borrowed(
                        &packed[offset as usize..(offset + len) as usize],
                    ))
                })?
            };
            push_rar4_entry(
                cx,
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
                attr,
                0,
                chunks,
            );
            cx.report_progress(file_size, file_size);
            Ok(())
        }
    }
}

/// Write one RAR4 directory FILE_HEAD member (WinRAR convention: zero
/// packed/unpacked sizes, CRC 0, the directory's DOS attribute byte (at least
/// `0x10`), `unp_ver 20`, name without a trailing slash; directories carry no
/// data payload).
pub(crate) fn write_rar4_dir_entry(
    cx: &mut dyn Engine,
    name: &str,
    meta: &fs::Metadata,
    mtime_secs: u32,
    mtime_ns: u32,
) -> RarResult<()> {
    use crate::format::rar4::write::{
        FileHeaderParams, build_file_header, build_member_ext_time, encode_file_name,
        unix_to_dos_time,
    };
    // A queued archive comment must precede the first member, whichever
    // kind it is; directories reached before any file flush it here.
    emit_pending_rar4_comment(cx)?;
    let (encoded_name, name_flags) = encode_file_name(name);
    let ext_time = build_member_ext_time(
        cx.write_ctx().solid.rar4_unp_ver,
        mtime_secs,
        Some(mtime_ns),
    );
    let mut flags = name_flags;
    if ext_time.is_some() {
        flags |= crate::format::rar4::FHD_EXTTIME;
    }
    // The directory's own DOS attributes (directory bit plus hidden/system),
    // like WinRAR; the model entry carries the same byte.
    let attr = crate::platform::rar4_dir_attributes(meta);
    let params = FileHeaderParams {
        flags,
        packed_size: 0,
        unpacked_size: 0,
        host_os: 2,
        file_crc: 0,
        file_time: unix_to_dos_time(mtime_secs),
        unp_ver: 20,
        method: crate::format::rar4::RAR4_METHOD_STORE,
        name: &encoded_name,
        attr,
        // All window bits set: the RAR4 directory marker that UnRAR and
        // WinRAR use to classify a member as a directory (files carry a
        // 0..=6 dictionary-size value instead).
        window_bits: 7,
        salt: None,
        ext_time: ext_time.as_deref(),
    };
    let hdr = build_file_header(&params)?;
    // Multi-volume: roll to a volume with room for this head plus the
    // volume-set end-of-archive block (same rule as file members). A volume
    // too small for even a fresh header must error instead of rolling
    // forever.
    if let Some(volume_size) = cx.write_ctx().output.volume_size {
        let endarc = crate::format::rar4::write::endarc_volume_reserve(cx.header_encryption());
        let mut rolled = false;
        loop {
            let used = cx.bytes_written();
            // The directory header is what the record will protect.
            let prefix = used + hdr.len() as u64;
            if volume_size.saturating_sub(used)
                > endarc + hdr.len() as u64 + cx.recovery_volume_reserve(prefix)
            {
                break;
            }
            if rolled {
                return Err(RarError::InvalidOption(format!(
                    "volume size {volume_size} is too small for a RAR4 directory header"
                )));
            }
            cx.start_next_volume()?;
            rolled = true;
        }
    }
    let stream = cx.stream_mut()?;
    stream.write_all(&hdr)?;
    cx.add_bytes_written(hdr.len() as u64);
    let head_crc = u16::from_le_bytes([hdr[0], hdr[1]]);
    cx.push_entry(ArchiveEntry {
        header: FileHeader {
            name: name.to_string(),
            unpacked_size: 0,
            packed_size: 0,
            attributes: u64::from(attr),
            mtime: mtime_secs,
            mtime_ns: ext_time.is_some().then_some(mtime_ns),
            crc32_val: Some(0),
            comp_method: 0,
            host_os: 2,
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
