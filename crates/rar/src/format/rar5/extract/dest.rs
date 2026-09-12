//! Member destination handling: streams, timestamps, redirects and the
//! safe-path policy.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::archive::RarArchive;
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::RedirectSpec;
#[cfg(windows)]
use crate::format::rar5::write as rar5_write;
#[cfg(any(unix, windows))]
use crate::fs::safe_path::resolve_redirect_target;
use crate::fs::safe_path::sanitize_archive_path;

impl RarArchive {
    /// Write the "STM" stream records attached to member `idx` onto the
    /// extracted file (`file:name`); Windows only.
    pub(super) fn extract_member_streams(&mut self, idx: usize, dest_path: &Path) -> RarResult<()> {
        #[cfg(windows)]
        {
            use std::io::{Read, Seek, SeekFrom};

            use crate::archive::StreamRecord;
            use crate::format::shared::stream_mut;

            let owned: Vec<StreamRecord> = self
                .read_ctx()
                .streams
                .iter()
                .filter(|s| s.owner_index == idx)
                .cloned()
                .collect();
            for s in owned {
                // Read the stream payload (possibly RAR5-compressed). The
                // size comes from the "STM" service header, so it is capped
                // before it can drive an allocation, and narrowed with
                // `try_from` so a 32-bit target reports an error instead of
                // silently truncating the buffer.
                let limit = self.read_ctx().extract_options.metadata_limit();
                if s.data_size > limit {
                    return Err(RarError::LimitExceeded {
                        limit,
                        context: format!(
                            "NTFS stream {:?} declares {} packed bytes",
                            s.name, s.data_size
                        ),
                    });
                }
                let declared =
                    usize::try_from(s.data_size).map_err(|_| RarError::LimitExceeded {
                        limit,
                        context: format!(
                            "NTFS stream {:?} packed size does not fit in usize",
                            s.name
                        ),
                    })?;
                // The unpacked size drives the decode window allocation, so it
                // needs the same cap as the packed size: otherwise a crafted
                // "STM" record can request a multi-TiB window through
                // `decode_standalone`.
                if s.unpacked_size > limit {
                    return Err(RarError::LimitExceeded {
                        limit,
                        context: format!(
                            "NTFS stream {:?} declares {} unpacked bytes",
                            s.name, s.unpacked_size
                        ),
                    });
                }
                let mut packed = vec![0u8; declared];
                {
                    let stream = stream_mut(&mut self.stream)?;
                    stream.seek(SeekFrom::Start(s.data_offset))?;
                    stream.read_exact(&mut packed)?;
                }
                let data = if s.method == crate::format::rar5::COMP_METHOD_STORE {
                    packed
                } else {
                    crate::codec::decode_standalone(
                        &packed,
                        s.unpacked_size,
                        s.dict_size_log,
                        None,
                        crate::version::ArchiveVersion::V50,
                    )
                    .map_err(|e| RarError::Format(format!("stream decode: {e}")))?
                };
                rar5_write::write_windows_stream(dest_path, &s.name, &data)?;
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (idx, dest_path);
        }
        Ok(())
    }

    /// Restore a member's stored timestamps on the extracted file: the
    /// modification time always (when nonzero), plus access time when
    /// requested via [`ExtractOptions`]. The creation time is set through
    /// `SetFileTime` on Windows (std has no creation-time setter) and is a
    /// `SetFileTime` on Windows (std has no creation-time setter) and is a
    /// no-op on Unix, where the change time cannot be set (matching
    /// WinRAR's behavior).
    pub(super) fn apply_member_times(&self, hdr: &crate::model::FileHeader, dest_path: &Path) {
        let mut times = std::fs::FileTimes::new();
        let mut any = false;
        if hdr.mtime != 0 || hdr.mtime_ns.is_some() {
            let mut mtime = UNIX_EPOCH + std::time::Duration::from_secs(hdr.mtime as u64);
            if let Some(ns) = hdr.mtime_ns {
                mtime += std::time::Duration::from_nanos(ns as u64);
            }
            times = times.set_modified(mtime);
            any = true;
        }
        if self.read_ctx().extract_options.set_access_time
            && let Some((secs, ns)) = hdr.atime
        {
            let t = UNIX_EPOCH
                + std::time::Duration::from_secs(secs)
                + std::time::Duration::from_nanos(ns as u64);
            times = times.set_accessed(t);
            any = true;
        }
        if any {
            let _ = std::fs::File::options()
                .write(true)
                .open(dest_path)
                .and_then(|f| f.set_times(times));
        }
        #[cfg(windows)]
        if self.read_ctx().extract_options.set_creation_time
            && let Some((secs, ns)) = hdr.ctime
        {
            let _ = rar5_write::windows_set_creation_time(dest_path, secs, ns);
        }
    }

    /// Materialize a RAR5 file redirection (symlink, hardlink or file
    /// copy) at `dest_path`.
    pub(super) fn extract_redirection(
        &self,
        dest_dir: &Path,
        dest_path: &Path,
        redir: &RedirectSpec,
    ) -> RarResult<PathBuf> {
        const REDIR_UNIX_SYMLINK: u64 = 0x01;
        const REDIR_WINDOWS_SYMLINK: u64 = 0x02;
        const REDIR_WINDOWS_JUNCTION: u64 = 0x03;
        const REDIR_HARDLINK: u64 = 0x04;
        const REDIR_FILE_COPY: u64 = 0x05;
        match redir.redir_type {
            REDIR_UNIX_SYMLINK | REDIR_WINDOWS_SYMLINK | REDIR_WINDOWS_JUNCTION => {
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                // The link body is an attacker-controlled string, so it goes
                // through the safe-path policy just like member names do:
                // a link whose target escapes the destination would let a
                // later member (or any other consumer of the tree) write
                // through it. `safe_paths = false` opts out, which is the
                // documented "trusted archive" escape hatch.
                #[cfg(unix)]
                {
                    self.check_link_target(dest_dir, dest_path, &redir.target)?;
                    std::os::unix::fs::symlink(&redir.target, dest_path)?;
                }
                #[cfg(windows)]
                {
                    // Windows must know whether the link points at a
                    // directory before it is created; a junction always does.
                    let resolved = self.resolved_link_target(dest_dir, dest_path, &redir.target)?;
                    let is_dir = redir.redir_type == REDIR_WINDOWS_JUNCTION
                        || resolved.as_deref().is_some_and(Path::is_dir);
                    if is_dir {
                        std::os::windows::fs::symlink_dir(&redir.target, dest_path)?;
                    } else {
                        std::os::windows::fs::symlink_file(&redir.target, dest_path)?;
                    }
                }
                #[cfg(not(any(unix, windows)))]
                {
                    return Err(RarError::Unsupported(
                        "symbolic links are not supported on this platform".into(),
                    ));
                }
            }
            REDIR_HARDLINK | REDIR_FILE_COPY => {
                let target_path = self.safe_dest_path(dest_dir, &redir.target)?;
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                if redir.redir_type == REDIR_HARDLINK {
                    fs::hard_link(&target_path, dest_path)?;
                } else {
                    fs::copy(&target_path, dest_path)?;
                }
            }
            _ => {
                // Unknown redirection type: fall back to an empty regular
                // file so the archive remains extractable.
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(dest_path, [])?;
            }
        }
        Ok(dest_path.to_path_buf())
    }

    /// Reject a link target that escapes the extraction root (safe-path
    /// policy). The check is lexical, so it also accepts targets that do not
    /// exist on disk yet.
    #[cfg(unix)]
    fn check_link_target(&self, dest_dir: &Path, dest_path: &Path, target: &str) -> RarResult<()> {
        if self.read_ctx().extract_options.safe_paths {
            resolve_redirect_target(&Self::link_dir_of(dest_dir, dest_path), target)?;
        }
        Ok(())
    }

    /// [`Self::check_link_target`] plus the on-disk path the target names
    /// under the extraction root. `None` when the safe-path policy is off,
    /// in which case the target is deliberately unconstrained.
    #[cfg(windows)]
    fn resolved_link_target(
        &self,
        dest_dir: &Path,
        dest_path: &Path,
        target: &str,
    ) -> RarResult<Option<PathBuf>> {
        if !self.read_ctx().extract_options.safe_paths {
            return Ok(None);
        }
        let mut resolved = dest_dir.to_path_buf();
        for part in resolve_redirect_target(&Self::link_dir_of(dest_dir, dest_path), target)? {
            resolved.push(part);
        }
        Ok(Some(resolved))
    }

    /// The archive-relative, slash-separated directory that holds a link
    /// member — the base a redirect target resolves against.
    #[cfg(any(unix, windows))]
    fn link_dir_of(dest_dir: &Path, dest_path: &Path) -> String {
        dest_path
            .parent()
            .and_then(|parent| parent.strip_prefix(dest_dir).ok())
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default()
    }

    /// Compute the destination path for an entry name, applying the safe
    /// path policy (sanitization + canonical containment check).
    pub(super) fn safe_dest_path(&self, dest_dir: &Path, name: &str) -> RarResult<PathBuf> {
        let sanitized = if self.read_ctx().extract_options.safe_paths {
            sanitize_archive_path(name)?
        } else {
            name.replace('\\', "/")
        };
        let dest_path = dest_dir.join(&sanitized);
        if self.read_ctx().extract_options.safe_paths
            && let Some(parent) = dest_path.parent()
        {
            // Containment is checked *before* the member's parent directory
            // is created: a rejected name then leaves nothing behind, and the
            // check cannot be satisfied by a directory we just created. The
            // parent usually does not exist yet, so its nearest existing
            // ancestor is canonicalized (resolving any symlink on the way)
            // and the remaining plain components are appended verbatim.
            fs::create_dir_all(dest_dir)?;
            let canon_dest = dest_dir.canonicalize()?;
            let canon_parent = Self::canonicalize_with_tail(parent, dest_dir);
            if !canon_parent.starts_with(&canon_dest) {
                return Err(RarError::Security(format!(
                    "entry {name:?} resolves outside the destination directory"
                )));
            }
        }
        Ok(dest_path)
    }

    /// Canonicalize `path` even when its last components do not exist yet:
    /// the nearest existing ancestor is canonicalized — so a symlink placed
    /// by an earlier member is resolved — and the remaining components are
    /// appended verbatim, since they are already known to be plain names.
    ///
    /// `root` stops the upward walk; when it is reached without finding an
    /// existing ancestor the unresolved path is returned, which then fails
    /// the containment check instead of being trusted.
    fn canonicalize_with_tail(path: &Path, root: &Path) -> PathBuf {
        let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
        let mut cursor = path;
        let mut resolved = loop {
            if let Ok(canonical) = cursor.canonicalize() {
                break canonical;
            }
            match (cursor.file_name(), cursor.parent()) {
                (Some(component), Some(parent)) if cursor != root => {
                    tail.push(component);
                    cursor = parent;
                }
                _ => return path.to_path_buf(),
            }
        };
        for component in tail.iter().rev() {
            resolved.push(component);
        }
        resolved
    }
}
