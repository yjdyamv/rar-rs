//! Shared archive operations for the `rar` and `unrar` binaries.
//!
//! Both binaries include this file with `#[path]`, so opening a reader,
//! member selection, extraction, listing and printing live in one place.
//! Only the per-binary switch surface (argument structs) and the message
//! wording stay in the binaries.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::Ordering;

use crate::error::{CliError, CliResult};
use rar_rs::version::ArchiveVersion;
use rar_rs::{ArchiveReader, EntryRef, ExtractOptions};

/// Quiet labels (`-idq` / `-inul`) suppress the listing tables entirely,
/// like WinRAR's `l`/`v`/`lt`.
fn listing_quiet() -> bool {
    crate::output::QUIET.load(Ordering::Relaxed)
}

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

/// WinRAR's integer `Ratio` cell (`0%` for directories and empty members).
fn ratio_percent(size: u64, packed: u64) -> String {
    if size == 0 {
        "0%".to_string()
    } else {
        format!("{}%", u128::from(packed) * 100 / u128::from(size))
    }
}

/// The `Archive:` / `Details:` preamble of the table listings.
fn list_preamble(rar: &ArchiveReader, archive: &str) {
    println!("Archive: {archive}");
    println!("Details: {}", archive_details(rar));
    println!();
}

/// WinRAR's `Details:` container label.
fn archive_details(rar: &ArchiveReader) -> &'static str {
    let Some(entry) = rar.entries().next() else {
        return "RAR 5";
    };
    let version = entry.version();
    if version.is_rar13() {
        "RAR 1.4"
    } else if version.is_legacy() {
        "RAR 1.5"
    } else if version == ArchiveVersion::V70
        && entry.dict_size_bytes().is_some_and(|size| size > 4 << 30)
    {
        "RAR 7"
    } else {
        "RAR 5"
    }
}

/// DOS/Windows attribute flags in WinRAR's `..A.SH.` column order.
fn dos_attributes(attrs: u32) -> String {
    let mut chars = [b'.'; 7];
    if attrs & 0x20 != 0 {
        chars[2] = b'A';
    }
    if attrs & 0x10 != 0 {
        chars[3] = b'D';
    }
    if attrs & 0x04 != 0 {
        chars[4] = b'S';
    }
    if attrs & 0x02 != 0 {
        chars[5] = b'H';
    }
    if attrs & 0x01 != 0 {
        chars[6] = b'R';
    }
    String::from_utf8(chars.to_vec()).expect("ascii attribute cells")
}

/// POSIX mode string (`-rw-r--r--`) for Unix-host members.
fn unix_attributes(attrs: u64) -> String {
    // RAR5 stores the mode directly; RAR 1.5–4.x keeps it in the high word.
    let mode = if attrs & 0o170000 != 0 {
        attrs
    } else {
        attrs >> 16
    };
    let mut out = String::with_capacity(10);
    out.push(match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o010000 => 'p',
        0o020000 => 'c',
        0o060000 => 'b',
        _ => '-',
    });
    for (shift, special) in [(6u32, 0o4000u64), (3, 0o2000), (0, 0o1000)] {
        let bits = (mode >> shift) & 7;
        out.push(if bits & 4 != 0 { 'r' } else { '-' });
        out.push(if bits & 2 != 0 { 'w' } else { '-' });
        let exec = bits & 1 != 0;
        let flagged = mode & special != 0;
        out.push(match (exec, flagged) {
            (true, true) if shift == 0 => 't',
            (true, true) => 's',
            (false, true) if shift == 0 => 'T',
            (false, true) => 'S',
            (true, false) => 'x',
            (false, false) => '-',
        });
    }
    out
}

fn attributes_cell(entry: &EntryRef<'_>) -> String {
    if entry.version().is_rar13() || entry.host_os() != 1 {
        dos_attributes(entry.attributes() as u32)
    } else {
        unix_attributes(entry.attributes())
    }
}

