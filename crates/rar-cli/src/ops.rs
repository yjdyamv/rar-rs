//! Shared archive operations for the `rar` and `unrar` binaries.
//!
//! Both binaries include this file with `#[path]`, so opening a reader,
//! member selection, extraction, listing and printing live in one place.
//! Only the per-binary switch surface (argument structs) and the message
//! wording stay in the binaries.

use std::io::Write;
use std::path::Path;

use crate::error::{CliError, CliResult};
use rar_rs::{ArchiveReader, EntryRef, ExtractOptions};

/// Open an archive for reading. A bad archive or wrong password becomes the
/// user-facing error the command runners return, keeping the library's
/// error category (wrong password, locked, format) for the exit code.
pub fn open_reader(path: impl AsRef<Path>, password: Option<&str>) -> CliResult<ArchiveReader> {
    let mut options = rar_rs::OpenOptions::new();
    if let Some(password) = password {
        options = options.password(password);
    }
    ArchiveReader::open_with(path, options).map_err(CliError::from)
}

/// UTC rendering of a unix timestamp (no chrono dependency).
fn format_unix_time(secs: u32) -> String {
    crate::time::format_local_time(secs)
}

/// The `Ratio` column shared by the standard and technical listings.
fn ratio_cell(entry: &EntryRef<'_>) -> String {
    if entry.is_dir() {
        "  dir".to_string()
    } else if entry.size() > 0 {
        format!(
            "{:.1}%",
            entry.compressed_size() as f64 / entry.size() as f64 * 100.0
        )
    } else {
        " 0.0%".to_string()
    }
}

/// Whether a member passes the requested name filter (empty = everything).
pub(crate) fn matches_filter(member: &str, names: &[String]) -> bool {
    names.is_empty()
        || names
            .iter()
            .any(|selector| crate::selector::name_matches(member, selector))
}

/// Bare list (`lb` / `vb`): member names only.
pub fn list_bare(rar: &ArchiveReader, names: &[String]) {
    for entry in rar
        .entries()
        .filter(|entry| matches_filter(entry.name(), names))
    {
        println!("{}", entry.name());
    }
}

/// Standard list (`l`): size / packed / ratio / method per member.
///
/// `totals` appends the footer line the `rar` binary prints; UnRAR's `l`
/// omits it.
pub fn list_entries(rar: &ArchiveReader, totals: bool, names: &[String]) {
    println!(
        "{:>10}  {:>10}  {:>6}  {:<8}  Name",
        "Size", "Packed", "Ratio", "Method"
    );
    println!("{}", "-".repeat(60));

    let mut total_size = 0u64;
    let mut total_packed = 0u64;
    let mut shown = 0usize;
    for entry in rar
        .entries()
        .filter(|entry| matches_filter(entry.name(), names))
    {
        println!(
            "{:>10}  {:>10}  {:>6}  {:<8}  {}",
            entry.size(),
            entry.compressed_size(),
            ratio_cell(&entry),
            entry.method_name(),
            entry.name()
        );
        if let Some(comment) = crate::output::format_comment_line(entry.comment()) {
            println!("      Comment: {comment}");
        }
        total_size += entry.size();
        total_packed += entry.compressed_size();
        shown += 1;
    }

    if totals {
        println!("{}", "-".repeat(60));
        let overall = if total_size > 0 {
            format!("{:.1}%", total_packed as f64 / total_size as f64 * 100.0)
        } else {
            " 0.0%".to_string()
        };
        println!(
            "{total_size:>10}  {total_packed:>10}  {overall:>6}  {:<8}  {} file(s)",
            "", shown
        );
    }
}

/// Technical list (`lt` / `vt`): mtime, sizes, ratio, CRC and method per
/// member, in the spirit of the official `lt`.
pub fn list_technical(rar: &ArchiveReader, names: &[String]) {
    println!(
        "{:>10}  {:>10}  {:>6}  {:>10}  {:<8}  {:<19}  Name",
        "Size", "Packed", "Ratio", "Checksum", "Method", "Modified"
    );
    println!("{}", "-".repeat(86));
    for entry in rar
        .entries()
        .filter(|entry| matches_filter(entry.name(), names))
    {
        let checksum = entry
            .crc32()
            .map(|crc| format!("{crc:08X}"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:>10}  {:>10}  {:>6}  {:>10}  {:<8}  {:<19}  {}",
            entry.size(),
            entry.compressed_size(),
            ratio_cell(&entry),
            checksum,
            entry.method_name(),
            format_unix_time(entry.mtime()),
            entry.name()
        );
    }
}

