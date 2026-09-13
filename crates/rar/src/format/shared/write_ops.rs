//! Format-neutral `RarArchive` write operations shared by the RAR4 and RAR5
//! pipelines: the member-addition dispatchers, batch progress/sequential
//! fallback and solid-chain state resets.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::archive::{BatchEntry, RarArchive};
use crate::error::{RarError, RarResult};

/// Derive an archive member name from a filesystem path when the caller did
/// not supply one. Root paths (`/`, `C:\`) have no final component; that is
/// a caller error rather than an internal invariant, so it maps to
/// `InvalidOption` instead of panicking on `file_name().unwrap()`.
pub(crate) fn archive_name_from_path(path: &Path) -> RarResult<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| {
            RarError::InvalidOption(format!(
                "cannot derive an archive name from {}; pass an explicit name",
                path.display()
            ))
        })
}

impl RarArchive {
    /// Add a file from the filesystem to the archive.
    pub(crate) fn add(&mut self, path: impl AsRef<Path>, compression_level: u8) -> RarResult<()> {
        self.check_cancel()?;
        let path = path.as_ref();
        if !path.exists() {
            return Err(RarError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
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
    pub(crate) fn add_as(
        &mut self,
        path: impl AsRef<Path>,
        arcname: &str,
        compression_level: u8,
    ) -> RarResult<()> {
        self.check_cancel()?;
        let path = path.as_ref();
        if !path.exists() {
            return Err(RarError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
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

    /// The container-neutral file dispatcher: RAR 1.3/1.4 uses its own
    /// DOS-era pipeline, RAR4 the legacy one, RAR5 the modern one.
    pub(crate) fn add_file(
        &mut self,
        path: &Path,
        arcname: Option<&str>,
        level: u8,
    ) -> RarResult<()> {
        if self.rar13 {
            self.add_file_rar13(path, arcname, level)
        } else if self.rar4 {
            self.add_file_rar4(path, arcname, level)
        } else {
            self.add_file_rar5(path, arcname, level)
        }
    }

    /// Add raw bytes as a named file in the archive.
    pub(crate) fn add_bytes(
        &mut self,
        arcname: &str,
        data: &[u8],
        compression_level: u8,
    ) -> RarResult<()> {
        self.check_cancel()?;
        if self.rar13 {
            let name = arcname.replace('\\', "/");
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            return self.add_rar13_data(
                name,
                data.to_vec(),
                compression_level,
                now.as_secs() as u32,
                now.subsec_nanos(),
                None,
            );
        }
        if self.rar4 {
            // RAR4 members are encoded through the same pipeline as
            // `add_file_rar4` (CRC, LZ/PPMd/filter/STORE candidates,
            // per-member encryption, volume splitting) with the current
            // time as the timestamp.
            let name = arcname.replace('\\', "/");
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            return self.add_rar4_data(
                name,
                data.to_vec(),
                compression_level,
                now.as_secs() as u32,
                now.subsec_nanos(),
                None,
            );
        }
        self.add_bytes_rar5(arcname, data, compression_level)
    }

    /// Add a directory entry only (no recursion).
    ///
    /// Writes the directory header without traversing children. Callers that
    /// enumerate files themselves (e.g. with exclusion filtering) use this to
    /// keep empty directories and the directory structure in the archive.
    pub(crate) fn add_directory_only(
        &mut self,
        path: impl AsRef<Path>,
        arcname: &str,
    ) -> RarResult<()> {
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
        let mtime_ns = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();

        if self.rar13 {
            return self.write_rar13_dir_entry(&name, mtime, mtime_ns);
        }
        if self.rar4 {
            return self.write_rar4_dir_entry(&name, mtime, mtime_ns);
        }
        self.write_rar5_dir_entry(&name, &meta, mtime)
    }

    /// Add a directory, optionally recursing into its children.
    fn add_directory(
        &mut self,
        path: &Path,
        arcname: Option<&str>,
        recursive: bool,
        level: u8,
    ) -> RarResult<()> {
        self.reset_solid_chain();
        let name = match arcname {
            Some(s) => s.to_string(),
            None => archive_name_from_path(path)?,
        };
        let name = name.replace('\\', "/").trim_end_matches('/').to_string();

        let meta = fs::metadata(path)?;
        let mtime = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        if self.rar13 {
            let mtime_ns = meta
                .modified()
                .unwrap_or(SystemTime::now())
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            self.write_rar13_dir_entry(&name, mtime, mtime_ns)?;
        } else if self.rar4 {
            let mtime_ns = meta
                .modified()
                .unwrap_or(SystemTime::now())
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            self.write_rar4_dir_entry(&name, mtime, mtime_ns)?;
        } else {
            self.write_rar5_dir_entry(&name, &meta, mtime)?;
        }

        if recursive {
            let mut children: Vec<_> = fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
            children.sort_by_key(|e| e.file_name());

            for child in children {
                self.check_cancel()?;
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

    /// One batch through the container's parallel path when eligible,
    /// otherwise the sequential fallback; archive order is always preserved.
    pub(crate) fn add_batch(&mut self, entries: &[BatchEntry<'_>]) -> RarResult<()> {
        self.check_cancel()?;
        #[cfg(feature = "parallel")]
        {
            if !self.rar4
                && !self.rar13
                && !self.write_ctx().solid.mode
                && !self.write_ctx().meta.streams
                && !entries.is_empty()
            {
                return self.add_batch_parallel(entries);
            }
            // RAR4: independent non-solid file members compress in parallel
            // waves too (solid runs stay sequential - shared window; a
            // deferred solid append buffers its additions for the close-time
            // repack and must never stream-write).
            if self.rar4
                && !self.write_ctx().solid.mode
                && !self.write_ctx().rar4.solid_append
                && !entries.is_empty()
            {
                return self.add_batch_parallel_rar4(entries);
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
    pub(crate) fn progress_set_batch_total(&mut self, entries: &[BatchEntry<'_>]) -> RarResult<()> {
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

    /// Route one batch entry through the format-neutral add* entry points.
    pub(crate) fn add_batch_entry_sequential(&mut self, entry: &BatchEntry<'_>) -> RarResult<()> {
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

    /// Drop the solid-chain encoder state (call after any member that does
    /// not participate in the LZ window: directories, STORE files, empty
    /// files, or when compression fell back to STORE).
    pub(crate) fn reset_solid_chain(&mut self) {
        self.write_ctx_mut().solid.encoder_state = None;
        self.write_ctx_mut().solid.rar4_encoder = None;
        self.write_ctx_mut().solid.legacy_encoder = None;
        self.write_ctx_mut().solid.rar4_run_has_member = false;
        self.write_ctx_mut().solid.last_ext = None;
    }

    /// Reset the solid chain when the next member's file extension differs
    /// from the previous one (WinRAR `-se`). No-op unless solid mode is on
    /// and `solid_reset` is `PerExtension`. Directories and STORE members
    /// break the chain through `reset_solid_chain`, which also clears
    /// `last_solid_ext`, so this only needs to run for compressed members.
    pub(crate) fn maybe_reset_solid_for_extension(&mut self, name: &str) {
        if !self.write_ctx().solid.mode
            || self.write_ctx().solid.reset != crate::options::SolidReset::PerExtension
        {
            return;
        }
        let base = name.trim_end_matches('/');
        let ext = base.rsplit('.').next().unwrap_or("");
        match &self.write_ctx().solid.last_ext {
            Some(prev) if prev == ext => {}
            _ => {
                self.write_ctx_mut().solid.encoder_state = None;
                self.write_ctx_mut().solid.rar4_encoder = None;
                self.write_ctx_mut().solid.legacy_encoder = None;
                self.write_ctx_mut().solid.rar4_run_has_member = false;
                self.write_ctx_mut().solid.last_ext = Some(ext.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::archive_name_from_path;
    use std::path::Path;

    #[test]
    fn paths_without_a_file_name_are_rejected() {
        for path in [Path::new(".."), Path::new("/")] {
            let err = archive_name_from_path(path).unwrap_err();
            assert!(
                matches!(err, crate::error::RarError::InvalidOption(_)),
                "{path:?}: expected InvalidOption, got {err}"
            );
        }
        assert_eq!(
            archive_name_from_path(Path::new("dir/file.txt")).unwrap(),
            "file.txt"
        );
    }
}