/// WinRAR omits `Host OS` for RAR 1.3/1.4 members and resolves the rest to
/// the two host families the shared model keeps.
fn host_os_cell(entry: &EntryRef<'_>) -> Option<&'static str> {
    if entry.version().is_rar13() {
        return None;
    }
    Some(if entry.host_os() == 1 {
        "Unix"
    } else {
        "Windows"
    })
}

fn format_byte_count(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes.is_multiple_of(GIB) {
        format!("{}g", bytes / GIB)
    } else if bytes.is_multiple_of(MIB) {
        format!("{}m", bytes / MIB)
    } else if bytes.is_multiple_of(KIB) {
        format!("{}k", bytes / KIB)
    } else {
        format!("{bytes}")
    }
}

/// Dictionary size for the `-md=` suffix, per container family.
fn dictionary_size(entry: &EntryRef<'_>) -> Option<String> {
    if entry.is_dir() {
        return None;
    }
    let version = entry.version();
    if version.is_rar13() {
        return Some("64k".to_string());
    }
    if version.is_legacy() {
        // Window-bits field (7 marks a directory, handled above).
        let bits = entry.comp_dict_size();
        if bits > 6 {
            return None;
        }
        return Some(format_byte_count((64 * 1024) << bits));
    }
    let bytes = entry
        .dict_size_bytes()
        .unwrap_or((128 * 1024) << entry.comp_dict_size());
    Some(format_byte_count(bytes))
}

/// WinRAR's `Compression:` cell (`RAR 5.0(v50) -m3 -md=4m`).
fn compression_cell(entry: &EntryRef<'_>) -> String {
    let version = entry.version();
    let (family, tag) = if version.is_rar13() {
        // WinRAR labels the RAR 1.3/1.4 codec "RAR 5.0(v13)".
        ("RAR 5.0", "v13")
    } else if version.is_legacy() {
        ("RAR 1.5", version.as_str())
    } else if version == ArchiveVersion::V70
        && entry.dict_size_bytes().is_some_and(|size| size > 4 << 30)
    {
        ("RAR 7.0", "v70")
    } else {
        ("RAR 5.0", "v50")
    };
    let mut cell = format!("{family}({tag}) -m{}", entry.method());
    if let Some(dict) = dictionary_size(entry) {
        cell.push_str(" -md=");
        cell.push_str(&dict);
    }
    cell
}

/// The date/time column pair (`YYYY-MM-DD`, `HH:MM`) WinRAR prints.
fn list_stamp(entry: &EntryRef<'_>) -> (String, String) {
    let stamp = member_stamp(entry);
    (stamp[..10].to_string(), stamp[11..16].to_string())
}

/// Member timestamp in the container's own time convention: RAR5 stores UTC
/// (rendered local), RAR 1.3–4.x stores DOS local time (rendered as-is).
fn member_stamp(entry: &EntryRef<'_>) -> String {
    let version = entry.version();
    if version.is_rar13() || version.is_legacy() {
        crate::time::format_civil_time(entry.mtime())
    } else {
        crate::time::format_local_time(entry.mtime())
    }
}

/// Member name as WinRAR displays it: separators translated to the platform
/// convention while the stored form stays `/`.
fn display_name(name: &str) -> String {
    if cfg!(windows) {
        name.replace('/', "\\")
    } else {
        name.to_string()
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
    if listing_quiet() {
        return;
    }
    for entry in rar
        .entries()
        .filter(|entry| matches_filter(entry.name(), names))
    {
        println!("{}", display_name(entry.name()));
    }
}

/// Standard list (`l`), or the verbose variant with the packed/ratio/CRC
/// columns (`v`), in WinRAR's table shape.
pub fn list_entries(rar: &ArchiveReader, archive: &str, names: &[String], verbose: bool) {
    if listing_quiet() {
        return;
    }
    list_preamble(rar, archive);

    let mut total_size = 0u64;
    let mut total_packed = 0u64;
    let mut shown = 0usize;
    let mut row = |entry: &EntryRef<'_>| {
        let (date, time) = list_stamp(entry);
        let attrs = attributes_cell(entry);
        let name = display_name(entry.name());
        if verbose {
            let checksum = if entry.version().is_rar13() {
                "????????".to_string()
            } else {
                entry
                    .crc32()
                    .map(|crc| format!("{crc:08X}"))
                    .unwrap_or_default()
            };
            println!(
                "{attrs:>11} {:>10} {:>10} {:>4}  {date} {time}  {checksum:>8}  {name}",
                entry.size(),
                entry.compressed_size(),
                ratio_percent(entry.size(), entry.compressed_size()),
            );
        } else {
            println!("{attrs:>11} {:>10}  {date} {time}  {name}", entry.size(),);
        }
        total_size += entry.size();
        total_packed += entry.compressed_size();
        shown += 1;
    };

    if verbose {
        println!(" Attributes       Size     Packed Ratio    Date    Time   Checksum  Name");
        println!("----------- ---------- ---------- ----- ---------- -----  --------  ----");
    } else {
        println!(" Attributes       Size     Date    Time   Name");
        println!("----------- ----------  ---------- -----  ----");
    }
    for entry in rar
        .entries()
        .filter(|entry| matches_filter(entry.name(), names))
    {
        row(&entry);
    }
    let total_ratio = ratio_percent(total_size, total_packed);
    if verbose {
        println!("----------- ---------- ---------- ----- ---------- -----  --------  ----");
        println!(
            "{:>11} {:>10} {:>10} {:>4}  {:>10} {:>5}  {:>8}  {shown}",
            "", total_size, total_packed, total_ratio, "", "", ""
        );
    } else {
        println!("----------- ----------  ---------- -----  ----");
        println!(
            "{:>11} {:>10}  {:>10} {:>5}  {shown}",
            "", total_size, "", ""
        );
    }
    println!();
}

/// Technical list (`lt` / `vt`): WinRAR's per-member block shape.
pub fn list_technical(rar: &ArchiveReader, archive: &str, names: &[String]) {
    if listing_quiet() {
        return;
    }
    list_preamble(rar, archive);
    for entry in rar
        .entries()
        .filter(|entry| matches_filter(entry.name(), names))
    {
        println!("{:>12}: {}", "Name", display_name(entry.name()));
        println!(
            "{:>12}: {}",
            "Type",
            if entry.is_dir() { "Directory" } else { "File" }
        );
        if !entry.is_dir() {
            println!("{:>12}: {}", "Size", entry.size());
            println!("{:>12}: {}", "Packed size", entry.compressed_size());
            println!(
                "{:>12}: {}",
                "Ratio",
                ratio_percent(entry.size(), entry.compressed_size())
            );
        }
        println!(
            "{:>12}: {},{:09}",
            "Modified",
            member_stamp(&entry),
            entry.mtime_ns().unwrap_or(0)
        );
        println!("{:>12}: {}", "Attributes", attributes_cell(&entry));
        if !entry.version().is_rar13()
            && let Some(crc) = entry.crc32()
        {
            println!("{:>12}: {crc:08X}", "CRC32");
        }
        if let Some(host) = host_os_cell(&entry) {
            println!("{:>12}: {host}", "Host OS");
        }
        println!("{:>12}: {}", "Compression", compression_cell(&entry));
        println!();
    }
}

/// Expand `@listfiles` and resolve the extraction target. The last argument
/// is the destination when it ends with a path separator and `--dest` was
/// not given; `-op<path>` overrides it and `-ad` appends the archive name.
pub fn extract_names_and_dest(
    names: &[String],
    list_files: Option<&str>,
    dest: Option<&str>,
    output_path: Option<&str>,
    append_dir: Option<&str>,
    archive: &str,
) -> Result<(Vec<String>, std::path::PathBuf), String> {
    let mut names = crate::listfile::expand(names, list_files)?;
    let dest_given = dest.is_some();
    let mut dest = dest.unwrap_or(".").to_string();
    if !dest_given
        && let Some(last) = names.last()
        && (last.ends_with('/') || last.ends_with('\\'))
    {
        dest = names.pop().expect("checked above");
    }
    let base = output_path.unwrap_or(&dest);
    let mode = crate::output::parse_append_dir(append_dir)?;
    Ok((names, crate::output::extract_dest(base, archive, mode)))
}

/// Verify the whole archive or the selected members. A name matching no
/// member is a hard error.
pub fn verify_members(
    rar: &mut ArchiveReader,
    names: &[String],
) -> CliResult<rar_rs::VerificationReport> {
    if names.is_empty() {
        return rar.verify().map_err(|e| CliError::from(e).context("test"));
    }
    let ids = crate::selector::select_entries(
        rar.entries()
            .filter(|entry| !entry.is_dir())
            .map(|entry| (entry.id(), entry.metadata().name())),
        names,
    );
    if ids.is_empty() {
        return Err(CliError::with_code(
            format!(
                "no archive members matched the requested name(s): {}",
                names.join(", ")
            ),
            crate::error::EXIT_NO_FILES,
        ));
    }
    rar.verify_ids_with_options(&ids, rar_rs::ExtractOptions::default())
        .map_err(|e| CliError::from(e).context("test"))
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
