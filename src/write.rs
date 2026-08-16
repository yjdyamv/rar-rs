//! Write pipeline: member creation, batch addition and the streaming
//! payload writer. Methods on [RarArchive] live in a sibling impl block
//! (see src/archive.rs for the shared state).

use super::*;

/// Write an NTFS alternate data stream (`path` + `stream_name` like
/// `:custom1`) on Windows.
#[cfg(windows)]
pub(crate) fn write_windows_stream(path: &Path, stream_name: &str, data: &[u8]) -> RarResult<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_ALWAYS,
    };
    let mut full: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    for unit in stream_name.encode_utf16() {
        full.insert(full.len() - 1, unit);
    }
    let handle = unsafe {
        CreateFileW(
            full.as_ptr(),
            0x4000_0000 | 0x8000_0000, // GENERIC_WRITE | GENERIC_READ
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(RarError::Io(std::io::Error::last_os_error()));
    }
    let mut written = 0u32;
    let ok = unsafe {
        WriteFile(
            handle,
            data.as_ptr() as *const _,
            data.len().min(u32::MAX as usize) as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return Err(RarError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Set a file's creation time (Windows only) via `SetFileTime`.
#[cfg(windows)]
pub(crate) fn windows_set_creation_time(path: &Path, secs: u64, ns: u32) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, SetFileTime, FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_WRITE_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let ft_100ns = (secs + 11_644_473_600) * 10_000_000 + u64::from(ns) / 100;
    let creation = FILETIME {
        dwLowDateTime: (ft_100ns & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ft_100ns >> 32) as u32,
    };
    let ok = unsafe { SetFileTime(handle, &creation, std::ptr::null(), std::ptr::null()) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Read a Windows file timestamp via `GetFileTime` (std exposes no
/// access/creation-time reader). `want_access` selects the last-access
/// time, otherwise the creation time. Returns unix (seconds, ns).
#[cfg(windows)]
fn windows_file_time(path: &Path, want_access: bool) -> Option<(u64, u32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileTime, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut access = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut write = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let ok = unsafe { GetFileTime(handle, &mut creation, &mut access, &mut write) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }
    let ft = if want_access {
        ((access.dwHighDateTime as u64) << 32) | access.dwLowDateTime as u64
    } else {
        ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64
    };
    Some((
        (ft / 10_000_000).saturating_sub(11_644_473_600),
        ((ft % 10_000_000) * 100) as u32,
    ))
}

/// Build the FILE_TIME extra record per explicit `-ts` settings (the
/// off-thread parallel batch path has no `&RarArchive`); `None` when no
/// time needs the extra record. On Windows the access/creation times are
/// read through `GetFileTime` (std exposes no access-time API).
fn time_extra_cfg(
    save_ctime: bool,
    save_atime: bool,
    save_mtime: bool,
    precision_seconds: bool,
    meta: &fs::Metadata,
    path: &Path,
    mtime: u32,
    mtime_ns: u32,
) -> Option<Vec<u8>> {
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
            windows_file_time(path, false).map(|(s, n)| (s, ns(n)))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = meta;
            let _ = path;
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
            windows_file_time(path, true).map(|(s, n)| (s, ns(n)))
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

impl RarArchive {

    // ── Public API: creation ───────────────────────────────────────────────

    /// Build the FILE_TIME extra record for `meta`, per the current
    /// `-ts` settings; `None` when no time needs the extra record.
    fn time_extra_for(&self, meta: &fs::Metadata, path: &Path, mtime: u32, mtime_ns: u32) -> Option<Vec<u8>> {
        time_extra_cfg(
            self.save_ctime,
            self.save_atime,
            self.save_mtime,
            self.time_precision_seconds,
            meta,
            path,
            mtime,
            mtime_ns,
        )
    }

    /// Build the OWNER extra record (numeric uid/gid) when `-ow` is on;
    /// `None` off-Unix or when disabled.
    fn owner_extra_for(&self, meta: &fs::Metadata) -> Option<Vec<u8>> {
        owner_extra_cfg(self.save_owner, meta)
    }

    /// Add a file from the filesystem to the archive.
    pub fn add(&mut self, path: impl AsRef<Path>, compression_level: u8) -> RarResult<()> {
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

        if let Some(cb) = self.progress_callback.as_deref_mut() {
            cb(0, file_size);
        }

        let method = level_to_method(level);
        let probe_incompressible = method != COMP_METHOD_STORE
            && file_size >= (SAMPLE_PROBE_HEAD as u64) * 4
            && sample_is_incompressible_file(path, file_size, method)?;
        let (dsl, dict_bytes) = dict_params_for(
            file_size as usize,
            self.dict_size_log,
            self.dict_size_bytes,
            method,
        );

        if method == COMP_METHOD_STORE || probe_incompressible {
            // STORE is written by streaming the file directly: bounded
            // memory regardless of file size. Encrypted STORE is encrypted
            // on the fly with a chained CBC state (also bounded memory).
            self.reset_solid_chain();
            let (plain_crc, plain_blake) = hash_file(path, file_size, self.blake2)?;
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
            if let Some(cb) = self.progress_callback.as_deref_mut() {
                cb(file_size, file_size);
            }
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

        // Compressed path: read and compress in bounded chunks with a
        // persistent encoder state (solid archives share the LZ window).
        let chain_solid = self.solid_mode && self.encoder_state.is_some();
        if self.solid_mode {
            self.encoder_state.get_or_insert_with(Default::default);
        }

        let mut crc_hasher = crc32fast::Hasher::new();
        let mut blake_hasher = if self.blake2 {
            Some(crate::blake2sp::Hasher::new())
        } else {
            None
        };
        let mut packed = Vec::new();
        let mut bytes_read = 0u64;
        {
            let mut file = io::BufReader::with_capacity(1 << 20, File::open(path)?);
            let mut buf = vec![0u8; crate::codec::DEFAULT_CHUNK_SIZE];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                bytes_read += n as u64;
                crc_hasher.update(&buf[..n]);
                if let Some(h) = blake_hasher.as_mut() {
                    h.update(&buf[..n]);
                }
                let state = self.encoder_state.as_mut();
                let compressed = compression::compress_chunked(
                    &buf[..n],
                    method,
                    dsl,
                    crate::codec::DEFAULT_CHUNK_SIZE,
                    state,
                    n < buf.len(),
                    None,
                    dict_bytes.is_some(),
                )
                .map_err(RarError::Unsupported)?;
                packed.extend(compressed);
                if let Some(cb) = self.progress_callback.as_deref_mut() {
                    cb(bytes_read, file_size);
                }
                if packed.len() as u64 >= file_size {
                    break;
                }
            }
        }

        let plain_crc = crc_hasher.finalize();
        let plain_blake = blake_hasher.map(|h| h.finalize());

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
            if let Some(cb) = self.progress_callback.as_deref_mut() {
                cb(file_size, file_size);
            }
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

        if let Some(cb) = self.progress_callback.as_deref_mut() {
            cb(file_size, file_size);
        }

        Ok(())
    }

    /// Add a file redirection entry (symlink, hardlink or file copy) to
    /// the archive (like `rar a -ol` / `-oh`).
    ///
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
        let path = path.as_ref();
        self.reset_solid_chain();
        let name = arcname.replace('\\', "/").trim_end_matches('/').to_string() + "/";

        let meta = fs::metadata(path)?;
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        #[cfg(unix)]
        let attrs = {
            use std::os::unix::fs::MetadataExt;
            meta.mode() as u64
        };
        #[cfg(not(unix))]
        let attrs = 0o040755u64;

        let fh = FileHeader {
            name: name.clone(),
            attributes: attrs,
            mtime,
            host_os: OS_UNIX,
            file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_DIRECTORY,
            is_directory: true,
            ..Default::default()
        };

        let hdr_bytes = fh.to_bytes();
        self.write_block_header(&hdr_bytes)?;
        self.volume_bytes_written += self.on_disk_header_len(hdr_bytes.len() as u64);
        self.entries.push(ArchiveEntry {
            header: fh,
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
        let name = name.replace('\\', "/").trim_end_matches('/').to_string() + "/";

        let meta = fs::metadata(path)?;
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        #[cfg(unix)]
        let attrs = {
            use std::os::unix::fs::MetadataExt;
            meta.mode() as u64
        };
        #[cfg(not(unix))]
        let attrs = 0o040755u64;

        let fh = FileHeader {
            name: name.clone(),
            attributes: attrs,
            mtime,
            host_os: OS_UNIX,
            file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_DIRECTORY,
            is_directory: true,
            ..Default::default()
        };

        let hdr_bytes = fh.to_bytes();
        self.write_block_header(&hdr_bytes)?;
        self.volume_bytes_written += self.on_disk_header_len(hdr_bytes.len() as u64);
        self.entries.push(ArchiveEntry {
            header: fh,
            chunks: Vec::new(),
        });

        if recursive {
            let mut children: Vec<_> = fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
            children.sort_by_key(|e| e.file_name());

            for child in children {
                let child_path = child.path();
                let child_name = format!("{}{}", name, child.file_name().to_string_lossy());
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
        let name = arcname.replace('\\', "/");
        let plain_crc = {
            let mut h = crc32fast::Hasher::new();
            h.update(data);
            h.finalize()
        };
        let plain_blake = if self.blake2 {
            Some(crate::blake2sp::hash(data))
        } else {
            None
        };
        let mtime = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        let method = level_to_method(compression_level);
        if let Some(cb) = self.progress_callback.as_deref_mut() {
            cb(0, data.len() as u64);
        }
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
                self.dict_size_log,
                self.dict_size_bytes,
                method,
            );
            let chain_solid = self.solid_mode && self.encoder_state.is_some();
            if self.solid_mode {
                self.encoder_state.get_or_insert_with(Default::default);
            }
            let mut progress: Option<&mut dyn FnMut(u64, u64)> = None;
            if let Some(cb) = self.progress_callback.as_deref_mut() {
                let cb: &mut dyn FnMut(u64, u64) = cb;
                progress = Some(cb);
            }
            let packed = compression::compress_chunked(
                data,
                method,
                dsl,
                crate::codec::DEFAULT_CHUNK_SIZE,
                self.encoder_state.as_mut(),
                true,
                progress,
                dict_bytes.is_some(),
            )
            .map_err(RarError::Unsupported)?;
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

        if let Some(cb) = self.progress_callback.as_deref_mut() {
            cb(data.len() as u64, data.len() as u64);
        }

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
        #[cfg(feature = "parallel")]
        {
            if !self.solid_mode && !entries.is_empty() {
                return self.add_batch_parallel(entries);
            }
        }
        for entry in entries {
            self.add_batch_entry_sequential(entry)?;
        }
        Ok(())
    }

    fn add_batch_entry_sequential(&mut self, entry: &BatchEntry<'_>) -> RarResult<()> {
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
        let mut i = 0usize;
        while i < entries.len() {
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
                let prepared = self.prepare_batch_wave(&wave)?;
                for (_, entry) in prepared {
                    let size = entry.unpacked_size;
                    if let Some(cb) = self.progress_callback.as_deref_mut() {
                        cb(0, size);
                    }
                    self.write_prepared_entry(entry)?;
                    if let Some(cb) = self.progress_callback.as_deref_mut() {
                        cb(size, size);
                    }
                }
            }

            if i < entries.len() {
                if let BatchEntry::File { path, name, level } = entries[i] {
                    let size = fs::metadata(path)?.len();
                    if size > PARALLEL_COMPRESS_MAX_MEMBER && size <= PARALLEL_BUFFER_LIMIT {
                        if let Some(prepared) =
                            self.prepare_large_file_parallel(path, name, level)?
                        {
                            let member_size = prepared.unpacked_size;
                            if let Some(cb) = self.progress_callback.as_deref_mut() {
                                cb(0, member_size);
                            }
                            self.write_prepared_entry(prepared)?;
                            if let Some(cb) = self.progress_callback.as_deref_mut() {
                                cb(member_size, member_size);
                            }
                            i += 1;
                            continue;
                        }
                        // STORE / probe-incompressible fallback: the
                        // sequential path streams the member directly.
                    } else if size > PARALLEL_BUFFER_LIMIT {
                        // Members beyond the parallel buffer budget stream
                        // through the sequential path: the compressed
                        // output is spilled to a temporary file instead of
                        // being buffered in memory (bounded memory for any
                        // file size).
                    }
                }
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
    ) -> RarResult<Vec<(usize, PreparedEntry)>> {
        use rayon::prelude::*;

        let ctx = BatchPrepareCtx {
            password: self.password.as_deref(),
            blake2: self.blake2,
            dict_size_log: self.dict_size_log,
            dict_size_bytes: self.dict_size_bytes,
            save_ctime: self.save_ctime,
            save_atime: self.save_atime,
            save_mtime: self.save_mtime,
            save_owner: self.save_owner,
            time_precision_seconds: self.time_precision_seconds,
        };
        let results: Vec<RarResult<(usize, PreparedEntry)>> = compression_pool().install(|| {
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
                                &ctx, name, data, level, 0o100644, mtime, None, false,
                            )
                        }
                        BatchEntry::File { path, name, level } => {
                            Self::prepare_file_entry(&ctx, path, name, level)
                        }
                        BatchEntry::Directory { .. } => {
                            unreachable!("directories never enter a compression wave")
                        }
                    };
                    prepared.map(|p| (idx, p))
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
    #[cfg(feature = "parallel")]
    fn prepare_data_entry(
        ctx: &BatchPrepareCtx<'_>,
        name: &str,
        data: &[u8],
        level: u8,
        attrs: u64,
        mtime: u32,
        time_extra: Option<Vec<u8>>,
        file_origin: bool,
    ) -> RarResult<PreparedEntry> {
        let plain_crc = crc32fast::hash(data);
        let plain_blake = if ctx.blake2 {
            Some(crate::blake2sp::hash(data))
        } else {
            None
        };
        let method = level_to_method(level);

        if method == COMP_METHOD_STORE || sample_is_incompressible(data, method) {
            return Self::prepared_from_payload(
                ctx,
                name,
                data.len(),
                attrs,
                mtime,
                time_extra,
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
        );
        let packed = if file_origin {
            // Mirror add_file's streaming loop exactly: each chunk is
            // compressed with a fresh encoder window (non-solid archives),
            // so the batch archive stays byte-identical to the sequential
            // path.
            let mut packed = Vec::new();
            for chunk in data.chunks(crate::codec::DEFAULT_CHUNK_SIZE) {
                let is_final = chunk.len() < crate::codec::DEFAULT_CHUNK_SIZE;
                let compressed = compression::compress_chunked(
                    chunk,
                    method,
                    dsl,
                    crate::codec::DEFAULT_CHUNK_SIZE,
                    None,
                    is_final,
                    None,
                    dict_bytes.is_some(),
                )
                .map_err(RarError::Unsupported)?;
                packed.extend(compressed);
                if packed.len() >= data.len() {
                    break;
                }
            }
            packed
        } else {
            compression::compress_chunked(
                data,
                method,
                dsl,
                crate::codec::DEFAULT_CHUNK_SIZE,
                None,
                true,
                None,
                dict_bytes.is_some(),
            )
            .map_err(RarError::Unsupported)?
        };

        if packed.len() >= data.len() {
            return Self::prepared_from_payload(
                ctx,
                name,
                data.len(),
                attrs,
                mtime,
                time_extra,
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
        Self::prepare_data_entry(ctx, &name, &data, level, attrs, mtime, time_extra, true)
    }

    /// Prepare a large file (over [`PARALLEL_COMPRESS_MAX_MEMBER`], up to
    /// [`PARALLEL_BUFFER_LIMIT`]) by compressing its 4 MiB chunks in
    /// parallel and concatenating them in file order. Members beyond the
    /// buffer limit stream through the sequential path instead (bounded
    /// memory for any file size).
    ///
    /// Non-solid archives encode each chunk with a fresh encoder window, so
    /// the packed stream is byte-identical to the sequential `add_file`
    /// path. Raw input is never buffered whole: each Rayon worker reads and
    /// compresses one chunk at a time, bounding memory to roughly
    /// `threads × chunk size + packed output`.
    ///
    /// Returns `None` when the member should go through the sequential
    /// path instead (STORE / sample-probe incompressible, or compression
    /// would not shrink the payload).
    #[cfg(feature = "parallel")]
    fn prepare_large_file_parallel(
        &self,
        path: &Path,
        arcname: Option<&str>,
        level: u8,
    ) -> RarResult<Option<PreparedEntry>> {
        use rayon::prelude::*;

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

        let method = level_to_method(level);
        let probe_incompressible = method != COMP_METHOD_STORE
            && file_size >= (SAMPLE_PROBE_HEAD as u64) * 4
            && sample_is_incompressible_file(path, file_size, method)?;
        if method == COMP_METHOD_STORE || probe_incompressible {
            return Ok(None);
        }

        let (dsl, dict_bytes) = dict_params_for(
            file_size as usize,
            self.dict_size_log,
            self.dict_size_bytes,
            method,
        );
        let (plain_crc, plain_blake) = hash_file(path, file_size, self.blake2)?;
        let mtime_ns = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let time_extra = self.time_extra_for(&meta, path, mtime, mtime_ns);
        let owner_extra = self.owner_extra_for(&meta);

        let chunk_size = crate::codec::DEFAULT_CHUNK_SIZE as u64;
        let chunk_count = file_size.div_ceil(chunk_size) as usize;
        let results: Vec<RarResult<(usize, Vec<u8>)>> = large_file_pool().install(|| {
            (0..chunk_count)
                .into_par_iter()
                .map(|idx| {
                    let _guard = BatchWorkerGuard::new();
                    let start = idx as u64 * chunk_size;
                    let len = file_size.saturating_sub(start).min(chunk_size) as usize;
                    let mut buf = vec![0u8; len];
                    {
                        use std::io::{Read, Seek, SeekFrom};
                        let mut f = fs::File::open(path)?;
                        f.seek(SeekFrom::Start(start))?;
                        f.read_exact(&mut buf)?;
                    }
                    let is_final = len < chunk_size as usize;
                    let packed = compression::compress_chunked(
                        &buf,
                        method,
                        dsl,
                        crate::codec::DEFAULT_CHUNK_SIZE,
                        None,
                        is_final,
                        None,
                        dict_bytes.is_some(),
                    )
                    .map_err(RarError::Unsupported)?;
                    Ok((idx, packed))
                })
                .collect()
        });

        let mut packed = Vec::new();
        for result in results {
            let (_, chunk_packed) = result?;
            packed.extend(chunk_packed);
            if packed.len() as u64 >= file_size {
                break;
            }
        }
        if packed.len() as u64 >= file_size {
            // Compression is a net loss; the sequential path streams STORE.
            return Ok(None);
        }

        let (header_crc, mut extra_data, stored_hash, encr_params) =
            RarArchive::payload_extra_and_crc(self.password.as_deref(), plain_crc, plain_blake)?;
        if let Some(t) = time_extra {
            extra_data.extend_from_slice(&t);
        }
        let payload = RarArchive::encrypt_payload_with(
            self.password.as_deref(),
            encr_params.as_ref(),
            &packed,
        )?;
        Ok(Some(PreparedEntry {
            name,
            unpacked_size: file_size,
            attrs,
            mtime,
            file_crc: header_crc,
            method,
            dict_size_log: dsl,
            dict_size_bytes: dict_bytes,
            extra_data,
            stored_hash,
            payload,
        }))
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

        if self.volume_size.is_none() {
            // Single-volume
            let hdr_bytes = fh_base.to_bytes();
            if self.quick_open {
                let pos = self.stream.as_mut().unwrap().stream_position()?;
                self.quick_open_entries.push((pos, hdr_bytes.clone()));
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
        let volume_size = self.volume_size.unwrap();
        // End-of-archive block: 8 plaintext bytes, or `[IV][padded]` when
        // header encryption wraps every block.
        let eoa_plain: u64 = 8;
        let eoa_size: u64 = self.on_disk_header_len(eoa_plain);
        let total_packed = packed_data.len() as u64;

        // Check if it fits in current volume
        let hdr_bytes = fh_base.to_bytes();
        let hdr_on_disk = self.on_disk_header_len(hdr_bytes.len() as u64);
        let total_needed = hdr_on_disk + total_packed + eoa_size;
        let remaining = volume_size.saturating_sub(self.volume_bytes_written);

        if total_needed <= remaining {
            // Fits entirely
            self.write_block_header(&hdr_bytes)?;
            let stream = self.stream.as_mut().unwrap();
            stream.write_all(packed_data)?;
            self.volume_bytes_written += hdr_on_disk + total_packed;
            let data_offset = stream.stream_position()? - total_packed;
            let chunk = DataChunk {
                volume_index: self.current_volume - 1,
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

        // Need to split across volumes
        let mut offset = 0u64;
        let mut chunks = Vec::new();
        let mut is_first = true;

        // Encrypted members: every chunk header carries the encryption
        // extra record (WinRAR repeats it on every volume). Non-final
        // chunks verify with a plain crc32 of the ciphertext chunk, so
        // their record must clear the hash-key MAC bit (flags=1); the
        // final chunk keeps the full record (flags=3, MAC'd checksum).
        let encr_params = if self.password.is_some() {
            encryption::parse_encryption_extra(extra_data)?
        } else {
            None
        };
        let chunk_extra = |is_last: bool, is_first: bool| -> Vec<u8> {
            if let Some(ref p) = encr_params {
                if is_last {
                    extra_data.to_vec()
                } else {
                    let mut np = p.clone();
                    np.flags &= !0x02;
                    np.to_extra_bytes()
                }
            } else if is_first {
                extra_data.to_vec()
            } else {
                Vec::new()
            }
        };

        while offset < total_packed {
            let remaining_vol = volume_size.saturating_sub(self.volume_bytes_written);

            // Build chunk flags
            let mut block_flags: u64 = 0;
            if !is_first {
                block_flags |= BLOCK_FLAG_DATA_CONTINUES;
            }

            // Estimate header size
            let chunk_fh = FileHeader {
                name: name.to_string(),
                unpacked_size,
                packed_size: remaining_vol.max(1),
                attributes: attrs,
                mtime,
                crc32_val: Some(0),
                comp_method: method,
                comp_solid: solid,
                comp_dict_size: dict_size_log,
                dict_size_bytes,
                host_os: OS_UNIX,
                flags: block_flags | BLOCK_FLAG_DATA_CONTINUE_TO,
                file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
                extra_data: chunk_extra(false, is_first),
                ..Default::default()
            };
            let hdr_size = self.on_disk_header_len(chunk_fh.to_bytes().len() as u64);

            let bytes_for_data = remaining_vol.saturating_sub(hdr_size + eoa_size);
            eprintln!("SPLIT vol_bytes={} remaining={} hdr_disk={} eoa={} bytes_for_data={} vol_size={}", self.volume_bytes_written, remaining_vol, hdr_size, eoa_size, bytes_for_data, volume_size);
            if bytes_for_data == 0 {
                self.start_next_volume()?;
                is_first = false;
                continue;
            }

            let chunk_size = bytes_for_data.min(total_packed - offset);
            let is_last = offset + chunk_size >= total_packed;
            let chunk_packed = &packed_data[offset as usize..(offset + chunk_size) as usize];

            // Set final flags
            if is_last {
                block_flags &= !BLOCK_FLAG_DATA_CONTINUE_TO;
            } else {
                block_flags |= BLOCK_FLAG_DATA_CONTINUE_TO;
            }

            let chunk_crc = if is_last {
                file_crc
            } else {
                let mut h = crc32fast::Hasher::new();
                h.update(chunk_packed);
                h.finalize()
            };

            let final_fh = FileHeader {
                name: name.to_string(),
                unpacked_size,
                packed_size: chunk_size,
                attributes: attrs,
                mtime,
                crc32_val: Some(chunk_crc),
                comp_method: method,
                comp_solid: solid,
                comp_dict_size: dict_size_log,
                dict_size_bytes,
                host_os: OS_UNIX,
                flags: block_flags,
                file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
                extra_data: chunk_extra(is_last, is_first),
                ..Default::default()
            };

            let final_hdr = final_fh.to_bytes();
            let final_hdr_disk = self.on_disk_header_len(final_hdr.len() as u64);
            self.write_block_header(&final_hdr)?;
            let stream = self.stream.as_mut().unwrap();
            stream.write_all(chunk_packed)?;
            self.volume_bytes_written += final_hdr_disk + chunk_size;

            let data_offset = stream.stream_position()? - chunk_size;
            chunks.push(DataChunk {
                volume_index: self.current_volume - 1,
                data_offset,
                packed_size: chunk_size,
                crc32_val: Some(chunk_crc),
                is_final: is_last,
                extra_data: if is_first {
                    extra_data.to_vec()
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
        self.encoder_state = None;
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
        Option<encryption::EncryptionParams>,
    )> {
        if let Some(password) = password {
            let params =
                encryption::EncryptionParams::generate_for_password(password, ENCR_PBKDF2_ITER_LOG);
            let header_crc = params.mac_crc32(plain_crc, password)?;
            let stored_hash = match plain_blake {
                Some(h) => Some(params.mac_hash32(h, password)?),
                None => None,
            };
            let mut extra = params.to_extra_bytes();
            if let Some(h) = stored_hash {
                extra.extend(crate::headers::hash_extra_record(h));
            }
            Ok((header_crc, extra, stored_hash, Some(params)))
        } else {
            let mut extra = Vec::new();
            if let Some(h) = plain_blake {
                extra.extend(crate::headers::hash_extra_record(h));
            }
            Ok((plain_crc, extra, plain_blake, None))
        }
    }

    /// Encrypt a member payload with the parameters returned by
    /// [`Self::payload_extra_and_crc`] (must match the member's stored
    /// salt).
    pub(crate) fn encrypt_payload_with(
        password: Option<&str>,
        params: Option<&encryption::EncryptionParams>,
        plaintext: &[u8],
    ) -> RarResult<Vec<u8>> {
        match (password, params) {
            (Some(password), Some(params)) => Ok(params.encrypt(plaintext, password)),
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
        encr: Option<&encryption::EncryptionParams>,
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
            (Some(params), Some(password)) => Some((params.get_key(password), params.iv)),
            (None, None) => None,
            _ => {
                return Err(RarError::Format(
                    "internal error: encryption parameters mismatch".into(),
                ))
            }
        };
        let mut write_src = payload_stream(&key_iv);
        let mut probe_src = payload_stream(&key_iv);

        if self.volume_size.is_none() {
            // ── Single-volume ──
            let hdr_bytes = fh_base.to_bytes();
            if self.quick_open {
                let pos = self.stream.as_mut().unwrap().stream_position()?;
                self.quick_open_entries.push((pos, hdr_bytes.clone()));
            }
            self.write_block_header(&hdr_bytes)?;
            let written = {
                let stream = self.stream.as_mut().unwrap();
                if progress {
                    match self.progress_callback.as_deref_mut() {
                        Some(cb) => {
                            let mut sink = ProgressWriter {
                                inner: stream,
                                total: unpacked_size,
                                written: 0,
                                cb,
                            };
                            write_src.emit_to(reader, plain_len, 0, packed_size, &mut sink)?;
                            sink.written
                        }
                        None => {
                            let mut counting = CountingWriter::new(stream);
                            write_src.emit_to(reader, plain_len, 0, packed_size, &mut counting)?;
                            counting.written()
                        }
                    }
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
        let volume_size = self.volume_size.unwrap();
        // End-of-archive block: 8 plaintext bytes, or `[IV][padded]` when
        // header encryption wraps every block.
        let eoa_size: u64 = self.on_disk_header_len(8);
        let mut offset = 0u64;
        let mut chunks = Vec::new();
        let mut is_first = true;

        // Encrypted members: every chunk header carries the encryption
        // extra record (WinRAR repeats it on every volume). Non-final
        // chunks verify with a plain crc32 of the ciphertext chunk, so
        // their record must clear the hash-key MAC bit (flags=1); the
        // final chunk keeps the full record (flags=3, MAC'd checksum).
        let encr_params = if self.password.is_some() {
            encryption::parse_encryption_extra(extra_data)?
        } else {
            None
        };
        let chunk_extra = |is_last: bool, is_first: bool| -> Vec<u8> {
            if let Some(ref p) = encr_params {
                if is_last {
                    extra_data.to_vec()
                } else {
                    let mut np = p.clone();
                    np.flags &= !0x02;
                    np.to_extra_bytes()
                }
            } else if is_first {
                extra_data.to_vec()
            } else {
                Vec::new()
            }
        };

        while offset < packed_size {
            let remaining_vol = volume_size.saturating_sub(self.volume_bytes_written);

            // Build chunk flags
            let mut block_flags: u64 = 0;
            if !is_first {
                block_flags |= BLOCK_FLAG_DATA_CONTINUES;
            }

            // Estimate header size
            let chunk_fh = FileHeader {
                name: name.to_string(),
                unpacked_size,
                packed_size: remaining_vol.max(1),
                attributes: attrs,
                mtime,
                crc32_val: Some(0),
                comp_method: method,
                comp_solid: solid,
                comp_dict_size: dict_size_log,
                dict_size_bytes,
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

            let chunk_size = bytes_for_data.min(packed_size - offset);
            let is_last = offset + chunk_size >= packed_size;

            // Set final flags
            if is_last {
                block_flags &= !BLOCK_FLAG_DATA_CONTINUE_TO;
            } else {
                block_flags |= BLOCK_FLAG_DATA_CONTINUE_TO;
            }

            // For non-final chunks the header carries the CRC of this
            // chunk's on-disk bytes (the ciphertext when encrypted),
            // computed in a probe pass with an independent encryptor
            // chain so the write pass below is not disturbed.
            let chunk_crc = if is_last {
                file_crc
            } else {
                let mut h = crc32fast::Hasher::new();
                let mut sink = CrcSink(&mut h);
                probe_src.emit_to(reader, plain_len, offset, offset + chunk_size, &mut sink)?;
                h.finalize()
            };

            let final_fh = FileHeader {
                name: name.to_string(),
                unpacked_size,
                packed_size: chunk_size,
                attributes: attrs,
                mtime,
                crc32_val: Some(chunk_crc),
                comp_method: method,
                comp_solid: solid,
                comp_dict_size: dict_size_log,
                dict_size_bytes,
                host_os: OS_UNIX,
                flags: block_flags,
                file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_CRC32,
                extra_data: chunk_extra(is_last, is_first),
                ..Default::default()
            };

            let final_hdr = final_fh.to_bytes();
            let final_hdr_disk = self.on_disk_header_len(final_hdr.len() as u64);
            self.write_block_header(&final_hdr)?;
            {
                let stream = self.stream.as_mut().unwrap();
                if progress {
                    match self.progress_callback.as_deref_mut() {
                        Some(cb) => {
                            let mut sink = ProgressWriter {
                                inner: stream,
                                total: unpacked_size,
                                written: offset,
                                cb,
                            };
                            write_src
                                .emit_to(reader, plain_len, offset, offset + chunk_size, &mut sink)?;
                        }
                        None => {
                            write_src.emit_to(
                                reader,
                                plain_len,
                                offset,
                                offset + chunk_size,
                                &mut *stream,
                            )?;
                        }
                    }
                } else {
                    write_src.emit_to(reader, plain_len, offset, offset + chunk_size, &mut *stream)?;
                }
            }
            self.volume_bytes_written += final_hdr_disk + chunk_size;
            let stream = self.stream.as_mut().unwrap();
            let data_offset = stream.stream_position()? - chunk_size;
            chunks.push(DataChunk {
                volume_index: self.current_volume - 1,
                data_offset,
                packed_size: chunk_size,
                crc32_val: Some(chunk_crc),
                is_final: is_last,
                extra_data: if is_first {
                    extra_data.to_vec()
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
                packed_size,
                ..fh_base
            },
            chunks,
        });

        Ok(())
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
        encr_params: Option<&encryption::EncryptionParams>,
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
                encryption::zero_padded_len(file_size),
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
        let chain_solid = self.solid_mode && self.encoder_state.is_some();
        if self.solid_mode {
            self.encoder_state.get_or_insert_with(Default::default);
        }

        let mut crc_hasher = crc32fast::Hasher::new();
        let mut blake_hasher = if self.blake2 {
            Some(crate::blake2sp::Hasher::new())
        } else {
            None
        };
        let mut bytes_read = 0u64;
        let mut packed_size = 0u64;
        let spill_path = spill_path_for(&self.path);
        let _spill_guard = SpillGuard(spill_path.clone());
        {
            let mut spill = File::create(&spill_path)?;
            let mut file = io::BufReader::with_capacity(1 << 20, File::open(path)?);
            let mut buf = vec![0u8; crate::codec::DEFAULT_CHUNK_SIZE];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                bytes_read += n as u64;
                crc_hasher.update(&buf[..n]);
                if let Some(h) = blake_hasher.as_mut() {
                    h.update(&buf[..n]);
                }
                let state = self.encoder_state.as_mut();
                let compressed = compression::compress_chunked(
                    &buf[..n],
                    method,
                    dsl,
                    crate::codec::DEFAULT_CHUNK_SIZE,
                    state,
                    n < buf.len(),
                    None,
                    dict_bytes.is_some(),
                )
                .map_err(RarError::Unsupported)?;
                spill.write_all(&compressed)?;
                packed_size += compressed.len() as u64;
                if let Some(cb) = self.progress_callback.as_deref_mut() {
                    cb(bytes_read, file_size);
                }
                if packed_size >= file_size {
                    break;
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
            if let Some(cb) = self.progress_callback.as_deref_mut() {
                cb(file_size, file_size);
            }
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
            packed_size,
            encr_params.as_ref(),
            password.as_deref(),
            false,
        )?;
        self.write_member_streams(path)?;
        if let Some(cb) = self.progress_callback.as_deref_mut() {
            cb(file_size, file_size);
        }
        Ok(())
    }

    /// Write the NTFS alternate data streams of `path` as "STM" service
    /// records right after the member's file block (WinRAR `-os`).
    fn write_member_streams(&mut self, path: &Path) -> RarResult<()> {
        if !self.save_streams {
            return Ok(());
        }
        #[cfg(windows)]
        {
            for stream in enumerate_windows_streams(path)? {
                // FindFirstStreamW returns names like "file:stream:$DATA"
                // (and "::$DATA" for the default stream); normalize to the
                // archive form ":stream".
                let raw_name = stream.0;
                if raw_name == "::$DATA" {
                    continue; // the default unnamed stream
                }
                let trimmed = raw_name
                    .strip_suffix(":$DATA")
                    .unwrap_or(&raw_name);
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
                    extra.extend(vint::encode(crate::constants::EXTRA_SERVICE_SUBDATA));
                    extra.extend(name.as_bytes());
                    extra
                };
                let hdr = crate::headers::build_service_block(
                    "STM",
                    &subdata,
                    data.len() as u64,
                    crate::constants::BLOCK_FLAG_DEPENDS_PREV,
                );
                self.write_block_header(&hdr)?;
                let stream = self.stream.as_mut().unwrap();
                stream.write_all(&data)?;
                self.volume_bytes_written =
                    self.volume_bytes_written.saturating_add(data.len() as u64);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = path;
        }
        Ok(())
    }

}

/// Enumerate the NTFS alternate data streams of `path` on Windows:
/// `(stream_name_with_leading_colon, size)` pairs.
#[cfg(windows)]
fn enumerate_windows_streams(path: &Path) -> RarResult<Vec<(String, u64)>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        FindFirstStreamW, FindNextStreamW, FindStreamInfoStandard, WIN32_FIND_STREAM_DATA,
    };
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut data = WIN32_FIND_STREAM_DATA {
        cStreamName: [0u16; 296],
        StreamSize: 0,
    };
    let handle = unsafe {
        FindFirstStreamW(
            wide.as_ptr(),
            FindStreamInfoStandard,
            &mut data as *mut _ as *mut core::ffi::c_void,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Ok(Vec::new()); // no streams (or not NTFS)
    }
    let mut out = Vec::new();
    loop {
        let mut len = 0usize;
        while len < data.cStreamName.len() && data.cStreamName[len] != 0 {
            len += 1;
        }
        let name = String::from_utf16_lossy(&data.cStreamName[..len]);
        if !name.is_empty() {
            out.push((name, data.StreamSize as u64));
        }
        let ok = unsafe { FindNextStreamW(handle, &mut data as *mut _ as *mut core::ffi::c_void) };
        if ok == 0 {
            break;
        }
    }
    unsafe { CloseHandle(handle) };
    Ok(out)
}
