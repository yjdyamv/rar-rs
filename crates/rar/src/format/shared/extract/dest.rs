//! Member destination handling: streams, timestamps, redirects and the
//! safe-path policy.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::engine::Engine;
use crate::error::{RarError, RarResult};
#[cfg(windows)]
use crate::format::rar5::write as rar5_write;
use crate::format::shared::entry_ext::RedirectSpec;
#[cfg(any(unix, windows))]
use crate::fs::safe_path::resolve_redirect_target;
use crate::fs::safe_path::sanitize_archive_path;

/// Restore a stored Unix mode (`chmod`), mirroring official UnRAR's rule.
///
/// UnRAR strips the set-user-ID and set-group-ID bits of *files* when the
/// extracting process is not root (`if (geteuid()!=0) FileAttr &=
/// ~(S_ISUID|S_ISGID);` in its `extract.cpp`), and keeps them when it is.
/// Those two bits are the only mode bits that change the identity a later
/// execution runs as, so an untrusted archive must not be able to install a
/// setuid executable as a side effect of extraction; a privileged extractor
/// asked for the stored mode and gets it.
///
/// Directories keep their stored mode, like UnRAR (its directory path calls
/// `SetFileAttr` without the `geteuid` test): the set-group-ID bit there only
/// selects the group of entries created inside, which confers no privilege
/// and is a commonly stored mode for shared directories. The sticky bit is
/// preserved everywhere, and the file-type bits are ignored by `chmod`.
///
/// Best-effort like the timestamp restoration: a filesystem that cannot
/// represent the mode must not fail an otherwise complete extraction.
#[cfg(unix)]
fn apply_unix_mode(dest_path: &Path, mode: u32, is_directory: bool) {
    use std::os::unix::fs::PermissionsExt;
    /// `S_ISUID | S_ISGID`.
    const SET_ID_BITS: u32 = 0o6000;
    let mode = if is_directory || extractor_is_privileged() {
        mode
    } else {
        mode & !SET_ID_BITS
    };
    let _ = fs::set_permissions(dest_path, fs::Permissions::from_mode(mode));
}

/// Whether the extracting process runs with effective uid 0. The same
/// `geteuid() != 0` test UnRAR uses before it strips stored set-ID bits.
#[cfg(unix)]
fn extractor_is_privileged() -> bool {
    // SAFETY: `geteuid` takes no arguments, has no side effects and cannot
    // fail; it only reads the process's effective uid.
    unsafe { libc::geteuid() == 0 }
}

/// Apply the attributes WinRAR restores on Windows: the stored DOS bits
/// for Windows-host members, the archive bit for other hosts (whose
/// attribute field holds a Unix mode), plus the directory bit. Failures
/// are deliberately ignored, like the timestamp restoration.
#[cfg(windows)]
fn apply_windows_attributes(hdr: &crate::model::FileHeader, dest_path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM, SetFileAttributesW,
    };

    const STORED_DOS_ATTRIBUTES: u64 = (FILE_ATTRIBUTE_READONLY
        | FILE_ATTRIBUTE_HIDDEN
        | FILE_ATTRIBUTE_SYSTEM
        | FILE_ATTRIBUTE_ARCHIVE) as u64;
    let mut attrs = match hdr.host_attributes() {
        crate::model::HostAttributes::Dos => (hdr.attributes & STORED_DOS_ATTRIBUTES) as u32,
        crate::model::HostAttributes::UnixMode(_) | crate::model::HostAttributes::Other => {
            if hdr.is_directory {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_ARCHIVE
            }
        }
    };
    if hdr.is_directory {
        attrs |= FILE_ATTRIBUTE_DIRECTORY;
    }
    if attrs == 0 {
        // `SetFileAttributesW` rejects a zero mask; `FILE_ATTRIBUTE_NORMAL`
        // is the documented way to say "no special attributes".
        attrs = FILE_ATTRIBUTE_NORMAL;
    }
    let wide: Vec<u16> = dest_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let _ = unsafe { SetFileAttributesW(wide.as_ptr(), attrs) };
}

/// Keep only the security zone from a Mark of the Web stream: the
/// `[ZoneTransfer]` section header and its `ZoneId=` line (WinRAR's `-om`
/// without the `1` modifier omits the potentially sensitive `ReferrerUrl`
/// and `HostUrl` fields).
#[cfg(windows)]
fn filter_motw_zone(stream: &[u8]) -> Option<Vec<u8>> {
    let text = String::from_utf8_lossy(stream);
    let mut out = String::new();
    let mut in_section = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("[ZoneTransfer]") {
            out.push_str("[ZoneTransfer]\r\n");
            in_section = true;
        } else if in_section && trimmed.to_ascii_lowercase().starts_with("zoneid=") {
            out.push_str(trimmed);
            out.push_str("\r\n");
        }
    }
    (!out.is_empty()).then(|| out.into_bytes())
}

