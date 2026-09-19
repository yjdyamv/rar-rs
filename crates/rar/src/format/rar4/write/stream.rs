//! RAR4 bounded-memory member streaming: compress (or copy) a large member
//! into a spill file, then emit its header and stream the payload across
//! volumes, encrypting on the fly for `-p`.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use super::cbc::{Rar4RangeEmitter, Rar15RangeEmitter, Rar20RangeEmitter, Rar30RangeEmitter};
use super::emit::{Rar4PayloadSource, Rar4SplitParams, emit_rar4_segment, emit_rar4_split};
use super::encode::{legacy_rar15_options, legacy_rar20_options};
use super::member::{emit_pending_rar4_comment, push_rar4_entry, track_rar4_solid_member};
use crate::engine::Engine;
use crate::error::{RarError, RarResult};
use crate::format::shared::engine::{CountingWriter, CrcReader, SpillGuard, spill_path_for};
use crate::version::LegacyCodec;
/// Create one large RAR4 member with bounded memory: compress the source
/// into a spill file (falling back to STORE when compression does not
/// help), optionally encrypting the payload on the fly, then emit the
/// FILE_HEAD and stream the payload across volumes.
///
/// Only the RAR29 codec streams; the v15/v20 encoders need the whole
/// input and are handled by the buffered path in [`Self::add_file_rar4`].
/// Large members use the LZ engine only (the PPMd trial and the
/// automatic VM filters need whole-member buffers).
pub(super) fn add_rar4_file_streaming(
    cx: &mut dyn Engine,
    path: &Path,
    name: &str,
    file_size: u64,
    mtime: u32,
    mtime_ns: u32,
    level: u8,
) -> RarResult<()> {
    cx.check_cancel()?;
    crate::format::rar4::create::ensure_member_size(file_size)?;
    emit_pending_rar4_comment(cx)?;
    if cx.write_ctx().solid.mode {
        crate::format::shared::write_ops::maybe_reset_solid_for_extension(cx, name);
    }

    let password = cx.password().is_some_and(|pw| !pw.is_empty());
    let codec = LegacyCodec::from_unp_ver(cx.write_ctx().solid.rar4_unp_ver);
    let solid_mode = cx.write_ctx().solid.mode;
    let spill = spill_path_for(cx.path());
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
            cx.check_cancel()?;
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            copied += n as u64;
            cx.report_progress(copied, file_size);
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
            read: 0,
        };
        let mut spill_file = crate::fs::atomic::read_write_create(&spill)?;
        let mut counter = CountingWriter::new(&mut spill_file);
        // A RAR 1.5 solid run's encoder is advanced by the trial encode
        // and committed only when this member actually packs: the reader
        // skips a STORE member without touching its adaptive tables, and
        // a RAR 1.5 chain is position-derived, so a stored member must
        // leave the carried encoder exactly where it was.
        let mut legacy_encoder_to_commit: Option<
            crate::codec::legacy::rar15_encoder::Unpack15Encoder,
        > = None;
        {
            let codec = LegacyCodec::from_unp_ver(cx.write_ctx().solid.rar4_unp_ver);
            let progress = cx.progress_slot().map(|(p, _)| p);
            let cancel = cx.cancel_token();
            let member = cx.progress_slot().map(|(_, m)| m).unwrap_or(0);
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
            if codec == Some(LegacyCodec::Rar20) && !solid_mode {
                // RAR 2.x streams as a sequence of LZ blocks, one per window.
                crate::codec::legacy::rar20_encoder::encode_member_windowed_streaming(
                    &mut source,
                    &mut counter,
                    legacy_rar20_options(level),
                    crate::codec::legacy::rar20_encoder::RAR20_STREAM_WINDOW,
                    Some(&mut report),
                )?;
            } else if codec == Some(LegacyCodec::Rar15) {
                // RAR 1.5 is one adaptive stream over the whole member, so
                // it encodes incrementally: a rolling window plus a chunk,
                // with the bit stream continuing across chunk boundaries.
                // A solid run clones the carried encoder for the trial and
                // commits it after the size check below.
                use crate::codec::legacy::rar15_encoder::Unpack15Encoder;
                let mut encoder = if solid_mode {
                    match cx.write_ctx().solid.legacy_encoder.as_ref() {
                        Some(crate::engine::LegacySolidEncoder::Rar15(encoder)) => {
                            encoder.clone_for_trial()
                        }
                        _ => Unpack15Encoder::with_options(legacy_rar15_options(level)),
                    }
                } else {
                    Unpack15Encoder::with_options(legacy_rar15_options(level))
                };
                encoder.encode_member_streaming(&mut source, &mut counter, Some(&mut report))?;
                if solid_mode {
                    legacy_encoder_to_commit = Some(encoder);
                }
            } else {
                let options = crate::codec::legacy::rar29_encoder::options_for_level(level);
                if solid_mode {
                    let encoder = cx
                        .write_ctx_mut()
                        .solid
                        .rar4_encoder
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
        }
        let read = source.hasher.finalize();
        if source.read != file_size {
            return Err(RarError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "file changed size while being archived: expected {file_size} bytes, read {}",
                    source.read
                ),
            )));
        }
        file_crc = read;
        let compressed = counter.written();
        if compressed < file_size {
            packed_len = compressed;
            method = crate::format::rar4::RAR4_METHOD_STORE + level;
            if let Some(encoder) = legacy_encoder_to_commit.take() {
                cx.write_ctx_mut().solid.legacy_encoder =
                    Some(crate::engine::LegacySolidEncoder::Rar15(Box::new(encoder)));
            }
        } else {
            // Compression is a net loss: stream STORE from the source
            // instead (the spill is dropped by its guard). Any trial chain
            // state is dropped with it.
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
    // Each generation has its own cipher for the member payload, and only
    // RAR 3.x adds a salt: RAR 2.x and 3.x encrypt whole 16-byte blocks
    // (the final one zero-padded), RAR 1.5 XORs a keystream.
    let mut salt = None;
    let mut padded = false;
    let mut source = if password {
        let password_bytes = cx.password().expect("password checked above").as_bytes();
        let emitter: Box<dyn Rar4RangeEmitter> = match codec {
            Some(LegacyCodec::Rar29) => {
                let mut salt_bytes = [0u8; 8];
                rand::fill(&mut salt_bytes);
                salt = Some(salt_bytes);
                padded = true;
                let cipher = crate::crypto::Rar30Cipher::new(password_bytes, Some(salt_bytes))
                    .map_err(|e| RarError::Format(format!("RAR4 member key setup: {e:?}")))?;
                Box::new(Rar30RangeEmitter::new(cipher))
            }
            Some(LegacyCodec::Rar20) => {
                padded = true;
                Box::new(Rar20RangeEmitter::new(crate::crypto::Rar20Cipher::new(
                    password_bytes,
                )))
            }
            Some(LegacyCodec::Rar15) => Box::new(Rar15RangeEmitter::new(
                crate::crypto::Rar15Cipher::new(password_bytes),
            )),
            _ => {
                return Err(RarError::Unsupported(format!(
                    "RAR4 write encryption: unp_ver {} has no cipher",
                    cx.write_ctx().solid.rar4_unp_ver
                )));
            }
        };
        Rar4PayloadSource::Encrypted {
            file: File::open(&source_path)?,
            plain_len,
            emitter,
        }
    } else {
        Rar4PayloadSource::Plain {
            file: File::open(&source_path)?,
        }
    };
    let packed_size = if padded {
        plain_len.next_multiple_of(16)
    } else {
        plain_len
    };
    crate::format::rar4::create::ensure_member_size(packed_size)?;

    // Solid-chain bookkeeping mirrors the buffered path: STORE ends the
    // run (and drops the encoder state the trial may have advanced).
    let solid_continuation = track_rar4_solid_member(cx, method, file_size);

    let ext_time = crate::format::rar4::write::build_ext_time(mtime, Some(mtime_ns));
    let dos_time = crate::format::rar4::write::unix_to_dos_time(mtime);
    let (encoded_name, name_flags) = crate::format::rar4::write::encode_file_name(name);
    let unpacked_size = file_size;
    let mut chunks = Vec::<crate::model::DataChunk>::new();

    match cx.write_ctx().output.volume_size {
        None => {
            // Header first (packed size is known), then stream the
            // payload in bounded chunks.
            let (data_offset, _) = emit_rar4_segment(
                cx,
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
                0x20,
                None,
                false,
                false,
            )?;
            const COPY: u64 = 1 << 20;
            let mut pos = 0u64;
            while pos < packed_size {
                cx.check_cancel()?;
                let end = (pos + COPY).min(packed_size);
                let chunk = source.read_range(pos, end)?;
                cx.stream_mut()?.write_all(&chunk)?;
                cx.add_bytes_written(chunk.len() as u64);
                pos = end;
                cx.report_progress(pos, file_size);
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
                    attr: 0x20,
                    comment: None,
                };
                emit_rar4_split(
                    cx,
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

    push_rar4_entry(
        cx,
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
    cx.report_progress(file_size, file_size);
    Ok(())
}
