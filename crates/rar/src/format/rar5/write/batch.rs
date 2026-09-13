//! Parallel batch preparation: compress a wave of independent members on
//! the Rayon pool, then serialize the results back in archive order.
//!
//! Split out of `write/mod.rs`; the sequential fallback shares the
//! progress/entry helpers here.

use std::fs;
#[cfg(feature = "parallel")]
use std::io;
#[cfg(feature = "parallel")]
use std::path::Path;
#[cfg(feature = "parallel")]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(feature = "parallel")]
use super::add::{owner_extra_cfg, time_extra_cfg};
#[cfg(feature = "parallel")]
use super::layout::{dict_params_for, sample_is_incompressible};
use crate::archive::{BatchEntry, RarArchive};
#[cfg(feature = "parallel")]
use crate::archive::{
    BatchPrepareCtx, PARALLEL_COMPRESS_MAX_MEMBER, PARALLEL_COMPRESS_WAVE_BUDGET, PreparedEntry,
};
#[cfg(feature = "parallel")]
use crate::codec::lzss_huff;
#[cfg(feature = "parallel")]
use crate::error::RarError;
use crate::error::RarResult;
#[cfg(feature = "parallel")]
use crate::format::rar5::{COMP_METHOD_STORE, level_to_method};
#[cfg(feature = "parallel")]
use crate::format::shared::write_ops::archive_name_from_path;
#[cfg(feature = "parallel")]
use crate::parallel::BatchWorkerGuard;
#[cfg(feature = "parallel")]
use crate::write_progress::ProgressTracker;

