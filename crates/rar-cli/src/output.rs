//! Message routing and extraction-output helpers shared by the binaries.

/// Suppresses informational messages when `-idq` / `-inul` is given.
pub static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Sends informational messages to stderr instead of stdout when `-ierr`
/// is given.
pub static ERR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Destination directory, honoring `-ad` (append the archive base name as
/// a subdirectory; `.partN` volume suffixes are stripped).
pub fn extract_dest(dest: &str, archive: &str, append_dir: bool) -> std::path::PathBuf {
    let dest = std::path::PathBuf::from(dest);
    if !append_dir {
        return dest;
    }
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
    dest.join(base)
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
pub fn print_verbose_list(rar: &rar_rs::ArchiveReader) -> Result<(), String> {
    println!(
        "{:>10}  {:>10}  {:>6}  {:>10}  {:<8}  Name",
        "Size", "Packed", "Ratio", "Checksum", "Method"
    );
    println!("{}", "-".repeat(70));
    let mut total_size = 0u64;
    let mut total_packed = 0u64;
    for entry in rar.entries() {
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
