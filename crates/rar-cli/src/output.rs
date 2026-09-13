//! Message routing and extraction-output helpers shared by the binaries.

/// Suppresses informational messages when `-idq` / `-inul` is given.
pub static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Sends informational messages to stderr instead of stdout when `-ierr`
/// is given.
pub static ERR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Whether extraction skips files that already exist.
///
/// WinRAR's default overwrite mode asks interactively; without a prompt we
/// follow its non-interactive outcome and skip. `-y` (assume yes) and
/// `-o+` overwrite, `-o-` skips, and `-or` auto-renames instead of
/// skipping.
pub fn skip_existing(overwrite: Option<&str>, assume_yes: bool, auto_rename: bool) -> bool {
    if auto_rename {
        return false;
    }
    match overwrite {
        Some("always") => false,
        Some("never") => true,
        _ => !assume_yes,
    }
}

/// `-ad[1,2]` destination mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendDir {
    /// No `-ad` switch: extract to the given destination.
    Off,
    /// `-ad`: destination directory + archive base name.
    Destination,
    /// `-ad1`: each archive's directory + archive base name (destination
    /// parameter ignored).
    ArchiveDir,
    /// `-ad2`: each archive's directory (destination parameter ignored).
    ArchiveDirFlat,
}

/// Parse the `--append-dir[=1|2]` argument.
pub fn parse_append_dir(spec: Option<&str>) -> Result<AppendDir, String> {
    match spec {
        None => Ok(AppendDir::Off),
        Some("") => Ok(AppendDir::Destination),
        Some("1") => Ok(AppendDir::ArchiveDir),
        Some("2") => Ok(AppendDir::ArchiveDirFlat),
        Some(other) => Err(format!("invalid -ad value: {other}")),
    }
}

/// Archive base name with a `.partN` volume suffix stripped.
fn archive_base(archive: &str) -> String {
    let mut base = std::path::Path::new(archive)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    if let Some(idx) = base.to_ascii_lowercase().find(".part")
        && base[idx + 5..].chars().all(|c| c.is_ascii_digit())
    {
        base.truncate(idx);
    }
    base
}

/// The archive's own directory (`""` parent means the current directory).
fn archive_dir(archive: &str) -> std::path::PathBuf {
    let parent = std::path::Path::new(archive)
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""));
    if parent.as_os_str().is_empty() {
        std::path::PathBuf::from(".")
    } else {
        parent.to_path_buf()
    }
}

/// Destination directory, honoring `-ad`/`-ad1`/`-ad2`.
pub fn extract_dest(dest: &str, archive: &str, mode: AppendDir) -> std::path::PathBuf {
    match mode {
        AppendDir::Off => std::path::PathBuf::from(dest),
        AppendDir::Destination => std::path::PathBuf::from(dest).join(archive_base(archive)),
        AppendDir::ArchiveDir => archive_dir(archive).join(archive_base(archive)),
        AppendDir::ArchiveDirFlat => archive_dir(archive),
    }
}