/// Extract the whole archive, or only the members whose name matches one of
/// `names` (full stored path or basename), using the same options. Returns
/// how many members were written. A name matching nothing is a hard error,
/// so a mistyped selector is never silently swallowed or treated as a
/// destination directory.
pub fn extract_members(
    rar: &mut ArchiveReader,
    dest: &Path,
    names: &[String],
    options: ExtractOptions,
) -> CliResult<usize> {
    if names.is_empty() {
        rar.extract_all_with_options(dest, options)
            .map_err(CliError::from)?;
        return Ok(rar.entries().len());
    }

    let wanted = crate::selector::select_entries(
        rar.entries()
            .filter(|entry| !entry.is_dir())
            .map(|entry| (entry.id(), entry.metadata().name())),
        names,
    );
    if wanted.is_empty() {
        return Err(CliError::with_code(
            format!(
                "no archive members matched the requested name(s): {}",
                names.join(", ")
            ),
            crate::error::EXIT_NO_FILES,
        ));
    }
    for &id in &wanted {
        let member = rar
            .entry(id)
            .map_err(|error| CliError::from(error).context("resolve archive member"))?
            .name()
            .to_string();
        rar.extract_entry_with_options(id, dest, options)
            .map_err(|error| CliError::from(error).context(format!("extract {member}")))?;
    }
    Ok(wanted.len())
}

/// Extract every file member to stdout, concatenated (`-so`), for piping.
/// Informational messages are suppressed by the caller so the stream stays
/// clean.
pub fn extract_to_stdout(
    rar: &mut ArchiveReader,
    names: &[String],
    max_dict_size: Option<u64>,
) -> CliResult<()> {
    let wanted = crate::selector::select_entries(
        rar.entries()
            .filter(|entry| !entry.is_dir())
            .map(|entry| (entry.id(), entry.metadata().name())),
        names,
    );
    if wanted.is_empty() && !names.is_empty() {
        return Err(CliError::with_code(
            format!(
                "no archive members matched the requested name(s): {}",
                names.join(", ")
            ),
            crate::error::EXIT_NO_FILES,
        ));
    }

    let options = ExtractOptions {
        max_unpacked_bytes: None,
        max_total_unpacked_bytes: None,
        max_dict_size: max_dict_size.or(Some(ExtractOptions::DEFAULT_MAX_DICT_SIZE)),
        ..Default::default()
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for id in wanted {
        let name = rar
            .entry(id)
            .map_err(|error| CliError::from(error).context("resolve archive member"))?
            .name()
            .to_string();
        rar.copy_entry_to_with_options(id, &mut out, options)
            .map_err(|error| CliError::from(error).context(format!("read {name}")))?;
    }
    out.flush().map_err(CliError::from)
}

/// Print one member, or every file member when `file` is `None`, to stdout
/// (`p`).
pub fn print_members(rar: &mut ArchiveReader, file: Option<&str>) -> CliResult<()> {
    let wanted: Vec<_> = if let Some(file) = file {
        rar.entries_named(file)
            .filter(|entry| !entry.is_dir())
            .map(|entry| entry.id())
            .collect()
    } else {
        rar.entries()
            .filter(|entry| !entry.is_dir())
            .map(|entry| entry.id())
            .collect()
    };
    if wanted.is_empty() && file.is_some() {
        let file = file.unwrap_or_default();
        return Err(CliError::with_code(
            format!("no archive members matched the requested name(s): {file}"),
            crate::error::EXIT_NO_FILES,
        ));
    }

    let options = ExtractOptions {
        max_unpacked_bytes: None,
        max_total_unpacked_bytes: None,
        ..Default::default()
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for id in wanted {
        let name = rar
            .entry(id)
            .map_err(|error| CliError::from(error).context("resolve archive member"))?
            .name()
            .to_string();
        rar.copy_entry_to_with_options(id, &mut out, options)
            .map_err(|error| CliError::from(error).context(name))?;
    }
    out.flush().map_err(CliError::from)
}
