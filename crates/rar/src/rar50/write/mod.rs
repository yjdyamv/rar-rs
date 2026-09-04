//! Write pipeline: member creation, batch addition and the streaming
//! payload writer. Methods on [RarArchive] live in a sibling impl block
//! (see src/archive.rs for the shared state).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) use self::engine::{
    CountingWriter, CrcSink, ProgressWriter, SpillGuard, payload_stream, spill_path_for,
};
use crate::archive::{ArchiveEntry, BatchEntry, Mode, RarArchive, STREAM_COMPRESS_THRESHOLD};
#[cfg(feature = "parallel")]
use crate::archive::{
    BatchPrepareCtx, PARALLEL_COMPRESS_MAX_MEMBER, PARALLEL_COMPRESS_WAVE_BUDGET, PreparedEntry,
};
use crate::codec::lzss_huff;
use crate::crypto;
use crate::error::{RarError, RarResult};
use crate::rar50::write::layout::{
    SAMPLE_PROBE_HEAD, dict_params_for, hash_file, sample_is_incompressible,
    sample_is_incompressible_file,
};
#[cfg(feature = "parallel")]
use crate::write_progress::ProgressTracker;

use crate::rar50::headers::*;
#[cfg(windows)]
use crate::rar50::vint;
use crate::rar50::*;
pub(crate) mod engine;
pub(crate) mod layout;
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use self::windows::{windows_set_creation_time, write_windows_stream};
#[cfg(feature = "parallel")]
use crate::parallel::BatchWorkerGuard;

/// Build the FILE_TIME extra record per explicit `-ts` settings (the
/// off-thread parallel batch path has no `&RarArchive`); `None` when no
/// time needs the extra record. On Windows the access/creation times are
/// read through `GetFileTime` (std exposes no access-time API).
#[allow(clippy::too_many_arguments)]
fn time_extra_cfg(
    save_ctime: bool,
    save_atime: bool,
    save_mtime: bool,
    precision_seconds: bool,
    meta: &fs::Metadata,
    _path: &Path,
    mtime: u32,
    mtime_ns: u32,
) -> Option<Vec<u8>> {
    // Only unix/windows branches use the nanosecond normalizer; on other
    // targets (e.g. wasm) the closure would be dead code.
    #[cfg(any(unix, windows))]
    let ns = |v: u32| if precision_seconds { 0 } else { v };
    let ctime = if save_ctime {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some((meta.ctime() as u64, ns(meta.ctime_nsec() as u32)))
        }
        #[cfg(windows)]
        {
            let _ = meta;
            windows::windows_file_time(_path, false).map(|(s, n)| (s, ns(n)))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = meta;
            let _ = _path;
            None
        }
    } else {
        None
    };
    let atime = if save_atime {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some((meta.atime() as u64, ns(meta.atime_nsec() as u32)))
        }
        #[cfg(windows)]
        {
            let _ = meta;
            windows::windows_file_time(_path, true).map(|(s, n)| (s, ns(n)))
        }
        #[cfg(not(any(unix, windows)))]
        {
            None
        }
    } else {
        None
    };
    let mtime = save_mtime.then_some((mtime as u64, if precision_seconds { 0 } else { mtime_ns }));
    let present = mtime.is_some() || ctime.is_some() || atime.is_some();
    present.then(|| file_time_extra_record(mtime, ctime, atime))
}