impl RarArchive {
    #[cfg(feature = "parallel")]
    pub(crate) fn add_batch_parallel(&mut self, entries: &[BatchEntry<'_>]) -> RarResult<()> {
        self.progress_set_batch_total(entries)?;
        let progress = self.progress.clone();
        let mut i = 0usize;
        while i < entries.len() {
            self.check_cancel()?;
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
                // The whole wave compresses concurrently; the shared tracker
                // turns each member's per-chunk events (and its completion,
                // reported inside `prepare_batch_wave`) into a monotonic
                // global stream, so the bar moves while the CPU-heavy pass
                // runs instead of freezing until every member is done.
                let prepared =
                    self.prepare_batch_wave(&wave, progress.as_ref(), self.effective_threads())?;
                self.check_cancel()?;
                for (idx, entry) in prepared {
                    self.progress_member = idx;
                    self.write_prepared_entry(entry)?;
                }
            }

            if i < entries.len() {
                if let BatchEntry::File { path, name, level } = entries[i] {
                    let size = fs::metadata(path)?.len();
                    // Members over the parallel wave budget stream through
                    // the sequential path: the compressed output is spilled
                    // to a temporary file instead of being buffered in
                    // memory (bounded memory for any file size), and the
                    // persistent encoder state keeps the LZ window (tail +
                    // long-range history) across chunks — byte-identical
                    // to `add_file` and with the same compression ratio.
                    let _ = (path, name, level, size);
                }
                self.progress_member = i;
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
        progress: Option<&std::sync::Arc<std::sync::Mutex<ProgressTracker>>>,
        threads: usize,
    ) -> RarResult<Vec<(usize, PreparedEntry)>> {
        use rayon::prelude::*;

        let ctx = BatchPrepareCtx {
            password: self.password.as_deref(),
            blake2: self.write_ctx().meta.blake2,
            dict_size_log: self.write_ctx().compression.dict_size_log,
            dict_size_bytes: self.write_ctx().compression.dict_size_bytes,
            force_v70: self.write_ctx().compression.force_v70,
            filters: self.write_ctx().compression.filters,
            save_ctime: self.write_ctx().meta.ctime,
            save_atime: self.write_ctx().meta.atime,
            save_mtime: self.write_ctx().meta.mtime,
            save_owner: self.write_ctx().meta.owner,
            time_precision_seconds: self.write_ctx().meta.time_precision_seconds,
            threads,
            cancel: self.cancel.clone(),
        };
        let results: Vec<RarResult<(usize, PreparedEntry)>> =
            crate::parallel::compression_pool_for(threads).install(|| {
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
                                    &ctx, name, data, level, 0o100644, mtime, None, None, false,
                                    idx, progress,
                                )
                            }
                            BatchEntry::File { path, name, level } => {
                                Self::prepare_file_entry(&ctx, path, name, level, idx, progress)
                            }
                            BatchEntry::Directory { .. } => {
                                unreachable!("directories never enter a compression wave")
                            }
                        };
                        prepared.map(|p| {
                            if let Some(progress) = progress {
                                let total = match entry {
                                    BatchEntry::Bytes { data, .. } => data.len() as u64,
                                    BatchEntry::File { path, .. } => {
                                        fs::metadata(path).map(|m| m.len()).unwrap_or(0)
                                    }
                                    BatchEntry::Directory { .. } => 0,
                                };
                                progress
                                    .lock()
                                    .expect("progress lock")
                                    .report(idx, total, total);
                            }
                            (idx, p)
                        })
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
    /// `member` and `progress` route per-chunk deltas into the shared tracker
    /// so the parallel wave reports live progress.
    #[cfg(feature = "parallel")]
    #[allow(clippy::too_many_arguments)]
    fn prepare_data_entry(
        ctx: &BatchPrepareCtx<'_>,
        name: &str,
        data: &[u8],
        level: u8,
        attrs: u64,
        mtime: u32,
        time_extra: Option<Vec<u8>>,
        owner_extra: Option<Vec<u8>>,
        file_origin: bool,
        member: usize,
        progress: Option<&std::sync::Arc<std::sync::Mutex<ProgressTracker>>>,
    ) -> RarResult<PreparedEntry> {
        let plain_crc = crc32fast::hash(data);
        let plain_blake = if ctx.blake2 {
            Some(crate::format::rar5::blake2sp::hash(data))
        } else {
            None
        };
        let method = level_to_method(level);

        if method == COMP_METHOD_STORE || sample_is_incompressible(data, method) {
            // Count the member's bytes so a folder of incompressible files
            // moves the bar during the (CPU-heavy) hashing pass instead of
            // freezing at ~0% until the terminal event slams it to 100%.
            // The write-back safety net in `write_prepared_entry` treats
            // this as a no-op (delta is already accounted).
            if let Some(progress) = progress {
                progress.lock().expect("progress lock").report(
                    member,
                    data.len() as u64,
                    data.len() as u64,
                );
            }
            return Self::prepared_from_payload(
                ctx,
                name,
                data.len(),
                attrs,
                mtime,
                time_extra,
                owner_extra,
                plain_crc,
                plain_blake,
                COMP_METHOD_STORE,
                0,
                None,
                data.to_vec(),
            );
        }

        let (dsl, dict_bytes) = dict_params_for(
            data.len(),
            ctx.dict_size_log,
            ctx.dict_size_bytes,
            method,
            ctx.force_v70,
        );
        // One encoder state per member: the sequential path keeps the LZ
        // window (tail + long-range history) within a member and resets
        // between members, so the batch archive stays byte-identical to
        // it while remaining parallel across members.
        let mut state = crate::codec::EncoderState::default();
        let total = data.len() as u64;
        let packed = if file_origin {
            // Mirror add_file's member encoding exactly for byte-identity:
            // the automatic delta (multimedia) filter runs first, then the
            // x86 (E8/E8E9) filter; each is kept only when it strictly beats
            // plain LZSS (the encoder compares against an unfiltered pack).
            // A filtered member is written standalone (non-solid); otherwise
            // the member is compressed in bounded chunks with one shared
            // encoder state across chunks.
            let cancel_ref = ctx.cancel.as_deref();
            let variant = crate::version::ArchiveVersion::from_v70(dict_bytes.is_some());
            match super::filter_policy::encode_with_filter_policy(
                data,
                method,
                dsl,
                variant,
                ctx.filters,
                ctx.threads,
                cancel_ref,
            )? {
                Some(filtered) if filtered.len() < data.len() => filtered,
                _ => {
                    // Mid-size members run the same windowed MT encode as
                    // add_file and the streaming path (byte-identical to
                    // add_file's MT branch — both slice the whole buffer
                    // with one shared encoder state); smaller ones or
                    // solid chains keep the sequential chunk loop with
                    // per-64 KiB progress.
                    const MT_MIN: usize = 3 * crate::codec::DEFAULT_CHUNK_SIZE;
                    if ctx.threads > 1 && data.len() >= MT_MIN {
                        let progress = progress.cloned();
                        let mut cb = move |done: u64, _total: u64| {
                            if let Some(progress) = &progress {
                                progress
                                    .lock()
                                    .expect("progress lock")
                                    .report(member, done, total);
                            }
                        };
                        crate::codec::lzss_huff::encode_chunked_mt_with_progress(
                            data,
                            method,
                            dsl,
                            crate::codec::DEFAULT_CHUNK_SIZE,
                            &mut state,
                            ctx.threads,
                            true,
                            crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                            None,
                            Some(&mut cb),
                            ctx.cancel.as_deref(),
                        )?
                    } else {
                        let mut packed = Vec::new();
                        // Fine-grained progress: the sequential path feeds the
                        // encoder a per-64 KiB callback (`encode_chunked` reports
                        // every 0x10000 input bytes); the batch path used to only
                        // report after each whole 4 MiB chunk, so the bar stepped
                        // 64× more coarsely. Route a per-64 KiB callback into the
                        // chunk encoder and offset its chunk-relative reports by
                        // the member bytes already processed, so the shared
                        // tracker sees a smooth member-relative stream.
                        let processed_cell = std::cell::Cell::new(0u64);
                        let cell_ref = &processed_cell;
                        let mut cb = move |done: u64, _chunk_total: u64| {
                            if let Some(progress) = progress {
                                progress.lock().expect("progress lock").report(
                                    member,
                                    cell_ref.get() + done,
                                    total,
                                );
                            }
                        };
                        for chunk in data.chunks(crate::codec::DEFAULT_CHUNK_SIZE) {
                            if ctx
                                .cancel
                                .as_ref()
                                .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
                            {
                                return Err(RarError::Cancelled);
                            }
                            // Same finality rule as add_file's streaming loop:
                            // the last chunk is final even when it fills the
                            // whole 4 MiB slice (an exact-multiple member must
                            // still mark its closing block).
                            let is_final = processed_cell.get() + chunk.len() as u64 >= total;
                            let compressed = lzss_huff::encode_chunked(
                                chunk,
                                lzss_huff::EncodeOptions {
                                    chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                                    state: Some(&mut state),
                                    is_final,
                                    variant: crate::version::ArchiveVersion::from_v70(
                                        dict_bytes.is_some(),
                                    ),
                                    progress: Some(&mut cb),
                                    ..lzss_huff::EncodeOptions::new(method, dsl)
                                },
                            )?;
                            packed.extend(compressed);
                            processed_cell.set(processed_cell.get() + chunk.len() as u64);
                            if packed.len() >= data.len() {
                                break;
                            }
                        }
                        packed
                    }
                }
            }
        } else {
            // add_bytes path: no filter attempt, one shared window. Same MT
            // gate as the file path.
            const MT_MIN: usize = 3 * crate::codec::DEFAULT_CHUNK_SIZE;
            if ctx.threads > 1 && data.len() >= MT_MIN {
                let progress = progress.cloned();
                let mut cb = move |done: u64, _total: u64| {
                    if let Some(progress) = &progress {
                        progress
                            .lock()
                            .expect("progress lock")
                            .report(member, done, total);
                    }
                };
                crate::codec::lzss_huff::encode_chunked_mt_with_progress(
                    data,
                    method,
                    dsl,
                    crate::codec::DEFAULT_CHUNK_SIZE,
                    &mut state,
                    ctx.threads,
                    true,
                    crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                    None,
                    Some(&mut cb),
                    ctx.cancel.as_deref(),
                )?
            } else {
                lzss_huff::encode_chunked(
                    data,
                    lzss_huff::EncodeOptions {
                        chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                        state: Some(&mut state),
                        is_final: true,
                        variant: crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                        ..lzss_huff::EncodeOptions::new(method, dsl)
                    },
                )?
            }
        };

        if packed.len() >= data.len() {
            return Self::prepared_from_payload(
                ctx,
                name,
                data.len(),
                attrs,
                mtime,
                time_extra,
                owner_extra,
                plain_crc,
                plain_blake,
                COMP_METHOD_STORE,
                0,
                None,
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
            owner_extra,
            plain_crc,
            plain_blake,
            method,
            dsl,
            dict_bytes,
            packed,
        )
    }

    #[cfg(feature = "parallel")]
    fn prepare_file_entry(
        ctx: &BatchPrepareCtx<'_>,
        path: &Path,
        arcname: Option<&str>,
        level: u8,
        member: usize,
        progress: Option<&std::sync::Arc<std::sync::Mutex<ProgressTracker>>>,
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
        let time_extra = time_extra_cfg(
            ctx.save_ctime,
            ctx.save_atime,
            ctx.save_mtime,
            ctx.time_precision_seconds,
            &meta,
            path,
            mtime,
            mtime_ns,
        );
        let owner_extra = owner_extra_cfg(ctx.save_owner, &meta);

        #[cfg(unix)]
        let attrs = {
            use std::os::unix::fs::MetadataExt;
            meta.mode() as u64
        };
        #[cfg(not(unix))]
        let attrs = 0o100644u64;

        let name = match arcname {
            Some(s) => s.to_string(),
            None => archive_name_from_path(path)?,
        };
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
        Self::prepare_data_entry(
            ctx,
            &name,
            &data,
            level,
            attrs,
            mtime,
            time_extra,
            owner_extra,
            true,
            member,
            progress,
        )
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
        owner_extra: Option<Vec<u8>>,
        plain_crc: u32,
        plain_blake: Option<[u8; 32]>,
        method: u8,
        dict_size_log: u8,
        dict_size_bytes: Option<u64>,
        payload: Vec<u8>,
    ) -> RarResult<PreparedEntry> {
        let (header_crc, mut extra_data, stored_hash, encr) =
            RarArchive::payload_extra_and_crc(ctx.password, plain_crc, plain_blake)?;
        if let Some(t) = time_extra {
            extra_data.extend_from_slice(&t);
        }
        if let Some(t) = owner_extra {
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
            dict_size_bytes,
            extra_data,
            stored_hash,
            payload,
        })
    }

    #[cfg(feature = "parallel")]
    fn write_prepared_entry(&mut self, entry: PreparedEntry) -> RarResult<()> {
        // Safety net: every member's bytes must enter the shared tracker
        // exactly once. Compression already reported most of them (LZSS per
        // 64 KiB, STORE/filtered at completion); this accounts any remaining
        // delta when the payload is written back to the archive stream, so a
        // member can never leave the bar short of the full total. It is a
        // no-op when the member already reported its full size.
        if let Some(progress) = self.progress.clone() {
            let member = self.progress_member;
            progress.lock().expect("progress lock").report(
                member,
                entry.unpacked_size,
                entry.unpacked_size,
            );
        }
        self.write_file_entry(
            &entry.name,
            entry.unpacked_size,
            &entry.payload,
            entry.file_crc,
            entry.method,
            entry.dict_size_log,
            entry.dict_size_bytes,
            &entry.extra_data,
            entry.attrs,
            entry.mtime,
            false,
            entry.stored_hash,
        )
    }
}
