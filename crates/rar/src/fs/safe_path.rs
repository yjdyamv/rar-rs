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

/// Resolve the target of a redirect (symlink / junction) member against the
/// directory that holds the link.
///
/// Returns the path components the target names **relative to the extraction
/// root**, so the caller can build the on-disk path without touching the
/// filesystem — the target of a link frequently does not exist yet when the
/// link itself is extracted, so any check that stats the path would reject
/// legitimate archives.
///
/// The resolution is lexical (`..` pops a component, `.` and empty
/// components are skipped) and rejects three shapes that can never resolve
/// inside the extraction root:
///
/// - **absolute targets** (`/etc/passwd`, `\\?\C:\…`),
/// - **Windows drive / ADS components** (`C:/…`, `name:stream`),
/// - **targets that walk above the root** (`../..`, `a/../../../b`).
///
/// A target that merely moves sideways inside the root (`sub/../target.txt`)
/// is accepted, matching the member-name policy in [`sanitize_archive_path`].
pub(crate) fn resolve_redirect_target(link_dir: &str, target: &str) -> RarResult<Vec<String>> {
    if target.is_empty() {
        return Err(RarError::Security("redirect target is empty".into()));
    }
    if target.contains('\0') {
        return Err(RarError::Security(
            "redirect target contains a NUL byte".into(),
        ));
    }
    let normalized = target.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(RarError::Security(format!(
            "absolute redirect target {target:?} rejected"
        )));
    }
    let mut parts: Vec<String> = link_dir
        .replace('\\', "/")
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .map(str::to_string)
        .collect();
    for component in normalized.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            if parts.pop().is_none() {
                return Err(RarError::Security(format!(
                    "redirect target {target:?} escapes the destination directory"
                )));
            }
        } else if component.contains(':') {
            return Err(RarError::Security(format!(
                "redirect target {target:?} contains a ':' (drive/ADS) component"
            )));
        } else {
            parts.push(component.to_string());
        }
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::{resolve_redirect_target, sanitize_archive_path};

    #[test]
    fn sanitize_rejects_unsafe_member_names() {
        assert!(sanitize_archive_path("a/b.txt").is_ok());
        assert!(sanitize_archive_path("").is_err());
        assert!(sanitize_archive_path("a\0b").is_err());
        assert!(sanitize_archive_path("/etc/passwd").is_err());
        assert!(sanitize_archive_path("a/../b").is_err());
        assert!(sanitize_archive_path("C:/x").is_err());
        // Redundant components are dropped, not rejected.
        assert_eq!(sanitize_archive_path("a/./b//c").unwrap(), "a/b/c");
    }

    #[test]
    fn redirect_targets_resolve_inside_the_root() {
        // A plain sibling link resolves next to the link.
        assert_eq!(
            resolve_redirect_target("dir", "target.txt").unwrap(),
            ["dir", "target.txt"]
        );
        // Sideways movement stays inside the root and is accepted.
        assert_eq!(
            resolve_redirect_target("dir", "sub/../target.txt").unwrap(),
            ["dir", "target.txt"]
        );
        // Walking up out of the link's own directory is fine as long as the
        // result is still under the root.
        assert_eq!(
            resolve_redirect_target("dir/nested", "../target.txt").unwrap(),
            ["dir", "target.txt"]
        );
    }

    #[test]
    fn redirect_targets_reject_escapes() {
        for target in [
            "../../outside.txt",
            "../../../outside.txt",
            // Windows spellings go through the same normalizer.
            "..\\..\\outside.txt",
            // Absolute paths and drive prefixes can never resolve inside.
            "/etc/passwd",
            "//server/share",
            "C:/Windows/win.ini",
            "\\\\?\\C:\\Windows",
            "name:stream",
        ] {
            let err = resolve_redirect_target("dir", target).unwrap_err();
            assert!(
                matches!(err, crate::error::RarError::Security(_)),
                "target {target:?} should be rejected as a security violation, got {err}"
            );
        }
        assert!(resolve_redirect_target("dir", "").is_err());
        assert!(resolve_redirect_target("dir", "a\0b").is_err());
    }
}
