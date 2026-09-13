//! Message routing and extraction-output helpers shared by the binaries.

/// Suppresses informational messages when `-idq` / `-inul` is given.
pub static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Sends informational messages to stderr instead of stdout when `-ierr`
/// is given.
pub static ERR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
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
    if let Some(idx) = base.to_lowercase().find(".part")
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

/// Render a member comment for display: decode the raw bytes to text, replace
/// control/newline characters with spaces, and truncate to a single line.
/// Returns `None` for absent or empty comments.
pub fn format_comment_line(comment: Option<&[u8]>) -> Option<String> {
    let c = comment?;
    if c.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(c);
    let cleaned: String = text
        .chars()
        .map(|ch| {
            if ch == '\n' || ch == '\r' || (ch.is_control() && ch != '\t') {
                ' '
            } else {
                ch
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut s = trimmed.to_string();
    if s.chars().count() > 200 {
        s = format!("{}…", s.chars().take(200).collect::<String>());
    }
    Some(s)
}

/// Print a verbose listing (like `rar v` / `unrar v`).
pub fn print_verbose_list(rar: &rar_rs::ArchiveReader, names: &[String]) -> Result<(), String> {
    println!(
        "{:>10}  {:>10}  {:>6}  {:>10}  {:<8}  Name",
        "Size", "Packed", "Ratio", "Checksum", "Method"
    );
    println!("{}", "-".repeat(70));
    let mut total_size = 0u64;
    let mut total_packed = 0u64;
    for entry in rar
        .entries()
        .filter(|entry| crate::ops::matches_filter(entry.name(), names))
    {
        let ratio = if entry.is_dir() {
            "  dir".to_string()
        } else if entry.size() > 0 {
            format!(
                "{:.1}%",
                entry.compressed_size() as f64 / entry.size() as f64 * 100.0
            )
        } else {
            " 0.0%".to_string()
        };
        let checksum = entry
            .crc32()
            .map(|c| format!("{c:08X}"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:>10}  {:>10}  {:>6}  {:>10}  {:<8}  {}",
            entry.size(),
            entry.compressed_size(),
            ratio,
            checksum,
            entry.method_name(),
            entry.name()
        );
        if let Some(comment) = format_comment_line(entry.comment()) {
            println!("      Comment: {comment}");
        }
        total_size += entry.size();
        total_packed += entry.compressed_size();
    }
    println!("{}", "-".repeat(70));
    let overall = if total_size > 0 {
        format!("{:.1}%", total_packed as f64 / total_size as f64 * 100.0)
    } else {
        " 0.0%".to_string()
    };
    println!(
        "{total_size:>10}  {total_packed:>10}  {overall:>6}  {:<10}  {} file(s)",
        "",
        rar.entries().len()
    );
    Ok(())
}
