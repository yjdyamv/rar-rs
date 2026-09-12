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
        if component_is_ambiguous(comp) {
            return Err(RarError::Security(format!(
                "entry name {name:?} contains the platform-ambiguous component {comp:?}"
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
/// Whether a path component means something different on this platform than
/// it does on POSIX.
///
/// Windows normalizes paths in two ways the generic checks above cannot see:
///
/// - trailing dots and spaces are stripped, so `".. "` opens as `".."` and a
///   name like `"report."` opens under a different name than it was written
///   with;
/// - the legacy device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`,
///   `LPT1`–`LPT9`) refer to devices regardless of any extension, so
///   `"CON.txt"` writes to the console rather than to a file.
///
/// Both are host hazards rather than archive-format hazards, so the check is
/// compiled for Windows only — POSIX accepts these names and nothing about
/// them is ambiguous there.
#[cfg(windows)]
fn component_is_ambiguous(component: &str) -> bool {
    if component.ends_with('.') || component.ends_with(' ') {
        return true;
    }
    // `CON.txt` is the console just as `CON` is, so only the stem matters.
    let stem = component.split('.').next().unwrap_or(component);
    let stem = stem.to_ascii_uppercase();
    let bytes = stem.as_bytes();
    if matches!(bytes, b"CON" | b"PRN" | b"AUX" | b"NUL") {
        return true;
    }
    bytes.len() == 4
        && matches!(&bytes[..3], b"COM" | b"LPT")
        && bytes[3].is_ascii_digit()
        && bytes[3] != b'0'
}

/// POSIX has no device names and no trailing-dot normalization, so every
/// component that survived the checks above is unambiguous.
#[cfg(not(windows))]
fn component_is_ambiguous(_component: &str) -> bool {
    false
}

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
        } else if component_is_ambiguous(component) {
            // Win32 normalizes these names (trailing dot/space stripping,
            // device names) when the link is later opened, so the lexical
            // containment check alone would not hold on disk.
            return Err(RarError::Security(format!(
                "redirect target {target:?} contains the platform-ambiguous component {component:?}"
            )));
        } else {
            parts.push(component.to_string());
        }
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::{component_is_ambiguous, resolve_redirect_target, sanitize_archive_path};

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

    /// On Windows these names either open a device or get normalized into a
    /// different path than the one that was written, so they must be refused
    /// rather than silently redirected.
    #[test]
    #[cfg(windows)]
    fn windows_ambiguous_components_are_rejected() {
        for component in [
            // Trailing dots and spaces are stripped by Win32, so `".. "`
            // opens as `".."`.
            ".. ", "...", "report.", "name ",
            // Legacy device names, with and without an extension.
            "CON", "con", "NUL.txt", "aux", "COM1", "com9", "LPT1.log",
        ] {
            assert!(
                component_is_ambiguous(component),
                "{component:?} should be ambiguous on Windows"
            );
            assert!(
                sanitize_archive_path(component).is_err(),
                "{component:?} should be rejected"
            );
        }
        // `con.txt.bak` is a device too (only the stem matters), so it is not
        // in this list; `COM0` and `LPT10` are not reserved names.
        for component in ["report", "COM", "COM0", "LPT10", "a.b"] {
            assert!(
                !component_is_ambiguous(component),
                "{component:?} should be a plain name"
            );
        }
    }

    /// POSIX has neither device names nor trailing-dot normalization, so the
    /// same names stay legal there — the check is platform-scoped on purpose.
    #[test]
    #[cfg(not(windows))]
    fn posix_accepts_names_windows_would_reject() {
        assert!(!component_is_ambiguous("CON"));
        assert!(!component_is_ambiguous("report."));
        assert_eq!(sanitize_archive_path("report.").unwrap(), "report.");
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

    /// Link targets are normalized by Win32 on open exactly like member
    /// names, so the ambiguous-name policy must cover them too.
    #[test]
    #[cfg(windows)]
    fn redirect_targets_reject_ambiguous_components() {
        for target in [".. ", "...", "CON", "NUL.txt", "report."] {
            let err = resolve_redirect_target("dir", target).unwrap_err();
            assert!(
                matches!(err, crate::error::RarError::Security(_)),
                "target {target:?} should be rejected, got {err}"
            );
        }
    }
}