/// Build the OWNER extra record (numeric uid/gid) per `-ow`; `None`
/// off-Unix or when disabled. Off-thread variant of `owner_extra_for`.
fn owner_extra_cfg(save_owner: bool, meta: &fs::Metadata) -> Option<Vec<u8>> {
    if !save_owner {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(build_owner_extra_record(
            &meta.uid().to_string(),
            &meta.gid().to_string(),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Scalar member fields shared by both multi-volume split drivers; the
/// chunk headers (which repeat per volume) are built from these rather
/// than from the base header so payload-specific fields stay defaulted.
struct SplitParams<'a> {
    name: &'a str,
    unpacked_size: u64,
    attrs: u64,
    mtime: u32,
    method: u8,
    solid: bool,
    dict_size_log: u8,
    dict_size_bytes: Option<u64>,
    extra_data: &'a [u8],
}

/// Which half of a split chunk the per-chunk source closure is asked for.
/// The loop invokes it once in [SplitPhase::Crc] (before the block header is
/// emitted) and again in [SplitPhase::Write] (after it), so a phase-returning
/// `u64` is at once the chunk checksum or its on-disk `data_offset`.
#[derive(Clone, Copy)]
enum SplitPhase {
    Crc,
    Write,
}

impl RarArchive {
    // ── Public API: creation ───────────────────────────────────────────────

    /// Build the FILE_TIME extra record for `meta`, per the current
    /// `-ts` settings; `None` when no time needs the extra record.
    fn time_extra_for(
        &self,
        meta: &fs::Metadata,
        path: &Path,
        mtime: u32,
        mtime_ns: u32,
    ) -> Option<Vec<u8>> {
        time_extra_cfg(
            self.write_ctx().save_ctime,
            self.write_ctx().save_atime,
            self.write_ctx().save_mtime,
            self.write_ctx().time_precision_seconds,
            meta,
            path,
            mtime,
            mtime_ns,
        )
    }

    /// Build the OWNER extra record (numeric uid/gid) when `-ow` is on;
    /// `None` off-Unix or when disabled.
    fn owner_extra_for(&self, meta: &fs::Metadata) -> Option<Vec<u8>> {
        owner_extra_cfg(self.write_ctx().save_owner, meta)
    }

    /// Add a file from the filesystem to the archive.
    pub fn add(&mut self, path: impl AsRef<Path>, compression_level: u8) -> RarResult<()> {
        self.check_cancel()?;
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
        self.check_cancel()?;
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
        self.check_cancel()?;
        if self.rar4 {
            return self.add_file_rar4(path, arcname, level);
        }
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
        let time_extra = self.time_extra_for(&meta, path, mtime, mtime_ns);
        let owner_extra = self.owner_extra_for(&meta);

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

        if self.progress.is_some() {
            self.report_progress(0, file_size);
        }

        let method = level_to_method(level);
        let probe_incompressible = method != COMP_METHOD_STORE
            && file_size >= (SAMPLE_PROBE_HEAD as u64) * 4
            && sample_is_incompressible_file(path, file_size, method)?;
        let (dsl, dict_bytes) = dict_params_for(
            file_size as usize,
            self.write_ctx().dict_size_log,
            self.write_ctx().dict_size_bytes,
            method,
            self.write_ctx().force_v70,
        );

        if method == COMP_METHOD_STORE || probe_incompressible {
            // STORE is written by streaming the file directly: bounded
            // memory regardless of file size. Encrypted STORE is encrypted
            // on the fly with a chained CBC state (also bounded memory).
            self.reset_solid_chain();
            let (plain_crc, plain_blake) = hash_file(path, file_size, self.write_ctx().blake2)?;
            let (header_crc, mut extra_data, stored_hash, encr_params) =
                RarArchive::payload_extra_and_crc(
                    self.password.as_deref(),
                    plain_crc,
                    plain_blake,
                )?;
            if let Some(ref t) = time_extra {
                extra_data.extend_from_slice(t);
            }
            if let Some(ref t) = owner_extra {
                extra_data.extend_from_slice(t);
            }
            self.write_store_member(
                path,
                &name,
                file_size,
                header_crc,
                &extra_data,
                stored_hash,
                encr_params.as_ref(),
                attrs,
                mtime,
                dict_bytes,
            )?;
            self.report_progress(file_size, file_size);
            return Ok(());
        }

        // Compressed path: files at or above the streaming threshold are
        // compressed in bounded chunks to a temporary spill file and then
        // streamed into the archive (bounded memory for any file size);
        // smaller files are compressed in memory.
        if file_size >= STREAM_COMPRESS_THRESHOLD {
            return self.add_file_streaming(
                path,
                &name,
                file_size,
                attrs,
                mtime,
                time_extra,
                owner_extra,
                method,
                dsl,
                dict_bytes,
            );
        }

        // Compressed path: read the member whole (bounded by the streaming
        // threshold), hash it, and try the automatic x86 (E8/E8E9) filter
        // first — WinRAR applies it to x86 code and it is worth several
        // percent on real binaries. A filtered member is written standalone
        // (non-solid): the decoder's window holds transformed bytes and the
        // filter positions are member-relative, so it cannot share the LZ
        // window with its neighbours.
        let mut whole = Vec::with_capacity(file_size as usize);
        {
            let mut file = io::BufReader::with_capacity(1 << 20, File::open(path)?);
            file.read_to_end(&mut whole)?;
        }
        let mut crc_hasher = crc32fast::Hasher::new();
        let mut blake_hasher = if self.write_ctx().blake2 {
            Some(crate::rar50::blake2sp::Hasher::new())
        } else {
            None
        };
        crc_hasher.update(&whole);
        if let Some(h) = blake_hasher.as_mut() {
            h.update(&whole);
        }
        let plain_crc = crc_hasher.finalize();
        let plain_blake = blake_hasher.map(|h| h.finalize());

        // Try the automatic delta (multimedia) filter first, then the x86
        // (E8/E8E9) filter. Ordering matters: real x86 code is not
        // multi-channel-correlated, so the cheap delta scan returns `None`
        // immediately and we fall through to x86; for correlated audio/raw
        // data the delta filter wins outright, so we never pay for a useless
        // x86 scan. Each filter is only kept when it strictly beats plain
        // LZSS (the encoder compares against an unfiltered pack), so neither
        // can steal a member from the better transform or from plain LZSS.
        // The caller's `< file_size` guard only accepts a filter when it also
        // beats STORE.
        let cancel_ref = self.cancel.as_deref();
        let filtered = match lzss_huff::encode_with_auto_delta_filter(
            &whole,
            method,
            dsl,
            crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
            self.effective_threads(),
            cancel_ref,
        )? {
            Some(f) => Some(f),
            None => lzss_huff::encode_with_auto_x86_filter(
                &whole,
                method,
                dsl,
                crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                self.effective_threads(),
                cancel_ref,
            )?,
        };
        if let Some(filtered) = filtered
            && (filtered.len() as u64) < file_size
        {
            self.reset_solid_chain();
            let (header_crc, mut extra_data, stored_hash, encr_params) =
                RarArchive::payload_extra_and_crc(
                    self.password.as_deref(),
                    plain_crc,
                    plain_blake,
                )?;
            if let Some(ref t) = time_extra {
                extra_data.extend_from_slice(t);
            }
            if let Some(ref t) = owner_extra {
                extra_data.extend_from_slice(t);
            }
            let packed_data = RarArchive::encrypt_payload_with(
                self.password.as_deref(),
                encr_params.as_ref(),
                &filtered,
            )?;
            self.write_file_entry(
                &name,
                file_size,
                &packed_data,
                header_crc,
                method,
                dsl,
                dict_bytes,
                &extra_data,
                attrs,
                mtime,
                false,
                stored_hash,
            )?;
            self.write_member_streams(path)?;
            self.report_progress(file_size, file_size);
            return Ok(());
        }

        // Unfiltered path: compress in bounded chunks with a persistent
        // encoder state (solid archives share the LZ window; non-solid
        // members keep one window within the member, reset between
        // members). The persistent state also carries the long-range match
        // history across chunk boundaries (WinRAR's `-mcl` long range
        // search).
        // WinRAR `-se`: reset the solid statistics when the extension changes.
        self.maybe_reset_solid_for_extension(&name);
        let chain_solid = self.write_ctx().solid_mode && self.write_ctx().encoder_state.is_some();
        self.write_ctx_mut()
            .encoder_state
            .get_or_insert_with(Default::default);

        // Mid-size members (2-64 MiB) get the same windowed MT encode as the
        // streaming path, matching WinRAR's per-file parallelization; the
        // measured ratio divergence from the sequential chunk loop on the
        // corpus is within ±0.3% (the repeat-distance cache resets per
        // slice). Filter members stay sequential (the transform runs over
        // the whole buffer), as do solid chains.
        #[cfg(feature = "parallel")]
        const MT_MIN: usize = 3 * crate::codec::DEFAULT_CHUNK_SIZE;
        #[cfg(feature = "parallel")]
        let threads = self.effective_threads();
        #[cfg(not(feature = "parallel"))]
        let threads = 1usize;
        let mut packed = Vec::new();
        let use_mt = {
            #[cfg(feature = "parallel")]
            {
                !chain_solid && threads > 1 && whole.len() >= MT_MIN
            }
            #[cfg(not(feature = "parallel"))]
            {
                let _ = threads;
                false
            }
        };
        if use_mt {
            let state = self
                .write_ctx_mut()
                .encoder_state
                .as_mut()
                .expect("encoder state seeded");
            packed = crate::codec::lzss_huff::encode_chunked_mt(
                &whole,
                method,
                dsl,
                crate::codec::DEFAULT_CHUNK_SIZE,
                state,
                threads,
                true,
                crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
            );
            self.report_progress(file_size, file_size);
        } else {
            let mut bytes_read = 0u64;
            for chunk in whole.chunks(crate::codec::DEFAULT_CHUNK_SIZE) {
                self.check_cancel()?;
                bytes_read += chunk.len() as u64;
                let state = self.write_ctx_mut().encoder_state.as_mut();
                let compressed = lzss_huff::encode_chunked(
                    chunk,
                    lzss_huff::EncodeOptions {
                        chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                        state,
                        is_final: bytes_read >= whole.len() as u64,
                        variant: crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                        ..lzss_huff::EncodeOptions::new(method, dsl)
                    },
                )?;
                packed.extend(compressed);
                self.report_progress(bytes_read, file_size);
                if packed.len() as u64 >= file_size {
                    break;
                }
            }
        }

        if packed.len() as u64 >= file_size {
            // Compression is a net loss: fall back to streaming STORE.
            self.reset_solid_chain();
            let (header_crc, mut extra_data, stored_hash, encr_params) =
                RarArchive::payload_extra_and_crc(
                    self.password.as_deref(),
                    plain_crc,
                    plain_blake,
                )?;
            if let Some(ref t) = time_extra {
                extra_data.extend_from_slice(t);
            }
            if let Some(ref t) = owner_extra {
                extra_data.extend_from_slice(t);
            }
            self.write_store_member(
                path,
                &name,
                file_size,
                header_crc,
                &extra_data,
                stored_hash,
                encr_params.as_ref(),
                attrs,
                mtime,
                dict_bytes,
            )?;
            self.write_member_streams(path)?;
            self.report_progress(file_size, file_size);
            return Ok(());
        }

        let (header_crc, mut extra_data, stored_hash, encr_params) =
            RarArchive::payload_extra_and_crc(self.password.as_deref(), plain_crc, plain_blake)?;
        if let Some(ref t) = time_extra {
            extra_data.extend_from_slice(t);
        }
        if let Some(ref t) = owner_extra {
            extra_data.extend_from_slice(t);
        }
        let packed_data = RarArchive::encrypt_payload_with(
            self.password.as_deref(),
            encr_params.as_ref(),
            &packed,
        )?;
        self.write_file_entry(
            &name,
            file_size,
            &packed_data,
            header_crc,
            method,
            dsl,
            dict_bytes,
            &extra_data,
            attrs,
            mtime,
            chain_solid,
            stored_hash,
        )?;
        self.write_member_streams(path)?;
        // Non-solid members use an independent LZ window: drop the
        // encoder state so the next member starts fresh.
        if !self.write_ctx().solid_mode {
            self.reset_solid_chain();
        }

        self.report_progress(file_size, file_size);

        Ok(())
    }

    /// RAR4 STORE path: write a member in the legacy container — STORE or
    /// LZSS-compressed (m1–m5), optionally AES-encrypted with the member
    /// password. A single-volume member is one FILE_HEAD + data; in a
    /// multi-volume set the member data is split at volume boundaries,
    /// writing `FHD_SPLIT_AFTER` on every non-final head and
    /// `FHD_SPLIT_BEFORE` on every continuation head.
    fn add_file_rar4(&mut self, path: &Path, arcname: Option<&str>, level: u8) -> RarResult<()> {
        let meta = fs::metadata(path)?;
        let file_size = meta.len();
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;
        let name = arcname
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().into_owned());
        let name = name.replace('\\', "/");

        if self.progress.is_some() {
            self.report_progress(0, file_size);
        }

        // Read the whole member, then (for level >= 1) LZSS-compress it.
        let mut reader = File::open(path)?;
        let mut data = Vec::with_capacity(file_size as usize);
        std::io::Read::read_to_end(&mut reader, &mut data)?;
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
        let (mut packed, method) = if (1..=5).contains(&level) {
            use crate::codec::rar29_encoder::{
                Rar29FilterKind, Unpack29Encoder, options_for_level,
            };
            let options = options_for_level(level);
            let solid = self.write_ctx().solid_mode;
            let lz = if solid {
                // Solid: reuse the persistent encoder so its sliding window
                // and Huffman table state carry across the members of the
                // run (this is what makes a real -ms archive compress better
                // than independent members). Auto-filters stay out of solid
                // runs (a filtered member's window holds the transformed
                // bytes, a later phase); solid RAR4 members are plain LZ,
                // which is also all WinRAR 6.23 produces.
                let encoder = self
                    .write_ctx_mut()
                    .rar4_solid_encoder
                    .get_or_insert_with(|| Unpack29Encoder::with_options(options));
                encoder.encode_member(&data)?
            } else {
                Unpack29Encoder::with_options(options).encode_member(&data)?
            };
            let mut best_len = lz.len();
            let mut best: (Vec<u8>, u8) = (lz, crate::rar40::RAR4_METHOD_STORE + level);

            if !solid && !data.is_empty() {
                // Auto filters on binary members (any level): every candidate
                // is measured with its own throwaway encoder (no chain state).
                // The RAR5 scanners gate the search — text never produces x86
                // clusters or structured deltas.
                let candidates = {
                    let mut list: Vec<(Rar29FilterKind, Vec<std::ops::Range<usize>>)> = Vec::new();
                    let e8e9 = crate::codec::filters::auto_x86_filter_ranges(&data, true);
                    if !e8e9.is_empty() {
                        list.push((Rar29FilterKind::E8E9, e8e9));
                    }
                    let e8 = crate::codec::filters::auto_x86_filter_ranges(&data, false);
                    if !e8.is_empty() {
                        list.push((Rar29FilterKind::E8, e8));
                    }
                    if let Some(channels) = crate::codec::filters::auto_delta_filter_channels(&data)
                    {
                        list.push((
                            Rar29FilterKind::Delta {
                                channels: channels as usize,
                            },
                            std::iter::once(0..data.len()).collect(),
                        ));
                    }
                    list
                };
                for (kind, ranges) in candidates {
                    let Ok(candidate) = Unpack29Encoder::with_options(options)
                        .encode_member_with_filter_ranges(&data, kind, &ranges)
                    else {
                        continue;
                    };
                    if candidate.len() < best_len {
                        best_len = candidate.len();
                        best = (candidate, crate::rar40::RAR4_METHOD_STORE + level);
                    }
                }
            }
            if level >= 4
                && !solid
                && !data.is_empty()
                && let Ok(ppmd) = Unpack29Encoder::with_options(options).encode_ppmd_member(&data)
                && ppmd.len() < best_len
            {
                best_len = ppmd.len();
                best = (ppmd, crate::rar40::RAR4_METHOD_STORE + level);
            }
            if best_len < data.len() {
                best
            } else {
                (data.clone(), crate::rar40::RAR4_METHOD_STORE)
            }
        } else {
            (data.clone(), crate::rar40::RAR4_METHOD_STORE)
        };
        let unpacked_size = file_size;

        // Solid-chain bookkeeping (mirrors rars' `solid_run_has_member`
        // logic): a member is a chain continuation when it compresses and the
        // run has already emitted a member; storing a member rebuilds the
        // encoder and ends the run. The reader keeps its window/tables across
        // members flagged `FHD_SOLID`, so the flags and the encoder must stay
        // in lockstep.
        let solid_continuation = self.write_ctx().solid_mode
            && method != crate::rar40::RAR4_METHOD_STORE
            && self.write_ctx().rar4_solid_run_has_member;
        if method == crate::rar40::RAR4_METHOD_STORE {
            self.write_ctx_mut().rar4_solid_encoder = None;
            self.write_ctx_mut().rar4_solid_run_has_member = false;
        } else if unpacked_size != 0 {
            self.write_ctx_mut().rar4_solid_run_has_member = true;
        }

        let mtime_ns = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let ext_time = crate::rar40::write::build_ext_time(Some(mtime_ns));

        // Member-level encryption (WinRAR `-p`): every member gets its own
        // 8-byte salt; the data is zero-padded to a 16-byte AES block and
        // encrypted with the RAR30 AES-128-CBC cipher. `packed_size` then
        // covers the padded ciphertext; the header CRC stays the plaintext
        // CRC and is checked after decryption.
        let mut salt = None;
        if self.password.as_deref().is_some_and(|pw| !pw.is_empty()) {
            let pw = self.password.as_deref().unwrap();
            let mut s = [0u8; 8];
            rand::fill(&mut s);
            salt = Some(s);
            let mut cipher = crate::crypto::Rar30Cipher::new(pw.as_bytes(), salt)
                .map_err(|e| RarError::Format(format!("RAR4 member key setup: {e:?}")))?;
            let pad = (16 - packed.len() % 16) % 16;
            packed.resize(packed.len() + pad, 0);
            cipher
                .encrypt_in_place(&mut packed)
                .map_err(|e| RarError::Format(format!("RAR4 member encrypt: {e:?}")))?;
        }
        let packed_size = packed.len() as u64;

        let dos_time = crate::rar40::write::unix_to_dos_time(mtime);
        let (encoded_name, name_flags) = crate::rar40::write::encode_file_name(&name);

        // Write head + data for one segment of `[packed[..packed_end])` on the
        // current volume. `split_before` marks a continuation head and
        // `split_after` a head whose data continues on the next volume.
        #[allow(clippy::too_many_arguments)]
        fn emit_segment(
            this: &mut crate::archive::RarArchive,
            encoded_name: &[u8],
            name_flags: u16,
            file_crc: u32,
            dos_time: u32,
            method: u8,
            packed_size: u32,
            unpacked_size: u32,
            data: &[u8],
            salt: Option<[u8; 8]>,
            ext_time: Option<&[u8]>,
            solid_continuation: bool,
            split_before: bool,
            split_after: bool,
        ) -> RarResult<(u64, u64)> {
            use crate::rar40::write::{FileHeaderParams, build_file_header};
            use crate::rar40::{
                FHD_EXTTIME, FHD_PASSWORD, FHD_SALT, FHD_SOLID, FHD_SPLIT_AFTER, FHD_SPLIT_BEFORE,
            };
            let mut fhd = name_flags;
            if split_before {
                fhd |= FHD_SPLIT_BEFORE;
            }
            if split_after {
                fhd |= FHD_SPLIT_AFTER;
            }
            if salt.is_some() {
                fhd |= FHD_PASSWORD | FHD_SALT;
            }
            if ext_time.is_some() {
                fhd |= FHD_EXTTIME;
            }
            if solid_continuation {
                fhd |= FHD_SOLID;
            }
            let params = FileHeaderParams {
                flags: fhd,
                packed_size,
                unpacked_size,
                host_os: 0,
                file_crc,
                file_time: dos_time,
                unp_ver: 29,
                method,
                name: encoded_name,
                attr: 0x20, // archive bit: regular file
                salt,
                ext_time,
                window_bits: 6, // 4 MiB dictionary
            };
            let hdr = build_file_header(&params)?;
            let stream = this.stream.as_mut().unwrap();
            // `-hp`: the file-header block is header-encrypted like every
            // other block after the main header. The member payload (data)
            // itself is NOT part of the ciphertext; it follows the encrypted
            // header on disk and is covered by member-level encryption (`-p`)
            // separately. The data offset is past the `[8B salt][align16]`
            // block, matching the read side's `block.header_end`.
            let (header_bytes, header_on_disk) = if this.header_encryption {
                let password = this.password.as_deref().ok_or_else(|| {
                    RarError::Encrypted("header encryption requires a password".into())
                })?;
                crate::rar40::write::encrypt_block_header(&hdr, password)?
            } else {
                (hdr.clone(), hdr.len() as u64)
            };
            let data_offset = stream.stream_position()? + header_on_disk;
            stream.write_all(&header_bytes)?;
            stream.write_all(data)?;
            this.write_ctx_mut().volume_bytes_written += header_on_disk + data.len() as u64;
            Ok((data_offset, data.len() as u64))
        }

        match self.write_ctx().volume_size {
            None => {
                // ── Single-volume ──
                let (data_offset, _) = emit_segment(
                    self,
                    &encoded_name,
                    name_flags,
                    file_crc,
                    dos_time,
                    method,
                    packed_size as u32,
                    unpacked_size as u32,
                    &packed,
                    salt,
                    ext_time.as_deref(),
                    solid_continuation,
                    false,
                    false,
                )?;
                self.entries.push(crate::archive::ArchiveEntry {
                    header: crate::rar50::headers::FileHeader {
                        name,
                        unpacked_size,
                        packed_size,
                        crc32_val: Some(file_crc),
                        mtime,
                        mtime_ns: Some(mtime_ns),
                        comp_method: method.wrapping_sub(crate::rar40::RAR4_METHOD_STORE),
                        host_os: 0,
                        format_version: 4,
                        unp_ver: 29,
                        data_offset,
                        flags: if salt.is_some() {
                            crate::rar40::FHD_PASSWORD as u64
                        } else {
                            0
                        },
                        salt,
                        extra_data: ext_time.unwrap_or_default(),
                        ..Default::default()
                    },
                    chunks: vec![crate::rar50::headers::DataChunk {
                        volume_index: 0,
                        data_offset,
                        packed_size,
                        crc32_val: Some(file_crc),
                        is_final: true,
                        extra_data: Vec::new(),
                    }],
                });
                self.report_progress(file_size, file_size);
                Ok(())
            }
            Some(volume_size) => {
                // ── Multi-volume: split the packed member across volumes ──
                let mut chunks = Vec::<crate::rar50::headers::DataChunk>::new();
                let mut sent = 0u64;
                let mut vol_index = self.write_ctx().current_volume - 1;
                let mut split_before = false;
                while sent < packed_size {
                    // Roll to a volume with room for at least 7 bytes (EOA).
                    loop {
                        let used = self.write_ctx().volume_bytes_written;
                        if volume_size.saturating_sub(used) > 7 {
                            break;
                        }
                        self.start_next_volume()?;
                        vol_index = self.write_ctx().current_volume - 1;
                    }
                    let used = self.write_ctx().volume_bytes_written;
                    let available = volume_size - used - 7;
                    let chunk_size = (packed_size - sent).min(available);
                    let split_after = sent + chunk_size < packed_size;
                    let segment = &packed[sent as usize..(sent + chunk_size) as usize];
                    // RAR4 split-member CRC convention (matches WinRAR):
                    // every non-final head carries the CRC32 of its OWN
                    // segment's data; only the final head carries the
                    // whole-file CRC. Unpacked size is the full file size in
                    // every head; packed size is per-segment.
                    let head_crc = if split_after {
                        crate::crc32::crc32(segment)
                    } else {
                        file_crc
                    };
                    let (data_offset, _) = emit_segment(
                        self,
                        &encoded_name,
                        name_flags,
                        head_crc,
                        dos_time,
                        method,
                        chunk_size as u32,
                        unpacked_size as u32,
                        segment,
                        salt,
                        ext_time.as_deref(),
                        solid_continuation,
                        split_before,
                        split_after,
                    )?;
                    chunks.push(crate::rar50::headers::DataChunk {
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
                self.entries.push(crate::archive::ArchiveEntry {
                    header: crate::rar50::headers::FileHeader {
                        name,
                        unpacked_size,
                        packed_size,
                        crc32_val: Some(file_crc),
                        mtime,
                        comp_method: method.wrapping_sub(crate::rar40::RAR4_METHOD_STORE),
                        host_os: 0,
                        format_version: 4,
                        unp_ver: 29,
                        data_offset: 0,
                        flags: if salt.is_some() {
                            crate::rar40::FHD_PASSWORD as u64
                        } else {
                            0
                        },
                        salt,
                        extra_data: ext_time.unwrap_or_default(),
                        ..Default::default()
                    },
                    chunks,
                });
                self.report_progress(file_size, file_size);
                Ok(())
            }
        }
    }
    /// The entry carries no data; `redir_type` is 1 (Unix symlink),
    /// 2 (Windows symlink), 3 (Windows junction), 4 (hardlink) or
    /// 5 (file copy) and `target` is the referenced member name.
    pub fn add_redirect(&mut self, name: &str, redir_type: u64, target: &str) -> RarResult<()> {
        if self.mode != Mode::Write && self.mode != Mode::Append {
            return Err(RarError::Format(
                "add_redirect requires an archive being written".into(),
            ));
        }
        self.reset_solid_chain();
        let fh = FileHeader {
            name: name.replace('\\', "/"),
            unpacked_size: 0,
            packed_size: 0,
            crc32_val: Some(0),
            file_flags: FILE_FLAG_CRC32,
            extra_data: redirect_extra_bytes(redir_type, target),
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

    /// Add a directory entry only (no recursion).
    ///
    /// Writes the directory header without traversing children. Callers that
    /// enumerate files themselves (e.g. with exclusion filtering) use this to
    /// keep empty directories and the directory structure in the archive.
    pub fn add_directory_only(&mut self, path: impl AsRef<Path>, arcname: &str) -> RarResult<()> {
        self.check_cancel()?;
        let path = path.as_ref();
        self.reset_solid_chain();
        let name = arcname.replace('\\', "/").trim_end_matches('/').to_string();

        let meta = fs::metadata(path)?;
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        if self.rar4 {
            let mtime_ns = meta
                .modified()
                .unwrap_or(SystemTime::now())
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            return self.write_rar4_dir_entry(&name, mtime, mtime_ns);
        }

        #[cfg(unix)]
        let attrs = {
            use std::os::unix::fs::MetadataExt;
            meta.mode() as u64
        };
        #[cfg(not(unix))]
        let attrs = 0o040755u64;

        let fh = FileHeader {
            name: format!("{name}/"),
            attributes: attrs,
            mtime,
            host_os: OS_UNIX,
            file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_DIRECTORY,
            is_directory: true,
            ..Default::default()
        };

        let hdr_bytes = fh.to_bytes();
        self.write_block_header(&hdr_bytes)?;
        self.write_ctx_mut().volume_bytes_written +=
            self.on_disk_header_len(hdr_bytes.len() as u64);
        self.entries.push(ArchiveEntry {
            header: fh,
            chunks: Vec::new(),
        });

        Ok(())
    }

    /// Write one RAR4 directory FILE_HEAD member (WinRAR convention: zero
    /// packed/unpacked sizes, CRC 0, `attr = 0x10`, `unp_ver 20`, name
    /// without a trailing slash; directories carry no data payload).
    fn write_rar4_dir_entry(
        &mut self,
        name: &str,
        mtime_secs: u32,
        mtime_ns: u32,
    ) -> RarResult<()> {
        use crate::rar40::write::{
            FileHeaderParams, build_ext_time, build_file_header, encode_file_name, unix_to_dos_time,
        };
        let (encoded_name, name_flags) = encode_file_name(name);
        let ext_time = build_ext_time(Some(mtime_ns));
        let mut flags = name_flags;
        if ext_time.is_some() {
            flags |= crate::rar40::FHD_EXTTIME;
        }
        let params = FileHeaderParams {
            flags,
            packed_size: 0,
            unpacked_size: 0,
            host_os: 0,
            file_crc: 0,
            file_time: unix_to_dos_time(mtime_secs),
            unp_ver: 20,
            method: crate::rar40::RAR4_METHOD_STORE,
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
        // 7-byte end-of-archive block (same rule as file members).
        if let Some(volume_size) = self.write_ctx().volume_size {
            loop {
                let used = self.write_ctx().volume_bytes_written;
                if volume_size.saturating_sub(used) > 7 + hdr.len() as u64 {
                    break;
                }
                self.start_next_volume()?;
            }
        }
        let stream = self.stream.as_mut().unwrap();
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
        let name = name.replace('\\', "/").trim_end_matches('/').to_string();

        let meta = fs::metadata(path)?;
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        if self.rar4 {
            let mtime_ns = meta
                .modified()
                .unwrap_or(SystemTime::now())
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            self.write_rar4_dir_entry(&name, mtime, mtime_ns)?;
        } else {
            #[cfg(unix)]
            let attrs = {
                use std::os::unix::fs::MetadataExt;
                meta.mode() as u64
            };
            #[cfg(not(unix))]
            let attrs = 0o040755u64;

            let fh = FileHeader {
                name: format!("{name}/"),
                attributes: attrs,
                mtime,
                host_os: OS_UNIX,
                file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_DIRECTORY,
                is_directory: true,
                ..Default::default()
            };

            let hdr_bytes = fh.to_bytes();
            self.write_block_header(&hdr_bytes)?;
            self.write_ctx_mut().volume_bytes_written +=
                self.on_disk_header_len(hdr_bytes.len() as u64);
            self.entries.push(ArchiveEntry {
                header: fh,
                chunks: Vec::new(),
            });
        }

        if recursive {
            let mut children: Vec<_> = fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
            children.sort_by_key(|e| e.file_name());

            for child in children {
                let child_path = child.path();
                let child_name = if name.is_empty() {
                    child.file_name().to_string_lossy().into_owned()
                } else {
                    format!("{name}/{}", child.file_name().to_string_lossy())
                };
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
        self.check_cancel()?;
        let name = arcname.replace('\\', "/");
        let plain_crc = {
            let mut h = crc32fast::Hasher::new();
            h.update(data);
            h.finalize()
        };
        let plain_blake = if self.write_ctx().blake2 {
            Some(crate::rar50::blake2sp::hash(data))
        } else {
            None
        };
        let mtime = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        let method = level_to_method(compression_level);
        self.report_progress(0, data.len() as u64);
        if method == COMP_METHOD_STORE || sample_is_incompressible(data, method) {
            self.reset_solid_chain();
            let (header_crc, extra_data, stored_hash, encr_params) =
                RarArchive::payload_extra_and_crc(
                    self.password.as_deref(),
                    plain_crc,
                    plain_blake,
                )?;
            let packed_data = RarArchive::encrypt_payload_with(
                self.password.as_deref(),
                encr_params.as_ref(),
                data,
            )?;
            self.write_file_entry(
                &name,
                data.len() as u64,
                &packed_data,
                header_crc,
                COMP_METHOD_STORE,
                0,
                None,
                &extra_data,
                0o100644,
                mtime,
                false,
                stored_hash,
            )?;
        } else {
            let (dsl, dict_bytes) = dict_params_for(
                data.len(),
                self.write_ctx().dict_size_log,
                self.write_ctx().dict_size_bytes,
                method,
                self.write_ctx().force_v70,
            );
            // WinRAR `-se`: reset the solid statistics when the extension changes.
            self.maybe_reset_solid_for_extension(&name);
            let chain_solid =
                self.write_ctx().solid_mode && self.write_ctx().encoder_state.is_some();
            if self.write_ctx().solid_mode {
                self.write_ctx_mut()
                    .encoder_state
                    .get_or_insert_with(Default::default);
            }
            let shared = self.progress.clone();
            let member = self.progress_member;
            let mut cb = move |done: u64, total: u64| {
                if let Some(shared) = &shared {
                    shared
                        .lock()
                        .expect("progress lock")
                        .report(member, done, total);
                }
            };
            let progress: Option<&mut dyn FnMut(u64, u64)> = Some(&mut cb);
            let packed = lzss_huff::encode_chunked(
                data,
                lzss_huff::EncodeOptions {
                    chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                    state: self.write_ctx_mut().encoder_state.as_mut(),
                    is_final: true,
                    variant: crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                    progress,
                    ..lzss_huff::EncodeOptions::new(method, dsl)
                },
            )?;
            if packed.len() >= data.len() {
                self.reset_solid_chain();
                let (header_crc, extra_data, stored_hash, encr_params) =
                    RarArchive::payload_extra_and_crc(
                        self.password.as_deref(),
                        plain_crc,
                        plain_blake,
                    )?;
                let packed_data = RarArchive::encrypt_payload_with(
                    self.password.as_deref(),
                    encr_params.as_ref(),
                    data,
                )?;
                self.write_file_entry(
                    &name,
                    data.len() as u64,
                    &packed_data,
                    header_crc,
                    COMP_METHOD_STORE,
                    0,
                    None,
                    &extra_data,
                    0o100644,
                    mtime,
                    false,
                    stored_hash,
                )?;
            } else {
                let (header_crc, extra_data, stored_hash, encr_params) =
                    RarArchive::payload_extra_and_crc(
                        self.password.as_deref(),
                        plain_crc,
                        plain_blake,
                    )?;
                let packed_data = RarArchive::encrypt_payload_with(
                    self.password.as_deref(),
                    encr_params.as_ref(),
                    &packed,
                )?;
                self.write_file_entry(
                    &name,
                    data.len() as u64,
                    &packed_data,
                    header_crc,
                    method,
                    dsl,
                    dict_bytes,
                    &extra_data,
                    0o100644,
                    mtime,
                    chain_solid,
                    stored_hash,
                )?;
            }
        }

        self.report_progress(data.len() as u64, data.len() as u64);

        Ok(())
    }

    // ── Batch addition ─────────────────────────────────────────────────────

    /// Add multiple entries at once.
    ///
    /// With the `parallel` feature enabled this compresses eligible
    /// members (non-solid archives, file/bytes entries up to 64 MiB) in
    /// Rayon waves and writes them in the original order. For unencrypted
    /// archives the resulting archive is byte-identical to calling the
    /// individual `add*` methods sequentially; files over
    /// [`PARALLEL_COMPRESS_MAX_MEMBER`] are compressed in parallel chunks
    /// (non-solid only), while directories and solid archives fall back to
    /// the sequential path. Without the feature this is a plain sequential
    /// loop over the same `add*` calls.
    pub fn add_batch(&mut self, entries: &[BatchEntry<'_>]) -> RarResult<()> {
        self.check_cancel()?;
        #[cfg(feature = "parallel")]
        {
            if !self.rar4 && !self.write_ctx().solid_mode && !entries.is_empty() {
                return self.add_batch_parallel(entries);
            }
        }
        self.progress_set_batch_total(entries)?;
        for (i, entry) in entries.iter().enumerate() {
            self.progress_member = i;
            self.add_batch_entry_sequential(entry)?;
        }
        Ok(())
    }

    /// Sum every member's input size so the progress denominator covers the
    /// whole batch (parallel waves and sequential members alike).
    fn progress_set_batch_total(&mut self, entries: &[BatchEntry<'_>]) -> RarResult<()> {
        let mut total = 0u64;
        for e in entries {
            let size = match e {
                BatchEntry::Bytes { data, .. } => data.len() as u64,
                BatchEntry::File { path, .. } => fs::metadata(path)?.len(),
                BatchEntry::Directory { .. } => 0,
            };
            total = total.saturating_add(size);
        }
        if let Some(progress) = &self.progress {
            progress.lock().expect("progress lock").set_total(total);
        }
        Ok(())
    }

    fn add_batch_entry_sequential(&mut self, entry: &BatchEntry<'_>) -> RarResult<()> {
        self.check_cancel()?;
        match *entry {
            BatchEntry::Bytes { name, data, level } => self.add_bytes(name, data, level),
            BatchEntry::File { path, name, level } => match name {
                Some(name) => self.add_as(path, name, level),
                None => self.add(path, level),
            },
            BatchEntry::Directory { path, name } => {
                let name = match name {
                    Some(name) => name.to_string(),
                    None => path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                };
                self.add_directory_only(path, &name)
            }
        }
    }

    #[cfg(feature = "parallel")]
    fn add_batch_parallel(&mut self, entries: &[BatchEntry<'_>]) -> RarResult<()> {
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
            blake2: self.write_ctx().blake2,
            dict_size_log: self.write_ctx().dict_size_log,
            dict_size_bytes: self.write_ctx().dict_size_bytes,
            force_v70: self.write_ctx().force_v70,
            save_ctime: self.write_ctx().save_ctime,
            save_atime: self.write_ctx().save_atime,
            save_mtime: self.write_ctx().save_mtime,
            save_owner: self.write_ctx().save_owner,
            time_precision_seconds: self.write_ctx().time_precision_seconds,
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
            Some(crate::rar50::blake2sp::hash(data))
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
            match lzss_huff::encode_with_auto_delta_filter(
                data,
                method,
                dsl,
                crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                ctx.threads,
                cancel_ref,
            )? {
                Some(filtered) if filtered.len() < data.len() => filtered,
                _ => match lzss_huff::encode_with_auto_x86_filter(
                    data,
                    method,
                    dsl,
                    crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
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
                },
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

        let name = arcname
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().into_owned());
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

    /// Write a file entry, splitting across volumes if needed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_file_entry(
        &mut self,
        name: &str,
        unpacked_size: u64,
        packed_data: &[u8],
        file_crc: u32,
        method: u8,
        dict_size_log: u8,
        dict_size_bytes: Option<u64>,
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
            dict_size_bytes,
            host_os: OS_UNIX,
            file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
            extra_data: extra_data.to_vec(),
            ..Default::default()
        };

        if self.write_ctx().volume_size.is_none() {
            // Single-volume
            let hdr_bytes = fh_base.to_bytes();
            if self.write_ctx().quick_open {
                let pos = self.stream.as_mut().unwrap().stream_position()?;
                self.write_ctx_mut()
                    .quick_open_entries
                    .push((pos, hdr_bytes.clone()));
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
        let volume_size = self.write_ctx().volume_size.unwrap();
        // End-of-archive block: 8 plaintext bytes, or `[IV][padded]` when
        // header encryption wraps every block.
        let eoa_plain: u64 = 8;
        let eoa_size: u64 = self.on_disk_header_len(eoa_plain);
        let total_packed = packed_data.len() as u64;

        // Check if it fits in current volume
        let hdr_bytes = fh_base.to_bytes();
        let hdr_on_disk = self.on_disk_header_len(hdr_bytes.len() as u64);
        let total_needed = hdr_on_disk + total_packed + eoa_size;
        let remaining = volume_size.saturating_sub(self.write_ctx().volume_bytes_written);

        if total_needed <= remaining {
            // Fits entirely
            self.write_block_header(&hdr_bytes)?;
            let stream = self.stream.as_mut().unwrap();
            stream.write_all(packed_data)?;
            let data_offset = stream.stream_position()? - total_packed;
            self.write_ctx_mut().volume_bytes_written += hdr_on_disk + total_packed;
            let chunk = DataChunk {
                volume_index: self.write_ctx().current_volume - 1,
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

        // Need to split across volumes.
        let params = SplitParams {
            name,
            unpacked_size,
            attrs,
            mtime,
            method,
            solid,
            dict_size_log,
            dict_size_bytes,
            extra_data,
        };
        self.write_split_member(
            total_packed,
            params,
            volume_size,
            eoa_size,
            fh_base,
            |this, phase, offset, chunk_size, is_last| match phase {
                SplitPhase::Crc => {
                    if is_last {
                        Ok(file_crc as u64)
                    } else {
                        let chunk_packed =
                            &packed_data[offset as usize..(offset + chunk_size) as usize];
                        let mut h = crc32fast::Hasher::new();
                        h.update(chunk_packed);
                        Ok(h.finalize() as u64)
                    }
                }
                SplitPhase::Write => {
                    let chunk_packed =
                        &packed_data[offset as usize..(offset + chunk_size) as usize];
                    let stream = this.stream.as_mut().unwrap();
                    stream.write_all(chunk_packed)?;
                    let data_offset = stream.stream_position()? - chunk_size;
                    Ok(data_offset)
                }
            },
        )
    }

    /// Drive the shared multi-volume split loop for a member whose packed
    /// payload must cross volume boundaries. The budget arithmetic, per-chunk
    /// header estimation, `chunk_extra` selection, volume transitions and the
    /// collected chunk bookkeeping live here once. Only the source-specific
    /// step is delegated: `phase` is invoked once with [SplitPhase::Crc] to
    /// compute the chunk's checksum (a probe pass for streamed payloads, a
    /// slice hash for in-memory data) before the block header is emitted, and
    /// again with [SplitPhase::Write] to write the chunk's bytes after the
    /// header and return its `data_offset` — preserving the on-disk
    /// [header][data] ordering.
    #[allow(clippy::too_many_arguments)]
    fn write_split_member(
        &mut self,
        total_packed: u64,
        params: SplitParams<'_>,
        volume_size: u64,
        eoa_size: u64,
        fh_base: FileHeader,
        mut phase: impl FnMut(&mut RarArchive, SplitPhase, u64, u64, bool) -> RarResult<u64>,
    ) -> RarResult<()> {
        let mut offset = 0u64;
        let mut chunks = Vec::new();
        let mut is_first = true;

        // Encrypted members: every chunk header carries the encryption
        // extra record (WinRAR repeats it on every volume). Non-final
        // chunks verify with a plain crc32 of the ciphertext chunk, so
        // their record must clear the hash-key MAC bit (flags=1); the
        // final chunk keeps the full record (flags=3, MAC'd checksum).
        let encr_params = if self.password.is_some() {
            crypto::parse_encryption_extra(params.extra_data)?
        } else {
            None
        };
        let chunk_extra = |is_last: bool, is_first: bool| -> Vec<u8> {
            if let Some(ref p) = encr_params {
                if is_last {
                    params.extra_data.to_vec()
                } else {
                    let mut np = p.clone();
                    np.flags &= !0x02;
                    np.to_extra_bytes()
                }
            } else if is_first {
                params.extra_data.to_vec()
            } else {
                Vec::new()
            }
        };

        while offset < total_packed {
            self.check_cancel()?;
            let remaining_vol = volume_size.saturating_sub(self.write_ctx().volume_bytes_written);

            // Build chunk flags
            let mut block_flags: u64 = 0;
            if !is_first {
                block_flags |= BLOCK_FLAG_DATA_CONTINUES;
            }

            // Estimate header size
            let chunk_fh = FileHeader {
                name: params.name.to_string(),
                unpacked_size: params.unpacked_size,
                packed_size: remaining_vol.max(1),
                attributes: params.attrs,
                mtime: params.mtime,
                crc32_val: Some(0),
                comp_method: params.method,
                comp_solid: params.solid,
                comp_dict_size: params.dict_size_log,
                dict_size_bytes: params.dict_size_bytes,
                host_os: OS_UNIX,
                flags: block_flags | BLOCK_FLAG_DATA_CONTINUE_TO,
                file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
                extra_data: chunk_extra(false, is_first),
                ..Default::default()
            };
            let hdr_size = self.on_disk_header_len(chunk_fh.to_bytes().len() as u64);

            let bytes_for_data = remaining_vol.saturating_sub(hdr_size + eoa_size);
            if bytes_for_data == 0 {
                self.start_next_volume()?;
                is_first = false;
                continue;
            }

            let chunk_size = bytes_for_data.min(total_packed - offset);
            let is_last = offset + chunk_size >= total_packed;

            // Set final flags
            if is_last {
                block_flags &= !BLOCK_FLAG_DATA_CONTINUE_TO;
            } else {
                block_flags |= BLOCK_FLAG_DATA_CONTINUE_TO;
            }

            let chunk_crc = phase(self, SplitPhase::Crc, offset, chunk_size, is_last)? as u32;

            let final_fh = FileHeader {
                name: params.name.to_string(),
                unpacked_size: params.unpacked_size,
                packed_size: chunk_size,
                attributes: params.attrs,
                mtime: params.mtime,
                crc32_val: Some(chunk_crc),
                comp_method: params.method,
                comp_solid: params.solid,
                comp_dict_size: params.dict_size_log,
                dict_size_bytes: params.dict_size_bytes,
                host_os: OS_UNIX,
                flags: block_flags,
                file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
                extra_data: chunk_extra(is_last, is_first),
                ..Default::default()
            };

            let final_hdr = final_fh.to_bytes();
            let final_hdr_disk = self.on_disk_header_len(final_hdr.len() as u64);
            self.write_block_header(&final_hdr)?;
            let data_offset = phase(self, SplitPhase::Write, offset, chunk_size, is_last)?;
            self.write_ctx_mut().volume_bytes_written += final_hdr_disk + chunk_size;

            chunks.push(DataChunk {
                volume_index: self.write_ctx().current_volume - 1,
                data_offset,
                packed_size: chunk_size,
                crc32_val: Some(chunk_crc),
                is_final: is_last,
                extra_data: if is_first {
                    params.extra_data.to_vec()
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
        self.write_ctx_mut().encoder_state = None;
        self.write_ctx_mut().rar4_solid_encoder = None;
        self.write_ctx_mut().rar4_solid_run_has_member = false;
        self.write_ctx_mut().last_solid_ext = None;
    }

    /// Reset the solid chain when the next member's file extension differs
    /// from the previous one (WinRAR `-se`). No-op unless solid mode is on
    /// and `solid_reset` is `PerExtension`. Directories and STORE members
    /// break the chain through `reset_solid_chain`, which also clears
    /// `last_solid_ext`, so this only needs to run for compressed members.
    pub(crate) fn maybe_reset_solid_for_extension(&mut self, name: &str) {
        if !self.write_ctx().solid_mode
            || self.write_ctx().solid_reset != crate::options::SolidReset::PerExtension
        {
            return;
        }
        let base = name.trim_end_matches('/');
        let ext = base.rsplit('.').next().unwrap_or("");
        match &self.write_ctx().last_solid_ext {
            Some(prev) if prev == ext => {}
            _ => {
                self.write_ctx_mut().encoder_state = None;
                self.write_ctx_mut().rar4_solid_encoder = None;
                self.write_ctx_mut().rar4_solid_run_has_member = false;
                self.write_ctx_mut().last_solid_ext = Some(ext.to_string());
            }
        }
    }

    /// Build the header CRC, extra-area records (encryption + BLAKE2sp)
    /// and stored hash value for a member, plus the encryption parameters
    /// to reuse for the actual payload encryption (one KDF/salt per
    /// member). For encrypted members the checksums are MAC'd with the
    /// hash key, matching WinRAR.
    #[allow(clippy::type_complexity)]
    pub(crate) fn payload_extra_and_crc(
        password: Option<&str>,
        plain_crc: u32,
        plain_blake: Option<[u8; 32]>,
    ) -> RarResult<(
        u32,
        Vec<u8>,
        Option<[u8; 32]>,
        Option<crypto::EncryptionParams>,
    )> {
        if let Some(password) = password {
            let params =
                crypto::EncryptionParams::generate_for_password(password, ENCR_PBKDF2_ITER_LOG);
            let header_crc = params.mac_crc32(plain_crc, password)?;
            let stored_hash = match plain_blake {
                Some(h) => Some(params.mac_hash32(h, password)?),
                None => None,
            };
            let mut extra = params.to_extra_bytes();
            if let Some(h) = stored_hash {
                extra.extend(crate::rar50::headers::hash_extra_record(h));
            }
            Ok((header_crc, extra, stored_hash, Some(params)))
        } else {
            let mut extra = Vec::new();
            if let Some(h) = plain_blake {
                extra.extend(crate::rar50::headers::hash_extra_record(h));
            }
            Ok((plain_crc, extra, plain_blake, None))
        }
    }

    /// Encrypt a member payload with the parameters returned by
    /// [`Self::payload_extra_and_crc`] (must match the member's stored
    /// salt).
    pub(crate) fn encrypt_payload_with(
        password: Option<&str>,
        params: Option<&crypto::EncryptionParams>,
        plaintext: &[u8],
    ) -> RarResult<Vec<u8>> {
        match (password, params) {
            (Some(password), Some(params)) => params.encrypt(plaintext, password),
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
    /// Progress is reported per chunk (`bytes_written, unpacked_size`),
    /// matching the historical streaming behavior.
    #[allow(clippy::too_many_arguments)]
    fn write_stored_file(
        &mut self,
        name: &str,
        unpacked_size: u64,
        file_crc: u32,
        attrs: u64,
        mtime: u32,
        reader: &mut File,
        extra_data: &[u8],
        hash_value: Option<[u8; 32]>,
    ) -> RarResult<()> {
        self.write_streamed_payload(
            name,
            unpacked_size,
            unpacked_size,
            file_crc,
            attrs,
            mtime,
            COMP_METHOD_STORE,
            0,
            None,
            extra_data,
            hash_value,
            false,
            reader,
            unpacked_size,
            None,
            None,
            true,
        )
    }

    /// Stream a member payload (compressed data or STORE bytes) from a
    /// seekable reader into the archive with bounded memory, splitting
    /// across volumes when needed.
    ///
    /// When `encr` is set the payload is AES-256-CBC encrypted on the fly
    /// (the IV chain carries across chunk boundaries), the header checksum
    /// is MAC'd, and non-final volume chunks carry the CRC32 of their
    /// on-disk ciphertext bytes — matching WinRAR's per-volume records.
    /// `plain_len` is the number of plaintext bytes available in `reader`;
    /// `packed_size` is the total on-disk data size (the zero-padded
    /// ciphertext length when encrypted). `progress` enables per-chunk
    /// progress callbacks (historical STORE-path behavior); the compressed
    /// path reports progress during its compression pass instead.
    #[allow(clippy::too_many_arguments)]
    fn write_streamed_payload(
        &mut self,
        name: &str,
        unpacked_size: u64,
        packed_size: u64,
        file_crc: u32,
        attrs: u64,
        mtime: u32,
        method: u8,
        dict_size_log: u8,
        dict_size_bytes: Option<u64>,
        extra_data: &[u8],
        hash_value: Option<[u8; 32]>,
        solid: bool,
        reader: &mut File,
        plain_len: u64,
        encr: Option<&crypto::EncryptionParams>,
        password: Option<&str>,
        progress: bool,
    ) -> RarResult<()> {
        let fh_base = FileHeader {
            name: name.to_string(),
            unpacked_size,
            packed_size,
            attributes: attrs,
            mtime,
            crc32_val: Some(file_crc),
            hash_type: if hash_value.is_some() { 0 } else { u8::MAX },
            hash_value,
            comp_method: method,
            comp_solid: solid,
            comp_dict_size: dict_size_log,
            dict_size_bytes,
            host_os: OS_UNIX,
            file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
            extra_data: extra_data.to_vec(),
            ..Default::default()
        };

        // Derive the AES key once per member. Two independent encryptors
        // are seeded with it: the probe pass (chunk CRC over the on-disk
        // ciphertext) and the write pass must produce identical bytes, so
        // they run separate chains from the same key and IV.
        let key_iv = match (encr, password) {
            (Some(params), Some(password)) => Some((params.get_key(password)?, params.iv)),
            (None, None) => None,
            _ => {
                return Err(RarError::Format(
                    "internal error: encryption parameters mismatch".into(),
                ));
            }
        };
        let mut write_src = payload_stream(&key_iv);
        let mut probe_src = payload_stream(&key_iv);

        if self.write_ctx().volume_size.is_none() {
            // ── Single-volume ──
            let hdr_bytes = fh_base.to_bytes();
            if self.write_ctx().quick_open {
                let pos = self.stream.as_mut().unwrap().stream_position()?;
                self.write_ctx_mut()
                    .quick_open_entries
                    .push((pos, hdr_bytes.clone()));
            }
            self.write_block_header(&hdr_bytes)?;
            let written = {
                let stream = self.stream.as_mut().unwrap();
                if progress {
                    let mut sink = ProgressWriter {
                        inner: stream,
                        total: unpacked_size,
                        written: 0,
                        member: self.progress_member,
                        progress: self.progress.clone(),
                    };
                    write_src.emit_to(reader, plain_len, 0, packed_size, &mut sink)?;
                    sink.written
                } else {
                    let mut counting = CountingWriter::new(stream);
                    write_src.emit_to(reader, plain_len, 0, packed_size, &mut counting)?;
                    counting.written()
                }
            };
            if written != packed_size {
                return Err(RarError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "file changed size while being archived: expected {packed_size} bytes, wrote {written}"
                    ),
                )));
            }
            let stream = self.stream.as_mut().unwrap();
            let data_offset = stream.stream_position()? - packed_size;
            self.entries.push(ArchiveEntry {
                header: FileHeader {
                    data_offset,
                    ..fh_base
                },
                chunks: vec![DataChunk {
                    volume_index: 0,
                    data_offset,
                    packed_size,
                    crc32_val: Some(file_crc),
                    is_final: true,
                    extra_data: extra_data.to_vec(),
                }],
            });
            return Ok(());
        }

        // ── Multi-volume splitting ──
        let volume_size = self.write_ctx().volume_size.unwrap();
        // End-of-archive block: 8 plaintext bytes, or `[IV][padded]` when
        // header encryption wraps every block.
        let eoa_size: u64 = self.on_disk_header_len(8);
        let params = SplitParams {
            name,
            unpacked_size,
            attrs,
            mtime,
            method,
            solid,
            dict_size_log,
            dict_size_bytes,
            extra_data,
        };
        self.write_split_member(
            packed_size,
            params,
            volume_size,
            eoa_size,
            fh_base,
            |this, phase, offset, chunk_size, is_last| match phase {
                SplitPhase::Crc => {
                    // For non-final chunks the header carries the CRC of this
                    // chunk's on-disk bytes (the ciphertext when encrypted),
                    // computed in a probe pass with an independent encryptor
                    // chain so the write pass below is not disturbed.
                    if is_last {
                        Ok(file_crc as u64)
                    } else {
                        let mut h = crc32fast::Hasher::new();
                        let mut sink = CrcSink(&mut h);
                        probe_src.emit_to(
                            reader,
                            plain_len,
                            offset,
                            offset + chunk_size,
                            &mut sink,
                        )?;
                        Ok(h.finalize() as u64)
                    }
                }
                SplitPhase::Write => {
                    {
                        let stream = this.stream.as_mut().unwrap();
                        if progress {
                            let mut sink = ProgressWriter {
                                inner: stream,
                                total: unpacked_size,
                                written: offset,
                                member: this.progress_member,
                                progress: this.progress.clone(),
                            };
                            write_src.emit_to(
                                reader,
                                plain_len,
                                offset,
                                offset + chunk_size,
                                &mut sink,
                            )?;
                        } else {
                            write_src.emit_to(
                                reader,
                                plain_len,
                                offset,
                                offset + chunk_size,
                                &mut *stream,
                            )?;
                        }
                    }
                    let stream = this.stream.as_mut().unwrap();
                    let data_offset = stream.stream_position()? - chunk_size;
                    Ok(data_offset)
                }
            },
        )
    }

    /// Stream a STORE member directly from disk (bounded memory),
    /// encrypting on the fly when a password is set.
    #[allow(clippy::too_many_arguments)]
    fn write_store_member(
        &mut self,
        path: &Path,
        name: &str,
        file_size: u64,
        header_crc: u32,
        extra_data: &[u8],
        stored_hash: Option<[u8; 32]>,
        encr_params: Option<&crypto::EncryptionParams>,
        attrs: u64,
        mtime: u32,
        dict_size_bytes: Option<u64>,
    ) -> RarResult<()> {
        let mut reader = File::open(path)?;
        let password = self.password.clone();
        match (password.as_deref(), encr_params) {
            (Some(password), Some(params)) => self.write_streamed_payload(
                name,
                file_size,
                crypto::zero_padded_len(file_size),
                header_crc,
                attrs,
                mtime,
                COMP_METHOD_STORE,
                0,
                dict_size_bytes,
                extra_data,
                stored_hash,
                false,
                &mut reader,
                file_size,
                Some(params),
                Some(password),
                false,
            ),
            (None, None) => self.write_stored_file(
                name,
                file_size,
                header_crc,
                attrs,
                mtime,
                &mut reader,
                extra_data,
                stored_hash,
            ),
            _ => Err(RarError::Format(
                "internal error: encryption parameters mismatch".into(),
            )),
        }
    }

    /// Compress a large file (≥ [`STREAM_COMPRESS_THRESHOLD`]) with bounded
    /// memory: the input is read and compressed in bounded chunks (with the
    /// persistent encoder state), spilling the compressed stream to a
    /// temporary file; once the packed size and plaintext checksums are
    /// known, the member header is written and the spill is streamed into
    /// the archive — encrypting on the fly when a password is set. Falls
    /// back to streaming STORE when compression does not shrink the
    /// payload.
    #[allow(clippy::too_many_arguments)]
    fn add_file_streaming(
        &mut self,
        path: &Path,
        name: &str,
        file_size: u64,
        attrs: u64,
        mtime: u32,
        time_extra: Option<Vec<u8>>,
        owner_extra: Option<Vec<u8>>,
        method: u8,
        dsl: u8,
        dict_bytes: Option<u64>,
    ) -> RarResult<()> {
        // A persistent encoder state carries the LZ window (tail and the
        // long-range match history) across chunks of one member — even in
        // non-solid archives, where the window is reset between members.
        // This is what makes >64 KiB match distances (WinRAR `-mcl`
        // long range search) work for large files.
        // WinRAR `-se`: reset the solid statistics when the extension changes.
        self.maybe_reset_solid_for_extension(name);
        let chain_solid = self.write_ctx().solid_mode && self.write_ctx().encoder_state.is_some();
        self.write_ctx_mut()
            .encoder_state
            .get_or_insert_with(Default::default);

        let mut crc_hasher = crc32fast::Hasher::new();
        let mut blake_hasher = if self.write_ctx().blake2 {
            Some(crate::rar50::blake2sp::Hasher::new())
        } else {
            None
        };
        let mut bytes_read = 0u64;
        let mut packed_size = 0u64;
        let threads = self.effective_threads();
        let cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> =
            self.cancel.clone();
        let cancel_ref = cancel_flag.as_deref();
        let spill_path = spill_path_for(&self.path);
        let _spill_guard = SpillGuard(spill_path.clone());
        {
            let mut spill = File::create(&spill_path)?;

            /// Encode one buffered window to the spill file. With the
            /// `parallel` feature and enough data, the window is split
            /// across the compression pool's workers; otherwise it falls
            /// back to the byte-for-byte sequential chunk loop.
            #[allow(clippy::too_many_arguments)]
            fn flush_window(
                work: &mut Vec<u8>,
                is_final: bool,
                chain_solid: bool,
                threads: usize,
                method: u8,
                dsl: u8,
                dict_bytes: Option<u64>,
                state: &mut Option<crate::codec::EncoderState>,
                spill: &mut File,
                packed_size: &mut u64,
                cancel: Option<&std::sync::atomic::AtomicBool>,
            ) -> RarResult<()> {
                if work.is_empty() {
                    return Ok(());
                }
                #[cfg(not(feature = "parallel"))]
                let _ = (chain_solid, threads);
                #[cfg(feature = "parallel")]
                const MT_MIN: usize = 3 * crate::codec::DEFAULT_CHUNK_SIZE;
                #[cfg(feature = "parallel")]
                if !chain_solid && work.len() >= MT_MIN && threads > 1 {
                    let packed = crate::codec::lzss_huff::encode_chunked_mt_with_progress(
                        work,
                        method,
                        dsl,
                        crate::codec::DEFAULT_CHUNK_SIZE,
                        state.get_or_insert_with(Default::default),
                        threads,
                        is_final,
                        crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                        None,
                        None,
                        cancel,
                    )?;
                    spill.write_all(&packed)?;
                    *packed_size += packed.len() as u64;
                    work.clear();
                    return Ok(());
                }
                let mut offset = 0usize;
                while offset < work.len() {
                    if cancel.is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed)) {
                        return Err(RarError::Cancelled);
                    }
                    let end = (offset + crate::codec::DEFAULT_CHUNK_SIZE).min(work.len());
                    let compressed = lzss_huff::encode_chunked(
                        &work[offset..end],
                        lzss_huff::EncodeOptions {
                            chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                            state: state.as_mut(),
                            is_final: is_final && end >= work.len(),
                            variant: crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                            ..lzss_huff::EncodeOptions::new(method, dsl)
                        },
                    )?;
                    spill.write_all(&compressed)?;
                    *packed_size += compressed.len() as u64;
                    offset = end;
                }
                work.clear();
                Ok(())
            }

            #[cfg(feature = "parallel")]
            let mt_window =
                (threads.max(2) * 8 * 1024 * 1024).clamp(24 * 1024 * 1024, 64 * 1024 * 1024);
            #[cfg(not(feature = "parallel"))]
            let mt_window = 0usize;

            let mut work: Vec<u8> = Vec::new();
            let mut eof = false;
            let mut file = io::BufReader::with_capacity(1 << 20, File::open(path)?);
            let mut buf = vec![0u8; crate::codec::DEFAULT_CHUNK_SIZE];
            while !eof {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    eof = true;
                } else {
                    bytes_read += n as u64;
                    crc_hasher.update(&buf[..n]);
                    if let Some(h) = blake_hasher.as_mut() {
                        h.update(&buf[..n]);
                    }
                    work.extend_from_slice(&buf[..n]);
                    self.report_progress(bytes_read, file_size);
                }
                if eof || work.len() >= mt_window {
                    flush_window(
                        &mut work,
                        eof,
                        chain_solid,
                        threads,
                        method,
                        dsl,
                        dict_bytes,
                        &mut self.write_ctx_mut().encoder_state,
                        &mut spill,
                        &mut packed_size,
                        cancel_ref,
                    )?;
                    if packed_size >= file_size {
                        break;
                    }
                }
            }
        }

        let plain_crc = crc_hasher.finalize();
        let plain_blake = blake_hasher.map(|h| h.finalize());

        if packed_size >= file_size {
            // Compression is a net loss: fall back to streaming STORE.
            self.reset_solid_chain();
            let (header_crc, mut extra_data, stored_hash, encr_params) =
                RarArchive::payload_extra_and_crc(
                    self.password.as_deref(),
                    plain_crc,
                    plain_blake,
                )?;
            if let Some(ref t) = time_extra {
                extra_data.extend_from_slice(t);
            }
            if let Some(ref t) = owner_extra {
                extra_data.extend_from_slice(t);
            }
            self.write_store_member(
                path,
                name,
                file_size,
                header_crc,
                &extra_data,
                stored_hash,
                encr_params.as_ref(),
                attrs,
                mtime,
                dict_bytes,
            )?;
            self.report_progress(file_size, file_size);
            return Ok(());
        }

        let (header_crc, mut extra_data, stored_hash, encr_params) =
            RarArchive::payload_extra_and_crc(self.password.as_deref(), plain_crc, plain_blake)?;
        if let Some(ref t) = time_extra {
            extra_data.extend_from_slice(t);
        }
        if let Some(ref t) = owner_extra {
            extra_data.extend_from_slice(t);
        }
        let mut spill = File::open(&spill_path)?;
        let password = self.password.clone();
        // Encrypted members store the zero-padded ciphertext length in
        // the header and on disk (the streaming encryptor pads the final
        // partial block); plain members store the packed length as-is.
        let (packed_size, plain_len) = match encr_params {
            Some(_) => (crypto::zero_padded_len(packed_size), packed_size),
            None => (packed_size, packed_size),
        };
        self.write_streamed_payload(
            name,
            file_size,
            packed_size,
            header_crc,
            attrs,
            mtime,
            method,
            dsl,
            dict_bytes,
            &extra_data,
            stored_hash,
            chain_solid,
            &mut spill,
            plain_len,
            encr_params.as_ref(),
            password.as_deref(),
            false,
        )?;
        self.write_member_streams(path)?;
        // Non-solid members use an independent LZ window: drop the
        // encoder state so the next member starts fresh.
        if !self.write_ctx().solid_mode {
            self.reset_solid_chain();
        }
        self.report_progress(file_size, file_size);
        Ok(())
    }

    /// Write the NTFS alternate data streams of `path` as "STM" service
    /// records right after the member's file block (WinRAR `-os`).
    fn write_member_streams(&mut self, path: &Path) -> RarResult<()> {
        if !self.write_ctx().save_streams {
            return Ok(());
        }
        #[cfg(windows)]
        {
            for stream in windows::enumerate_windows_streams(path)? {
                // FindFirstStreamW returns names like "file:stream:$DATA"
                // (and "::$DATA" for the default stream); normalize to the
                // archive form ":stream".
                let raw_name = stream.0;
                if raw_name == "::$DATA" {
                    continue; // the default unnamed stream
                }
                let trimmed = raw_name.strip_suffix(":$DATA").unwrap_or(&raw_name);
                let stream_part = trimmed.rsplit_once(':').map(|(_, s)| s).unwrap_or(trimmed);
                let name = format!(":{stream_part}");
                // Read the stream payload through the `file:stream` path.
                let mut full = path.as_os_str().to_os_string();
                full.push(&name);
                let data = match std::fs::read(std::path::PathBuf::from(&full)) {
                    Ok(d) => d,
                    Err(_) => continue, // stream vanished mid-run
                };
                let subdata = {
                    let mut extra = Vec::new();
                    extra.extend(vint::encode((1 + name.len()) as u64));
                    extra.extend(vint::encode(crate::rar50::EXTRA_SERVICE_SUBDATA));
                    extra.extend(name.as_bytes());
                    extra
                };
                let hdr = crate::rar50::headers::build_service_block(
                    "STM",
                    &subdata,
                    data.len() as u64,
                    crate::rar50::BLOCK_FLAG_DEPENDS_PREV,
                );
                self.write_block_header(&hdr)?;
                let stream = self.stream.as_mut().unwrap();
                stream.write_all(&data)?;
                self.write_ctx_mut().volume_bytes_written = self
                    .write_ctx()
                    .volume_bytes_written
                    .saturating_add(self.on_disk_header_len(hdr.len() as u64))
                    .saturating_add(data.len() as u64);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = path;
        }
        Ok(())
    }
}
