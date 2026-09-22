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

/// The "All" answer to the overwrite prompt, shared across the calls of one
/// extraction (`0` = still asking, `1` = overwrite every later destination).
pub type OverwriteAllState = std::sync::atomic::AtomicU8;

/// Ask the console whether to overwrite an existing destination, WinRAR's
/// `Y`/`N`/`A`/`R`/`Q` prompt. Reads a line from stdin; an unreadable or empty
/// line leaves the file untouched. Once `all` is set, later calls overwrite
/// without asking again.
pub fn prompt_overwrite(
    path: &std::path::Path,
    all: &OverwriteAllState,
) -> rar_rs::OverwriteChoice {
    use std::io::Write;
    use std::sync::atomic::Ordering;

    if all.load(Ordering::Relaxed) == 1 {
        return rar_rs::OverwriteChoice::Overwrite;
    }
    loop {
        print!(
            "{} already exists\nOverwrite? (Y)es, (N)o, (A)ll, (R)ename, (Q)uit: ",
            path.display()
        );
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            return rar_rs::OverwriteChoice::Skip;
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return rar_rs::OverwriteChoice::Overwrite,
            "n" | "no" => return rar_rs::OverwriteChoice::Skip,
            "a" | "all" => {
                all.store(1, Ordering::Relaxed);
                return rar_rs::OverwriteChoice::Overwrite;
            }
            "r" | "rename" => return rar_rs::OverwriteChoice::Rename,
            "q" | "quit" => return rar_rs::OverwriteChoice::Quit,
            _ => {}
        }
    }
}