/// Write the "STM" stream records attached to member `idx` onto the
/// extracted file (`file:name`); Windows only.
pub(crate) fn extract_member_streams(
    cx: &mut dyn Engine,
    idx: usize,
    dest_path: &Path,
) -> RarResult<()> {
    #[cfg(windows)]
    {
        for (name, data) in crate::format::rar5::extract::decode::read_member_streams(cx, idx)? {
            rar5_write::write_windows_stream(dest_path, &name, &data)?;
        }
    }
    #[cfg(not(windows))]
    {
        // The STM path is Windows-only; keep every parameter consumed so the
        // Linux/wasm `-D warnings` builds stay clean.
        let _ = (cx, idx, dest_path);
    }
    Ok(())
}

/// Copy the archive file's Mark of the Web (its `Zone.Identifier`
/// stream) onto an extracted file (WinRAR's `-om`); Windows only.
pub(crate) fn propagate_member_mark_of_the_web(cx: &dyn Engine, dest_path: &Path) {
    #[cfg(windows)]
    {
        let Some(options) = cx.read_ctx().motw.as_ref() else {
            return;
        };
        if let Some(extensions) = &options.extensions {
            let ext = dest_path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase());
            let matches = ext.is_some_and(|ext| extensions.iter().any(|wanted| wanted == &ext));
            if !matches {
                return;
            }
        }
        let Some(stream) = rar5_write::read_windows_stream(cx.path(), ":Zone.Identifier") else {
            return;
        };
        let data = if options.all_fields {
            stream
        } else {
            match filter_motw_zone(&stream) {
                Some(data) => data,
                None => return,
            }
        };
        let _ = rar5_write::write_windows_stream(dest_path, ":Zone.Identifier", &data);
    }
    #[cfg(not(windows))]
    {
        // MOTW is a Windows concept; keep both parameters consumed.
        let _ = (cx, dest_path);
    }
}

/// Restore a member's stored timestamps on the extracted file: the
/// modification time always (when the header carries one), plus access
/// time when requested via [`ExtractOptions`]. The creation time is set
/// through `SetFileTime` on Windows (std has no creation-time setter)
/// and is a no-op on Unix, where the change time cannot be set
/// (matching WinRAR's behavior).
pub(crate) fn apply_member_times(
    cx: &dyn Engine,
    hdr: &crate::model::FileHeader,
    dest_path: &Path,
) {
    let mut times = std::fs::FileTimes::new();
    let mut any = false;
    // RAR 1.3–4.x store local wall-clock time; the catalog holds it as
    // civil-as-UTC seconds, so convert back to an instant here. RAR5
    // regular members default to the Unix epoch when no time record
    // exists (like WinRAR); link redirects without one stay untouched.
    let time_known = crate::format::shared::entry_ext::file_header_has_mtime(hdr);
    if time_known {
        let secs = if hdr.uses_local_civil_time() {
            crate::format::shared::legacy_time::local_civil_to_epoch(hdr.mtime)
        } else {
            hdr.mtime
        };
        let mut mtime = UNIX_EPOCH + std::time::Duration::from_secs(u64::from(secs));
        if let Some(ns) = hdr.mtime_ns {
            mtime += std::time::Duration::from_nanos(ns as u64);
        }
        times = times.set_modified(mtime);
        any = true;
    }
    if cx.read_ctx().extract_options.set_access_time
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
    if cx.read_ctx().extract_options.set_creation_time
        && let Some((secs, ns)) = hdr.ctime
    {
        let _ = rar5_write::windows_set_creation_time(dest_path, secs, ns);
    }
}

/// Restore a member's stored attributes on the extracted path: the
/// Unix permission bits (`chmod`, with UnRAR's set-ID rule for
/// non-root extractors — see [`apply_unix_mode`]) for Unix-host members,
/// the DOS attributes (`SetFileAttributesW`) for Windows-host members.
///
/// Applied after the member's data (and its NTFS streams) are in place,
/// so a read-only attribute cannot block the writes that follow. The
/// application is best-effort like [`Self::apply_member_times`]: a
/// filesystem that cannot represent the attributes must not fail an
/// otherwise complete extraction.
pub(crate) fn apply_member_attributes(
    _cx: &dyn Engine,
    hdr: &crate::model::FileHeader,
    dest_path: &Path,
) {
    #[cfg(unix)]
    if let crate::model::HostAttributes::UnixMode(mode) = hdr.host_attributes() {
        apply_unix_mode(dest_path, mode, hdr.is_directory);
    }
    #[cfg(windows)]
    apply_windows_attributes(hdr, dest_path);
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (hdr, dest_path);
    }
}

