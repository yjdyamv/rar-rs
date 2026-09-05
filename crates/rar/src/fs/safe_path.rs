//! Safe-path policy for extraction: member names are sanitized before
//! they can reach the filesystem.

use crate::error::{RarError, RarResult};

/// Sanitize an archive member name for safe extraction.
///
/// Rejects empty names, absolute paths, `..` traversal components, NUL
/// bytes and Windows drive/ADS components (`:`). Backslashes are treated
/// as separators and redundant `.`/empty components are dropped.
pub(crate) fn sanitize_archive_path(name: &str) -> RarResult<String> {
    if name.is_empty() {
        return Err(RarError::Security("empty entry name".into()));
    }
    if name.contains('\0') {
        return Err(RarError::Security("entry name contains a NUL byte".into()));
    }
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(RarError::Security(format!(
            "absolute entry name {name:?} rejected"
        )));
    }
    let mut out = String::new();
    for comp in normalized.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            return Err(RarError::Security(format!(
                "entry name {name:?} contains a '..' traversal component"
            )));
        }
        if comp.contains(':') {
            return Err(RarError::Security(format!(
                "entry name {name:?} contains a ':' (drive/ADS) component"
            )));
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(comp);
    }
    if out.is_empty() {
        return Err(RarError::Security(format!(
            "entry name {name:?} resolves to an empty path"
        )));
    }
    Ok(out)
}
