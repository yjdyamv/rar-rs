//! RAR5 member addition: file/bytes entry points, directory headers, the
//! `-ts`/`-ow` extra-record builders and the redirect writer.
//!
//! The format-neutral dispatchers live in
//! `crate::format::shared::write_ops`; emission lives in [`super::emit`].

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::layout::{
    SAMPLE_PROBE_HEAD, dict_params_for, hash_file, sample_is_incompressible,
    sample_is_incompressible_file,
};
use crate::archive::{ArchiveEntry, Mode, RarArchive, STREAM_COMPRESS_THRESHOLD};
use crate::codec::lzss_huff;
use crate::error::{RarError, RarResult};
#[cfg(unix)]
use crate::format::rar5::headers::build_owner_extra_record;
use crate::format::rar5::headers::{file_time_extra_record, redirect_extra_bytes};
use crate::format::rar5::{
    COMP_METHOD_STORE, FILE_FLAG_CRC32, FILE_FLAG_DIRECTORY, FILE_FLAG_TIME_UNIX, OS_UNIX,
    level_to_method,
};
use crate::format::shared::stream_mut;
use crate::format::shared::write_ops::archive_name_from_path;
use crate::model::FileHeader;

#[cfg(windows)]
use super::windows;

/// Build the FILE_TIME extra record per explicit `-ts` settings (the
/// off-thread parallel batch path has no `&RarArchive`); `None` when no
/// time needs the extra record. On Windows the access/creation times are
/// read through `GetFileTime` (std exposes no access-time API).
#[allow(clippy::too_many_arguments)]
pub(super) fn time_extra_cfg(
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
pub(super) fn owner_extra_cfg(save_owner: bool, meta: &fs::Metadata) -> Option<Vec<u8>> {
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
            self.write_ctx().meta.ctime,
            self.write_ctx().meta.atime,
            self.write_ctx().meta.mtime,
            self.write_ctx().meta.time_precision_seconds,
            meta,
            path,
            mtime,
            mtime_ns,
        )
    }

    /// Build the OWNER extra record (numeric uid/gid) when `-ow` is on;
    /// `None` off-Unix or when disabled.
    fn owner_extra_for(&self, meta: &fs::Metadata) -> Option<Vec<u8>> {
        owner_extra_cfg(self.write_ctx().meta.owner, meta)
    }

    /// Try the automatic delta (multimedia) and then the x86 (E8/E8E9)
    /// filter, returning the packed bytes of whichever the scan found worth
    /// filtering — or `None` when plain LZSS should handle the member.
    fn try_auto_filters(
        &mut self,
        data: &[u8],
        method: u8,
        dsl: u8,
        dict_bytes: Option<u64>,
    ) -> RarResult<Option<Vec<u8>>> {
        let variant = crate::version::ArchiveVersion::from_v70(dict_bytes.is_some());
        let threads = self.effective_threads();
        let cancel = self.cancel.as_deref();
        Ok(
            match lzss_huff::encode_with_auto_delta_filter(
                data, method, dsl, variant, threads, cancel,
            )? {
                Some(f) => Some(f),
                None => lzss_huff::encode_with_auto_x86_filter(
                    data, method, dsl, variant, threads, cancel,
                )?,
            },
        )
    }

    /// RAR5 filesystem-file member writer (the neutral dispatcher lives in
    /// `format::shared::write_ops`).
    pub(crate) fn add_file_rar5(
        &mut self,
        path: &Path,
        arcname: Option<&str>,
        level: u8,
    ) -> RarResult<()> {
        self.check_cancel()?;
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

        let name = match arcname {
            Some(s) => s.to_string(),
            None => archive_name_from_path(path)?,
        };
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
            self.write_ctx().compression.dict_size_log,
            self.write_ctx().compression.dict_size_bytes,
            method,
            self.write_ctx().compression.force_v70,
        );

        if method == COMP_METHOD_STORE || probe_incompressible {
            // STORE is written by streaming the file directly: bounded
            // memory regardless of file size. Encrypted STORE is encrypted
            // on the fly with a chained CBC state (also bounded memory).
            self.reset_solid_chain();
            let (plain_crc, plain_blake) = hash_file(
                path,
                file_size,
                self.write_ctx().meta.blake2,
                self.cancel.as_deref(),
            )?;
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
        let mut blake_hasher = if self.write_ctx().meta.blake2 {
            Some(crate::format::rar5::blake2sp::Hasher::new())
        } else {
            None
        };
        crc_hasher.update(&whole);
        if let Some(h) = blake_hasher.as_mut() {
            h.update(&whole);
        }
        let plain_crc = crc_hasher.finalize();
        let plain_blake = blake_hasher.map(|h| h.finalize());

        // WinRAR `-se`: reset the solid statistics when the extension changes.
        self.maybe_reset_solid_for_extension(&name);
        let chain_solid =
            self.write_ctx().solid.mode && self.write_ctx().solid.encoder_state.is_some();
        self.write_ctx_mut()
            .solid
            .encoder_state
            .get_or_insert_with(Default::default);
        // Each member starts its own frame; see `EncoderState::begin_member`.
        self.write_ctx_mut()
            .solid
            .encoder_state
            .as_mut()
            .expect("encoder state seeded")
            .begin_member();

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
        //
        // A filtered member cannot share the LZ window (see above), so a
        // solid archive never tries one: the first member would leave the
        // chain immediately and every later member would follow it, leaving
        // `-s` to buy nothing.
        let filtered = if self.write_ctx().solid.mode {
            None
        } else {
            self.try_auto_filters(&whole, method, dsl, dict_bytes)?
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
        // Mid-size members (>= MT_MIN) get the same windowed MT encode as
        // the streaming path, matching WinRAR's per-file parallelization;
        // the measured ratio divergence from the sequential chunk loop on
        // the corpus is within ±0.3% (the repeat-distance cache resets per
        // slice). Solid chains take it too: the window still carries over
        // through the shared tail and long-range table, so only the parse
        // tier differs from the sequential chain — the same documented MT
        // divergence, now visible inside a chain as well. Filter members
        // stay sequential (the transform runs over the whole buffer).
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
                threads > 1 && whole.len() >= MT_MIN
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
                .solid
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
                let state = self.write_ctx_mut().solid.encoder_state.as_mut();
                let compressed = lzss_huff::encode_chunked(
                    chunk,
                    lzss_huff::EncodeOptions {
                        chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                        state,
                        is_final: bytes_read >= whole.len() as u64,
                        variant: crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                        // `add_path` already probed the file before reading
                        // it; re-probing here would sample-encode every
                        // member twice.
                        skip_incompressible_probe: true,
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
        if !self.write_ctx().solid.mode {
            self.reset_solid_chain();
        }

        self.report_progress(file_size, file_size);

        Ok(())
    }

    /// Roll to a fresh volume until `needed` on-disk bytes fit; `needed`
    /// already includes the end-of-archive reserve. No-op for single-volume
    /// archives, and a volume too small for one header errors instead of
    /// rolling forever (matching the file-member splitter).
    fn ensure_rar5_volume_space(&mut self, needed: u64) -> RarResult<()> {
        let Some(volume_size) = self.write_ctx().output.volume_size else {
            return Ok(());
        };
        let mut rolled = false;
        loop {
            let used = self.write_ctx().output.bytes_written;
            if volume_size.saturating_sub(used) >= needed {
                return Ok(());
            }
            if rolled {
                return Err(RarError::InvalidOption(format!(
                    "volume size {volume_size} is too small for a RAR5 member header"
                )));
            }
            self.start_next_volume()?;
            rolled = true;
        }
    }

    /// The entry carries no data; `redir_type` is 1 (Unix symlink),
    /// 2 (Windows symlink), 3 (Windows junction), 4 (hardlink) or
    /// 5 (file copy) and `target` is the referenced member name.
    pub(crate) fn add_redirect(
        &mut self,
        name: &str,
        redir_type: u64,
        target: &str,
    ) -> RarResult<()> {
        if self.rar4 {
            return Err(RarError::Unsupported(
                "redirect members are not supported for RAR4 archives".into(),
            ));
        }
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
        let hdr_on_disk = self.on_disk_header_len(hdr_bytes.len() as u64);
        self.ensure_rar5_volume_space(hdr_on_disk + self.on_disk_header_len(8))?;
        if self.write_ctx().locator.quick_open {
            let pos = stream_mut(&mut self.stream)?.stream_position()?;
            self.write_ctx_mut()
                .locator
                .quick_open_entries
                .push((pos, hdr_bytes.clone()));
        }
        self.write_block_header(&hdr_bytes)?;
        self.write_ctx_mut().output.bytes_written += hdr_on_disk;
        self.entries.push(ArchiveEntry {
            header: fh,
            chunks: Vec::new(),
        });
        Ok(())
    }

    /// Write one RAR5 directory header (FILE_HEAD with the directory flag).
    /// Shared by the neutral `add_directory_only` / `add_directory`
    /// dispatchers in `format::shared::write_ops`.
    pub(crate) fn write_rar5_dir_entry(
        &mut self,
        name: &str,
        meta: &fs::Metadata,
        mtime: u32,
    ) -> RarResult<()> {
        #[cfg(unix)]
        let attrs = {
            use std::os::unix::fs::MetadataExt;
            meta.mode() as u64
        };
        #[cfg(not(unix))]
        let attrs = {
            let _ = meta;
            0o040755u64
        };

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
        let hdr_on_disk = self.on_disk_header_len(hdr_bytes.len() as u64);
        self.ensure_rar5_volume_space(hdr_on_disk + self.on_disk_header_len(8))?;
        if self.write_ctx().locator.quick_open {
            let pos = stream_mut(&mut self.stream)?.stream_position()?;
            self.write_ctx_mut()
                .locator
                .quick_open_entries
                .push((pos, hdr_bytes.clone()));
        }
        self.write_block_header(&hdr_bytes)?;
        self.write_ctx_mut().output.bytes_written += hdr_on_disk;
        self.entries.push(ArchiveEntry {
            header: fh,
            chunks: Vec::new(),
        });
        Ok(())
    }

    /// RAR5 raw-bytes member writer (the neutral `add_bytes` dispatcher
    /// lives in `format::shared::write_ops`).
    pub(crate) fn add_bytes_rar5(
        &mut self,
        arcname: &str,
        data: &[u8],
        compression_level: u8,
    ) -> RarResult<()> {
        let name = arcname.replace('\\', "/");
        let plain_crc = {
            let mut h = crc32fast::Hasher::new();
            h.update(data);
            h.finalize()
        };
        let plain_blake = if self.write_ctx().meta.blake2 {
            Some(crate::format::rar5::blake2sp::hash(data))
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
                self.write_ctx().compression.dict_size_log,
                self.write_ctx().compression.dict_size_bytes,
                method,
                self.write_ctx().compression.force_v70,
            );
            // WinRAR `-se`: reset the solid statistics when the extension changes.
            self.maybe_reset_solid_for_extension(&name);
            let chain_solid =
                self.write_ctx().solid.mode && self.write_ctx().solid.encoder_state.is_some();
            if self.write_ctx().solid.mode {
                self.write_ctx_mut()
                    .solid
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
                    state: self.write_ctx_mut().solid.encoder_state.as_mut(),
                    is_final: true,
                    variant: crate::version::ArchiveVersion::from_v70(dict_bytes.is_some()),
                    progress,
                    // Already probed above (`sample_is_incompressible`).
                    skip_incompressible_probe: true,
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
}