/// Create `link` as a real NTFS junction (reparse tag
/// `IO_REPARSE_TAG_MOUNT_POINT`) to `target`, which must be an absolute
/// Windows path (a drive path, a `\\server\share` UNC path, or one already
/// carrying the `\??\` NT prefix).
///
/// A junction is what WinRAR/UnRAR write for a stored redirect of type 3,
/// and unlike a directory *symlink* it needs no
/// `SeCreateSymbolicLinkPrivilege`, so an unprivileged extraction succeeds.
/// The buffer layout follows UnRAR's `win32lnk.cpp` (`ReparseDataLength` = 4
/// lengths + both NUL-terminated names, the substitute name recorded without
/// its NUL and the print name starting past it).
///
/// Any failure (a relative target, an over-long path, a filesystem that
/// refuses reparse points) returns an error and leaves nothing behind, so
/// the caller can fall back to a directory symlink instead of losing the
/// member.
#[cfg(windows)]
fn create_windows_junction(link: &Path, target: &str) -> std::io::Result<()> {
    /// `MAXIMUM_REPARSE_DATA_BUFFER_SIZE`: the documented ceiling for
    /// `FSCTL_SET_REPARSE_POINT` input.
    const MAX_BUFFER: usize = 16 * 1024;
    /// `IO_REPARSE_TAG_MOUNT_POINT` (from `winnt.h`; the enabled
    /// `windows-sys` features do not carry it, and
    /// `crates/rar-cli/src/bin/rar/links.rs` defines the same value).
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

    // A junction's substitute name must be an NT path. `read_link` returns
    // the stored body verbatim, which is the `\??\`-prefixed spelling the
    // writer captured (or a plain `C:\...` / `\\server\share` when the
    // archive came from elsewhere); normalize the three accepted shapes and
    // reject anything relative, which has no meaning as a mount point.
    let (substitute, print) = if let Some(rest) = target.strip_prefix(r"\??\") {
        let print = match rest.strip_prefix("UNC\\") {
            Some(unc) => format!(r"\\{unc}"),
            None => rest.to_string(),
        };
        (target.to_string(), print)
    } else if let Some(unc) = target.strip_prefix(r"\\") {
        // `\\server\share` -> `\??\UNC\server\share`.
        (format!(r"\??\UNC\{unc}"), target.to_string())
    } else if target.len() >= 3
        && target.as_bytes()[1] == b':'
        && matches!(target.as_bytes()[2], b'\\' | b'/')
    {
        (format!(r"\??\{target}"), target.to_string())
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "junction target is not an absolute Windows path",
        ));
    };

    let substitute: Vec<u16> = substitute.encode_utf16().collect();
    let print: Vec<u16> = print.encode_utf16().collect();
    let substitute_bytes = (substitute.len() + 1) * 2; // NUL-terminated
    let print_bytes = (print.len() + 1) * 2;
    let data_length = 8 + substitute_bytes + print_bytes;
    let total = 8 + data_length;
    if total > MAX_BUFFER || data_length > u16::MAX as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "junction target is too long for a reparse point",
        ));
    }

    // Little-endian throughout: every Windows target is little-endian.
    let mut buffer = Vec::with_capacity(total);
    buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend_from_slice(&(data_length as u16).to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    buffer.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
    buffer.extend_from_slice(&((substitute.len() * 2) as u16).to_le_bytes());
    buffer.extend_from_slice(&(substitute_bytes as u16).to_le_bytes()); // PrintNameOffset
    buffer.extend_from_slice(&((print.len() * 2) as u16).to_le_bytes());
    for unit in substitute.iter().chain(std::iter::once(&0)) {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    for unit in print.iter().chain(std::iter::once(&0)) {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    debug_assert_eq!(buffer.len(), total);

    // A mount point is set on an existing, empty directory.
    fs::create_dir(link)?;
    if let Err(error) = set_reparse_point(link, &buffer) {
        let _ = fs::remove_dir(link);
        return Err(error);
    }
    Ok(())
}

/// Apply a prepared reparse buffer to the directory `path` via
/// `FSCTL_SET_REPARSE_POINT`.
#[cfg(windows)]
fn set_reparse_point(path: &Path, buffer: &[u8]) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    /// `FSCTL_SET_REPARSE_POINT` (`winioctl.h`).
    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call; the access,
    // share and flag values are the documented combination for opening a
    // directory reparse point without following it.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x8000_0000 | 0x4000_0000, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    let mut returned = 0u32;
    // SAFETY: `handle` is a live handle from `CreateFileW`; `buffer` is a
    // live slice and its exact length is passed; the output buffer is null
    // with length 0, which is what `FSCTL_SET_REPARSE_POINT` requires.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    let error = if ok == 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    // SAFETY: `handle` came from `CreateFileW` and is closed exactly once
    // here, on every path.
    unsafe { CloseHandle(handle) };
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Materialize a RAR5 file redirection (symlink, hardlink or file
/// copy) at `dest_path`.
pub(crate) fn extract_redirection(
    cx: &dyn Engine,
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
                check_link_target(cx, dest_dir, dest_path, &redir.target)?;
                std::os::unix::fs::symlink(&redir.target, dest_path)?;
            }
            #[cfg(windows)]
            {
                // Windows must know whether the link points at a
                // directory before it is created; a junction always does.
                let resolved = resolved_link_target(cx, dest_dir, dest_path, &redir.target)?;
                let is_dir = redir.redir_type == REDIR_WINDOWS_JUNCTION
                    || resolved.as_deref().is_some_and(Path::is_dir);
                if redir.redir_type == REDIR_WINDOWS_JUNCTION {
                    // Recreate a stored junction as a real NTFS mount
                    // point, like WinRAR/UnRAR: it needs no
                    // SeCreateSymbolicLinkPrivilege, so an unprivileged
                    // extraction succeeds. Fall back to a directory
                    // symlink when the target is not an absolute
                    // Windows path (or the buffer is refused), so the
                    // member is still extracted.
                    if create_windows_junction(dest_path, &redir.target).is_err() {
                        std::os::windows::fs::symlink_dir(&redir.target, dest_path)?;
                    }
                } else if is_dir {
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
            let target_path = safe_dest_path(cx, dest_dir, &redir.target)?;
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
fn check_link_target(
    cx: &dyn Engine,
    dest_dir: &Path,
    dest_path: &Path,
    target: &str,
) -> RarResult<()> {
    if cx.read_ctx().extract_options.safe_paths && !cx.read_ctx().extract_options.allow_unsafe_links
    {
        resolve_redirect_target(&link_dir_of(dest_dir, dest_path), target)?;
    }
    Ok(())
}

/// [`Self::check_link_target`] plus the on-disk path the target names
/// under the extraction root. `None` when the safe-path policy is off,
/// in which case the target is deliberately unconstrained.
#[cfg(windows)]
fn resolved_link_target(
    cx: &dyn Engine,
    dest_dir: &Path,
    dest_path: &Path,
    target: &str,
) -> RarResult<Option<PathBuf>> {
    if !cx.read_ctx().extract_options.safe_paths || cx.read_ctx().extract_options.allow_unsafe_links
    {
        return Ok(None);
    }
    let mut resolved = dest_dir.to_path_buf();
    for part in resolve_redirect_target(&link_dir_of(dest_dir, dest_path), target)? {
        resolved.push(part);
    }
    Ok(Some(resolved))
}

/// Compute the destination path for an entry name, applying the safe
/// path policy (sanitization + canonical containment check).
pub(crate) fn safe_dest_path(cx: &dyn Engine, dest_dir: &Path, name: &str) -> RarResult<PathBuf> {
    safe_dest_path_with(cx, dest_dir, name, cx.read_ctx().extract_options.safe_paths)
}

/// [`Self::safe_dest_path`] with an explicit safe-path policy, for
/// callers that resolve with options not installed in the read context
/// (see `members::resolve_dest_path_with`).
pub(crate) fn safe_dest_path_with(
    _cx: &dyn Engine,
    dest_dir: &Path,
    name: &str,
    safe_paths: bool,
) -> RarResult<PathBuf> {
    let sanitized = if safe_paths {
        sanitize_archive_path(name)?
    } else {
        name.replace('\\', "/")
    };
    let dest_path = dest_dir.join(&sanitized);
    if safe_paths && let Some(parent) = dest_path.parent() {
        // Containment is checked *before* the member's parent directory
        // is created: a rejected name then leaves nothing behind, and the
        // check cannot be satisfied by a directory we just created. The
        // parent usually does not exist yet, so its nearest existing
        // ancestor is canonicalized (resolving any symlink on the way)
        // and the remaining plain components are appended verbatim.
        fs::create_dir_all(dest_dir)?;
        let canon_dest = dest_dir.canonicalize()?;
        let canon_parent = canonicalize_with_tail(parent, dest_dir);
        if !canon_parent.starts_with(&canon_dest) {
            return Err(RarError::Security(format!(
                "entry {name:?} resolves outside the destination directory"
            )));
        }
    }
    Ok(dest_path)
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
