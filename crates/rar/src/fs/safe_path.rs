//! Safe-path policy for extraction: member names are sanitized before
//! they can reach the filesystem.

use crate::error::{RarError, RarResult};

/// Sanitize an archive member name for safe extraction.
///
/// Rejects empty names, absolute paths, `..` traversal components and NUL
/// bytes. Backslashes are treated as separators and redundant `.`/empty
/// components are dropped.
///
/// Windows-only name hazards are **corrected** the way WinRAR does rather
/// than rejected (see [`sanitize_component`]): `:` becomes `_`, a trailing
/// dot/space becomes `_`, and an exact reserved device name gets a leading
/// `_`. With `allow_incompatible` (WinRAR's `-oni`) the name is kept as
/// written apart from `:`.
/// Test-only convenience wrapper: the production paths use
/// [`sanitize_archive_path_corrected`] so they can report a device-name
/// correction.
#[cfg(test)]
pub(crate) fn sanitize_archive_path(name: &str, allow_incompatible: bool) -> RarResult<String> {
    sanitize_archive_path_corrected(name, allow_incompatible).map(|(name, _)| name)
}

/// [`sanitize_archive_path`], also reporting whether a reserved device name
/// was corrected. WinRAR prints its `Attempting to correct the invalid file
/// or directory name` warning for exactly that case (the trailing-dot and
/// colon corrections are silent).
pub(crate) fn sanitize_archive_path_corrected(
    name: &str,
    allow_incompatible: bool,
) -> RarResult<(String, bool)> {
    if name.is_empty() {
        return Err(RarError::security("empty entry name"));
    }
    if name.contains('\0') {
        return Err(RarError::security("entry name contains a NUL byte"));
    }
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(RarError::security(format!(
            "absolute entry name {name:?} rejected"
        )));
    }
    let mut out = String::new();
    let mut corrected = false;
    for comp in normalized.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            return Err(RarError::security(format!(
                "entry name {name:?} contains a '..' traversal component"
            )));
        }
        let (comp, fixed) = sanitize_component(comp, allow_incompatible);
        corrected |= fixed;
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&comp);
    }
    if out.is_empty() {
        return Err(RarError::security(format!(
            "entry name {name:?} resolves to an empty path"
        )));
    }
    Ok((out, corrected))
}

/// Correct one Windows-hostile path component the way WinRAR's default does:
///
/// - `:` becomes `_` (it names a drive or an alternate data stream, never a
///   file), even under `-oni`;
/// - a trailing dot or space becomes `_` (Win32 strips it otherwise, so a
///   literal name cannot round-trip); WinRAR's `-oni` refuses that case
///   outright, we keep the safe correction;
/// - an exact reserved device name (`CON`, `AUX`, `NUL`, `COM1`–`COM9`, ...)
///   gets a leading `_`, unless `-oni` asked for the name as written.
///
/// A device name *with* an extension (`aux.txt`) is left alone: it is an
/// ordinary file on modern Windows, and WinRAR does not correct it either.
/// POSIX has none of these hazards, so the component is returned unchanged.
#[cfg(windows)]
fn sanitize_component(component: &str, allow_incompatible: bool) -> (String, bool) {
    let mut result = component.replace(':', "_");
    if result.ends_with('.') || result.ends_with(' ') {
        result.pop();
        result.push('_');
    }
    let mut corrected = false;
    if !allow_incompatible && is_reserved_device_name(&result) {
        result.insert(0, '_');
        corrected = true;
    }
    (result, corrected)
}

/// POSIX has no device names, trailing-dot normalization or ADS semantics, so
/// every component is already unambiguous and stays as written.
#[cfg(not(windows))]
fn sanitize_component(component: &str, _allow_incompatible: bool) -> (String, bool) {
    (component.to_string(), false)
}

/// Whether the whole component is one of Windows' reserved device names.
/// Only an exact match counts (`aux.txt` is a plain file).
#[cfg(windows)]
fn is_reserved_device_name(component: &str) -> bool {
    let upper = component.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    if matches!(
        bytes,
        b"CON" | b"PRN" | b"AUX" | b"NUL" | b"CONIN$" | b"CONOUT$"
    ) {
        return true;
    }
    bytes.len() == 4
        && matches!(&bytes[..3], b"COM" | b"LPT")
        && bytes[3].is_ascii_digit()
        && bytes[3] != b'0'
}

