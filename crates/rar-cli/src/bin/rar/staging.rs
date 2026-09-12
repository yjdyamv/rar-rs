//! Transactional archive staging: copy, update, replace.

use crate::error::CliResult;
/// A same-directory archive copy that is removed unless successfully
/// installed over the original archive.
struct StagedArchive {
    path: std::path::PathBuf,
    committed: bool,
}

impl StagedArchive {
    fn copy_from(original: &std::path::Path) -> Result<Self, String> {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let parent = original
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let file_name = original
            .file_name()
            .ok_or_else(|| format!("invalid archive path: {}", original.display()))?
            .to_string_lossy();
        for _ in 0..100 {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".{file_name}.rar-rs-update-{}-{id}.tmp",
                std::process::id()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    drop(file);
                    if let Err(error) = std::fs::copy(original, &path) {
                        let _ = std::fs::remove_file(&path);
                        return Err(format!("stage archive copy: {error}"));
                    }
                    return Ok(Self {
                        path,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("create staged archive: {error}")),
            }
        }
        Err("could not allocate a unique staged archive path".into())
    }

    fn commit(mut self, original: &std::path::Path) -> CliResult<()> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("sync staged archive: {error}"))?;
        replace_archive_file(&self.path, original)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagedArchive {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn replace_archive_file(staged: &std::path::Path, original: &std::path::Path) -> CliResult<()> {
    std::fs::rename(staged, original).map_err(|error| format!("replace archive: {error}"))?;
    Ok(())
}

#[cfg(windows)]
fn replace_archive_file(staged: &std::path::Path, original: &std::path::Path) -> CliResult<()> {
    use std::os::windows::ffi::OsStrExt;

    let original: Vec<u16> = original.as_os_str().encode_wide().chain(Some(0)).collect();
    let staged: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
    let replaced = unsafe {
        windows_sys::Win32::Storage::FileSystem::ReplaceFileW(
            original.as_ptr(),
            staged.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        Err(format!("replace archive: {}", std::io::Error::last_os_error()).into())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn replace_archive_file(_staged: &std::path::Path, _original: &std::path::Path) -> CliResult<()> {
    Err("transactional archive replacement is not supported on this platform".into())
}

pub(crate) fn update_archive_transactionally(
    archive: &std::path::Path,
    operation: impl FnOnce(&std::path::Path) -> Result<(), String>,
) -> CliResult<()> {
    let staged = StagedArchive::copy_from(archive)?;
    operation(&staged.path)?;
    staged.commit(archive)
}
