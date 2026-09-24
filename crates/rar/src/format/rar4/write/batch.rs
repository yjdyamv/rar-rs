//! RAR4 parallel batch compression (feature `parallel`): independent non-solid
//! members are compressed on the pool, then emitted in archive order on the
//! writing thread. Byte-identical to the sequential path (each member uses a
//! fresh engine, exactly like `add_file_rar4` non-solid).

use std::borrow::Cow;
use std::fs::{self, File};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::emit::{Rar4SplitParams, emit_rar4_segment, emit_rar4_split};
use super::encode::{best_rar29_member, encode_legacy_codec_member, rar4_member_encrypt};
use super::member::push_rar4_entry;
use crate::engine::{
    BatchEntry, Engine, PARALLEL_COMPRESS_MAX_MEMBER, PARALLEL_COMPRESS_WAVE_BUDGET,
};
use crate::error::{RarError, RarResult};
use crate::version::LegacyCodec;
pub(crate) struct Rar4PreparedMember {
    pub name: String,
    pub mtime: u32,
    pub mtime_ns: u32,
    pub file_size: u64,
    pub file_crc: u32,
    pub packed: Vec<u8>,
    pub method: u8,
    /// The member's DOS attributes (read-only/hidden/system), like WinRAR.
    pub attr: u32,
    /// The requested compression level, needed to pick the member's
    /// `unp_ver` (0 writes a RAR 2.x member).
    pub level: u8,
}

/// Compress one RAR4 file member (non-solid, independent engine state)
/// without touching the archive stream: read + CRC + the smallest of
/// LZ / PPMd (m4+) / auto-filter candidates / STORE. Runs on the pool;
/// emission happens later on the writing thread.
pub(crate) fn prepare_rar4_file_member(
    path: &Path,
    name: &str,
    level: u8,
    codec: LegacyCodec,
    filters: crate::options::FilterOptions,
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
    let attr = crate::platform::rar4_file_attributes(&meta);
    let mut reader = File::open(path)?;
    let mut data = Vec::with_capacity(file_size as usize);
    std::io::Read::read_to_end(&mut reader, &mut data)?;
    let file_crc = crate::crc32::crc32(&data);

    let (packed, method) =
        if crate::format::shared::write_ops::whole_member_is_incompressible(&data, level) {
            (data, crate::format::rar4::RAR4_METHOD_STORE)
        } else if (1..=5).contains(&level) && codec != LegacyCodec::Rar29 {
            let packed = encode_legacy_codec_member(&data, level, codec)?;
            if packed.len() < data.len() {
                (packed, crate::format::rar4::RAR4_METHOD_STORE + level)
            } else {
                (data, crate::format::rar4::RAR4_METHOD_STORE)
            }
        } else if (1..=5).contains(&level) {
            match best_rar29_member(&data, level, filters)? {
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
        attr,
        level,
    })
}

/// Emit a compressed RAR4 member prepared on a worker thread: member
/// encryption, header + payload (single or multi-volume split), entry
/// bookkeeping and progress. Mirrors `add_file_rar4`'s emission half
/// for non-solid members (no FHD_SOLID continuation).
fn emit_rar4_prepared(cx: &mut dyn Engine, prepared: Rar4PreparedMember) -> RarResult<()> {
    let Rar4PreparedMember {
        name,
        mtime,
        mtime_ns,
        file_size,
        file_crc,
        mut packed,
        method,
        attr,
        level,
    } = prepared;
    let member_unp_ver = crate::format::rar4::write::member_unp_ver(
        cx.write_ctx().solid.rar4_unp_ver,
        level,
        cx.password().is_some_and(|pw| !pw.is_empty()),
    );
    let ext_time =
        crate::format::rar4::write::build_member_ext_time(member_unp_ver, mtime, Some(mtime_ns));

    let password_encrypted = cx.password().is_some_and(|pw| !pw.is_empty());
    let mut salt = None;
    if password_encrypted {
        salt = rar4_member_encrypt(cx, &mut packed)?;
    }
    let packed_size = packed.len() as u64;
    let unpacked_size = file_size;

    let dos_time = crate::format::rar4::write::unix_to_dos_time(mtime);
    let (encoded_name, name_flags) = crate::format::rar4::write::encode_file_name(&name);

    match cx.write_ctx().output.volume_size {
        None => {
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
                false,
                attr,
                None,
                false,
                false,
                member_unp_ver,
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
                None,
                false,
                attr,
                0,
                vec![crate::model::DataChunk {
                    volume_index: 0,
                    data_offset,
                    packed_size,
                    crc32_val: Some(file_crc),
                    is_final: true,
                    extra_data: Vec::new(),
                }],
                member_unp_ver,
            );
            cx.report_progress(file_size, file_size);
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
                    attr,
                    comment: None,
                    unp_ver: member_unp_ver,
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
                None,
                false,
                attr,
                0,
                chunks,
                member_unp_ver,
            );
            cx.report_progress(file_size, file_size);
            Ok(())
        }
    }
}

/// Parallel RAR4 batch: waves of independent non-solid file members are
/// compressed on the pool and emitted in archive order (byte-identical
/// to the sequential path). Solid runs, directories and oversized
/// members fall back to the sequential path at their original position.
pub(crate) fn add_batch_parallel_rar4(
    cx: &mut dyn Engine,
    entries: &[BatchEntry<'_>],
) -> RarResult<()> {
    use rayon::prelude::*;
    crate::format::shared::write_ops::progress_set_batch_total(cx, entries)?;
    let mut i = 0usize;
    while i < entries.len() {
        cx.check_cancel()?;
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
            let threads = cx.effective_threads();
            let pool = crate::parallel::compression_pool_for(threads);
            let unp_ver = cx.write_ctx().solid.rar4_unp_ver;
            let codec = LegacyCodec::from_unp_ver(unp_ver).ok_or_else(|| {
                RarError::Unsupported(format!(
                    "RAR4 write dispatch: unp_ver {unp_ver} has no encoder"
                ))
            })?;
            let filters = cx.write_ctx().compression.filters;
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
                        prepare_rar4_file_member(path, &name, level, codec, filters)
                            .map(|p| (idx, p))
                    })
                    .collect()
            });
            cx.check_cancel()?;
            let mut ordered = Vec::with_capacity(prepared.len());
            for result in prepared {
                ordered.push(result?);
            }
            ordered.sort_by_key(|(idx, _)| *idx);
            for (idx, member) in ordered {
                cx.set_progress_member(idx);
                emit_rar4_prepared(cx, member)?;
            }
        }
        if i < entries.len() {
            cx.set_progress_member(i);
            crate::format::shared::write_ops::add_batch_entry_sequential(cx, &entries[i])?;
            i += 1;
        }
    }
    Ok(())
}