/// Whether a path component means something different on this platform than
/// it does on POSIX.
///
/// Windows normalizes paths in three ways the generic checks above cannot
/// see:
///
/// - a `:` names a drive (`C:`) or an alternate data stream (`name:stream`),
///   so the component does not resolve to the literal name written;
/// - trailing dots and spaces are stripped, so `".. "` opens as `".."` and a
///   name like `"report."` opens under a different name than it was written
///   with;
/// - the legacy device names (`CON`, `PRN`, `AUX`, `NUL`, `CONIN$`,
///   `CONOUT$`, `COM1`–`COM9`, `LPT1`–`LPT9`) refer to devices regardless of
///   any extension or trailing dot/space, so `"CON.txt"` and `"CON .txt"`
///   write to the console rather than to a file.
///
/// All three are host hazards rather than archive-format hazards, so the
/// check is compiled for Windows only — POSIX accepts these names (`:` in
/// particular is an ordinary filename character and official unrar extracts
/// `foo:bar` on Linux) and nothing about them is ambiguous there.
#[cfg(windows)]
fn component_is_ambiguous(component: &str) -> bool {
    if component.contains(':') {
        return true;
    }
    if component.ends_with('.') || component.ends_with(' ') {
        return true;
    }
    // `CON.txt` is the console just as `CON` is, so only the stem matters —
    // and Win32 strips trailing dots and spaces *before* it tests the
    // reserved names, so a stem of `"CON "` (from `CON .txt`) is the device
    // too, as are the console/pipe names `CONIN$` and `CONOUT$`.
    let stem = component.split('.').next().unwrap_or(component);
    let stem = stem.trim_end_matches(['.', ' ']).to_ascii_uppercase();
    let bytes = stem.as_bytes();
    if matches!(
        bytes,
        b"CON" | b"PRN" | b"AUX" | b"NUL" | b"CONIN$" | b"CONOUT$"
    ) {
        return true;
    }
    bytes.len() == 4
        && matches!(&bytes[..3], b"COM" | b"LPT")
        && bytes[3].is_ascii_digit()
        && bytes[3] != b'0'
}

/// POSIX has no device names and no trailing-dot normalization, so every
/// component that survived the checks above is unambiguous.
#[cfg(unix)]
fn component_is_ambiguous(_component: &str) -> bool {
    false
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
/// - **Windows drive / ADS components** (`C:/…`, `name:stream`) and the
///   other host-ambiguous names checked by [`component_is_ambiguous`],
/// - **targets that walk above the root** (`../..`, `a/../../../b`).
///
/// A target that merely moves sideways inside the root (`sub/../target.txt`)
/// is accepted, matching the member-name policy in [`sanitize_archive_path`].
#[cfg(any(unix, windows))]
pub(crate) fn resolve_redirect_target(link_dir: &str, target: &str) -> RarResult<Vec<String>> {
    if target.is_empty() {
        return Err(RarError::security("redirect target is empty"));
    }
    if target.contains('\0') {
        return Err(RarError::security("redirect target contains a NUL byte"));
    }
    let normalized = target.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(RarError::security(format!(
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
                return Err(RarError::security(format!(
                    "redirect target {target:?} escapes the destination directory"
                )));
            }
        } else if component_is_ambiguous(component) {
            // Win32 normalizes these names (drive/ADS colons, trailing
            // dot/space stripping, device names) when the link is later
            // opened, so the lexical containment check alone would not hold
            // on disk.
            return Err(RarError::security(format!(
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
    use super::{resolve_redirect_target, sanitize_archive_path};

    #[test]
    fn sanitize_rejects_unsafe_member_names() {
        assert!(sanitize_archive_path("a/b.txt", false).is_ok());
        assert!(sanitize_archive_path("", false).is_err());
        assert!(sanitize_archive_path("a\0b", false).is_err());
        assert!(sanitize_archive_path("/etc/passwd", false).is_err());
        assert!(sanitize_archive_path("a/../b", false).is_err());
        // Redundant components are dropped, not rejected.
        assert_eq!(sanitize_archive_path("a/./b//c", false).unwrap(), "a/b/c");
    }

    /// Windows **corrects** these names the way WinRAR's default does instead
    /// of rejecting them: `:` becomes `_`, a trailing dot/space becomes `_`,
    /// and an exact device name gets a leading `_`. Under `-oni` (`true`)
    /// only the colon is still replaced.
    #[test]
    #[cfg(windows)]
    fn windows_ambiguous_components_are_corrected() {
        for (input, default, oni) in [
            // Colons name drives / alternate data streams on Windows.
            ("C:", "C_", "C_"),
            ("C:/x", "C_/x", "C_/x"),
            ("name:stream", "name_stream", "name_stream"),
            ("foo:bar", "foo_bar", "foo_bar"),
            // Trailing dots and spaces are stripped by Win32; WinRAR replaces
            // the final character with `_`.
            ("report.", "report_", "report_"),
            ("name ", "name_", "name_"),
            ("a..", "a._", "a._"),
            ("a ..", "a ._", "a ._"),
            // Exact legacy device names get a leading `_`; `-oni` keeps them
            // as written. An extension makes an ordinary file either way.
            ("aux", "_aux", "aux"),
            ("AUX", "_AUX", "AUX"),
            ("CON", "_CON", "CON"),
            ("NUL", "_NUL", "NUL"),
            ("COM1", "_COM1", "COM1"),
            ("com9", "_com9", "com9"),
            ("LPT1", "_LPT1", "LPT1"),
            ("CONIN$", "_CONIN$", "CONIN$"),
            ("aux.txt", "aux.txt", "aux.txt"),
            ("NUL.log", "NUL.log", "NUL.log"),
            // The trailing dot is corrected *before* the device test, so this
            // is a plain name afterwards.
            ("con.", "con_", "con_"),
        ] {
            assert_eq!(
                sanitize_archive_path(input, false).unwrap(),
                default,
                "{input:?} default"
            );
            assert_eq!(
                sanitize_archive_path(input, true).unwrap(),
                oni,
                "{input:?} under -oni"
            );
        }
        // `.. ` is corrected to `.._`, so it can never walk out; the exact
        // `..` component is still rejected.
        assert_eq!(sanitize_archive_path(".. ", false).unwrap(), ".._");
        assert!(sanitize_archive_path("../x", false).is_err());
    }

    /// POSIX has neither device names, trailing-dot normalization nor ADS
    /// semantics, so the same names stay legal there — the correction is
    /// platform-scoped on purpose. `foo:bar` in particular is an ordinary
    /// filename on Linux, matching official unrar.
    #[test]
    #[cfg(unix)]
    fn posix_accepts_names_windows_would_correct() {
        use super::component_is_ambiguous;
        assert!(!component_is_ambiguous("CON"));
        assert!(!component_is_ambiguous("report."));
        assert!(!component_is_ambiguous("foo:bar"));
        assert_eq!(sanitize_archive_path("report.", false).unwrap(), "report.");
        assert_eq!(sanitize_archive_path("foo:bar", false).unwrap(), "foo:bar");
        assert_eq!(sanitize_archive_path("C:/x", false).unwrap(), "C:/x");
        assert_eq!(
            resolve_redirect_target("dir", "foo:bar").unwrap(),
            ["dir", "foo:bar"]
        );
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
            // Absolute paths can never resolve inside. (`C:/Windows/win.ini`
            // is absolute on Windows; on POSIX it is a relative name and is
            // covered by the ambiguity test there.)
            "/etc/passwd",
            "//server/share",
            "\\\\?\\C:\\Windows",
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
        for target in [
            ".. ",
            "...",
            "CON",
            "NUL.txt",
            "report.",
            "name:stream",
            "C:/Windows/win.ini",
        ] {
            let err = resolve_redirect_target("dir", target).unwrap_err();
            assert!(
                matches!(err, crate::error::RarError::Security(_)),
                "target {target:?} should be rejected, got {err}"
            );
        }
    }
}
