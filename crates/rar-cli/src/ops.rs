//! Shared archive operations for the `rar` and `unrar` binaries.
//!
//! Both binaries include this file with `#[path]`, so opening a reader,
//! member selection, extraction, listing and printing live in one place.
//! Only the per-binary switch surface (argument structs) and the message
//! wording stay in the binaries.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use crate::error::{CliError, CliResult};
use crate::output;
use rar_rs::version::ArchiveVersion;
use rar_rs::{ArchiveReader, EntryId, EntryRef, ExtractOptions, ExtractionReport};

/// Quiet labels (`-idq` / `-inul`) suppress the listing tables entirely,
/// like WinRAR's `l`/`v`/`lt`.
fn listing_quiet() -> bool {
    crate::output::QUIET.load(Ordering::Relaxed)
}

/// Open an archive for reading. A bad archive or wrong password becomes the
/// user-facing error the command runners return, keeping the library's
/// error category (wrong password, locked, format) for the exit code.
///
/// Like WinRAR, a missing name without an extension is retried with `.rar`
/// (`rar l exa` reads `exa.rar`); a `.part1.rar` first volume is accepted
/// the same way. A genuinely missing archive keeps the original error.
pub fn open_reader(path: impl AsRef<Path>, password: Option<&str>) -> CliResult<ArchiveReader> {
    let path = path.as_ref();
    let open = |candidate: &Path| {
        let mut options = rar_rs::OpenOptions::new();
        if let Some(password) = password {
            options = options.password(password);
        }
        ArchiveReader::open_with(candidate, options)
    };
    let first = match open(path) {
        Ok(rar) => return Ok(rar),
        Err(error) => error,
    };
    if !path.exists() && path.extension().is_none() {
        for candidate in inferred_archive_paths(path) {
            if let Ok(rar) = open(&candidate) {
                return Ok(rar);
            }
        }
    }
    Err(CliError::from(first))
}

/// The archive names WinRAR infers for an extension-less read request:
/// `<path>.rar`, then the `<path>.part1.rar` first volume.
///
/// Both names are built with raw `OsString`s, so a non-UTF-8 host path
/// (legal on Unix) is probed exactly as spelled; a `display()` /
/// `to_string_lossy` hop would rewrite it to U+FFFD and never resolve.
fn inferred_archive_paths(path: &Path) -> Vec<PathBuf> {
    let mut rar = path.to_path_buf();
    rar.set_extension("rar");
    let part1 = match path.file_name() {
        Some(name) => {
            let mut name = name.to_os_string();
            name.push(".part1.rar");
            path.with_file_name(name)
        }
        None => {
            let mut name = path.as_os_str().to_os_string();
            name.push(".part1.rar");
            PathBuf::from(name)
        }
    };
    vec![rar, part1]
}

/// WinRAR's integer `Ratio` cell (`0%` for directories and empty members).
fn ratio_percent(size: u64, packed: u64) -> String {
    if size == 0 {
        "0%".to_string()
    } else {
        format!("{}%", u128::from(packed) * 100 / u128::from(size))
    }
}

/// The volume a listing was opened from: WinRAR's table listings show only
/// the members with data in that volume, with per-fragment columns for
/// members split across volumes.
#[derive(Clone, Copy)]
struct VolumeView {
    index: usize,
    count: usize,
}

impl VolumeView {
    fn of(archive: &str) -> Self {
        let volumes = rar_rs::discover_volumes(std::path::Path::new(archive));
        if volumes.len() <= 1 {
            return Self { index: 0, count: 1 };
        }
        let opened = std::path::Path::new(archive).file_name();
        let index = opened
            .and_then(|name| {
                let name = name.to_string_lossy();
                volumes.iter().position(|volume| {
                    volume.file_name().is_some_and(|candidate| {
                        candidate.to_string_lossy().eq_ignore_ascii_case(&name)
                    })
                })
            })
            .unwrap_or(0);
        Self {
            index,
            count: volumes.len(),
        }
    }

    /// The chunk of `entry` that lives in the opened volume, with its
    /// position in the member's chunk list and its stored checksum; `None`
    /// when the member has no data in this volume (WinRAR omits it).
    fn fragment(&self, entry: &EntryRef<'_>) -> Option<(usize, u64, Option<u32>)> {
        let chunks = entry.chunks();
        if self.count <= 1 {
            let chunk = chunks.first()?;
            return Some((0, chunk.packed_size, chunk.crc32_val));
        }
        let position = chunks
            .iter()
            .position(|chunk| chunk.volume_index == self.index)?;
        let chunk = &chunks[position];
        Some((position, chunk.packed_size, chunk.crc32_val))
    }

    fn includes(&self, entry: &EntryRef<'_>) -> bool {
        self.count <= 1 || self.fragment(entry).is_some()
    }

    /// WinRAR's Ratio cell for a fragment: `-->`/`<->`/`<--` for members
    /// split across volumes, the integer percentage otherwise.
    fn ratio_cell(&self, entry: &EntryRef<'_>, position: usize, packed: u64) -> String {
        let chunks = entry.chunks();
        if self.count > 1 && chunks.len() > 1 {
            return if position == 0 {
                "-->".to_string()
            } else if position + 1 == chunks.len() {
                "<--".to_string()
            } else {
                "<->".to_string()
            };
        }
        ratio_percent(entry.size(), packed)
    }

    /// Whether this fragment starts the member in the opened volume; the
    /// totals rows count only those members, like WinRAR.
    fn starts_here(&self, position: usize) -> bool {
        position == 0
    }

    /// Whether the fragment is the member's last chunk (the one carrying
    /// the whole-member checksum).
    fn is_final_fragment(&self, entry: &EntryRef<'_>, position: usize) -> bool {
        position + 1 == entry.chunks().len()
    }
}

/// The `Archive:` / `Details:` preamble of the table listings.
fn list_preamble(rar: &ArchiveReader, archive: &str, view: &VolumeView) {
    println!("Archive: {archive}");
    println!("Details: {}", archive_details(rar, view));
    println!();
}

/// WinRAR's `Details:` container label with its `, solid` / `, volume`
/// annotations.
fn archive_details(rar: &ArchiveReader, view: &VolumeView) -> String {
    let first = rar.entries().next();
    let version = first.map(|entry| entry.version());
    let mut details = match version {
        Some(rc) if rc.is_rar13() => "RAR 1.4",
        Some(rc) if rc.is_legacy() => "RAR 1.5",
        Some(ArchiveVersion::V70)
            if first
                .and_then(|entry| entry.dict_size_bytes())
                .is_some_and(|size| size > 4 << 30) =>
        {
            "RAR 7"
        }
        _ => "RAR 5",
    }
    .to_string();
    if rar.is_solid() {
        details.push_str(", solid");
    }
    if view.count > 1 {
        let rar5 = matches!(version, Some(rc) if !rc.is_rar13() && !rc.is_legacy());
        if rar5 {
            details.push_str(&format!(", volume {}", view.index + 1));
        } else {
            details.push_str(", volume");
        }
    }
    details
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
    let unix_host = if entry.version().is_rar13() {
        false
    } else if entry.version().is_legacy() {
        // Raw RAR4 host table: 3 = Unix, 4 = Mac.
        matches!(entry.host_os_raw(), 3 | 4)
    } else {
        entry.host_os() == 1
    };
    if unix_host {
        unix_attributes(entry.attributes())
    } else {
        dos_attributes(entry.attributes() as u32)
    }
}

/// WinRAR omits `Host OS` for RAR 1.3/1.4 members; legacy members show
/// their raw host (DOS/OS-2/Windows/Unix/Mac).
fn host_os_cell(entry: &EntryRef<'_>) -> Option<&'static str> {
    if entry.version().is_rar13() {
        return None;
    }
    Some(entry.host_os_name())
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

/// The date/time column pair (`YYYY-MM-DD`, `HH:MM`) WinRAR prints;
/// `????-??-??` / `??:??` when the member carries no time.
fn list_stamp(entry: &EntryRef<'_>) -> (String, String) {
    if !entry.has_mtime() {
        return ("????-??-??".to_string(), "??:??".to_string());
    }
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

/// Bare list (`lb` / `vb`): member names only (of the opened volume).
pub fn list_bare(rar: &ArchiveReader, archive: &str, names: &[String]) {
    if listing_quiet() {
        return;
    }
    let view = VolumeView::of(archive);
    for entry in rar
        .entries()
        .filter(|entry| matches_filter(entry.name(), names) && view.includes(entry))
    {
        println!("{}", display_name(entry.name()));
    }
}

/// Standard list (`l`), or the verbose variant with the packed/ratio/CRC
/// columns (`v`), in WinRAR's table shape. On a volume set only the members
/// with data in the opened volume are listed, with per-fragment columns.
pub fn list_entries(rar: &ArchiveReader, archive: &str, names: &[String], verbose: bool) {
    if listing_quiet() {
        return;
    }
    let view = VolumeView::of(archive);
    list_preamble(rar, archive, &view);

    let mut total_size = 0u64;
    let mut total_packed = 0u64;
    let mut shown = 0usize;
    let mut row =
        |entry: &EntryRef<'_>, position: usize, packed: u64, fragment_crc: Option<u32>| {
            let (date, time) = list_stamp(entry);
            let attrs = attributes_cell(entry);
            let name = display_name(entry.name());
            if verbose {
                let checksum = if entry.version().is_rar13() {
                    if entry.is_dir() {
                        String::new()
                    } else {
                        "????????".to_string()
                    }
                } else if view.is_final_fragment(entry, position) {
                    entry
                        .crc32()
                        .map(|crc| format!("{crc:08X}"))
                        .unwrap_or_default()
                } else {
                    fragment_crc
                        .map(|crc| format!("{crc:08X}"))
                        .unwrap_or_default()
                };
                println!(
                    "{attrs:>11} {:>10} {:>10} {:>4}  {date} {time}  {checksum:>8}  {name}",
                    entry.size(),
                    packed,
                    view.ratio_cell(entry, position, packed),
                );
            } else {
                println!("{attrs:>11} {:>10}  {date} {time}  {name}", entry.size(),);
            }
            if view.starts_here(position) {
                total_size += entry.size();
                shown += 1;
            }
            total_packed += packed;
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
        .filter(|entry| matches_filter(entry.name(), names) && view.includes(entry))
    {
        let (position, packed, fragment_crc) = view.fragment(&entry).expect("included above");
        row(&entry, position, packed, fragment_crc);
    }
    // Legacy `.partN.rar` sets annotate the totals row like WinRAR.
    let volume_cell =
        (view.count > 1 && rar.is_new_numbering()).then(|| format!("volume {}", view.index + 1));
    let volume_cell = volume_cell.as_deref().unwrap_or("");
    let total_ratio = ratio_percent(total_size, total_packed);
    if verbose {
        println!("----------- ---------- ---------- ----- ---------- -----  --------  ----");
        println!(
            "{:>11} {:>10} {:>10} {:>4}  {volume_cell:<10} {:>5}  {:>8}  {shown}",
            "", total_size, total_packed, total_ratio, "", ""
        );
    } else {
        println!("----------- ----------  ---------- -----  ----");
        println!(
            "{:>11} {:>10}  {volume_cell:<10} {:>5}  {shown}",
            "", total_size, ""
        );
    }
    println!();
}

/// Technical list (`lt` / `vt`): WinRAR's per-member block shape (fragment
/// values when listing one volume of a set).
pub fn list_technical(rar: &ArchiveReader, archive: &str, names: &[String]) {
    if listing_quiet() {
        return;
    }
    let view = VolumeView::of(archive);
    list_preamble(rar, archive, &view);
    for entry in rar
        .entries()
        .filter(|entry| matches_filter(entry.name(), names) && view.includes(entry))
    {
        let (position, packed, fragment_crc) = view.fragment(&entry).expect("included above");
        println!("{:>12}: {}", "Name", display_name(entry.name()));
        println!("{:>12}: {}", "Type", member_type_cell(&entry));
        if let Some((_, target)) = entry.redirect() {
            println!("{:>12}: {target}", "Target");
        }
        if !entry.is_dir() {
            println!("{:>12}: {}", "Size", entry.size());
            println!("{:>12}: {}", "Packed size", packed);
            println!(
                "{:>12}: {}",
                "Ratio",
                view.ratio_cell(&entry, position, packed)
            );
        }
        if entry.has_mtime() {
            println!(
                "{:>12}: {},{:09}",
                "Modified",
                member_stamp(&entry),
                entry.mtime_ns().unwrap_or(0)
            );
        }
        println!("{:>12}: {}", "Attributes", attributes_cell(&entry));
        if !entry.version().is_rar13() {
            let is_final = view.is_final_fragment(&entry, position);
            let label = if is_final { "CRC32" } else { "Pack-CRC32" };
            let crc = if is_final {
                entry.crc32()
            } else {
                fragment_crc
            };
            if let Some(crc) = crc {
                println!("{:>12}: {crc:08X}", label);
            }
        }
        if let Some(host) = host_os_cell(&entry) {
            println!("{:>12}: {host}", "Host OS");
        }
        println!("{:>12}: {}", "Compression", compression_cell(&entry));
        if entry.comp_solid() && !entry.version().is_rar13() {
            // WinRAR marks chain continuations, with a trailing space after
            // the value.
            println!("{:>12}: solid ", "Flags");
        }
        println!();
    }
}

/// WinRAR's `Type:` cell: redirect members carry their link kind, other
/// entries are files or directories.
fn member_type_cell(entry: &EntryRef<'_>) -> &'static str {
    match entry.redirect().map(|(redir_type, _)| redir_type) {
        Some(1) => "Unix symbolic link",
        Some(2) => "Windows symbolic link",
        Some(3) => "NTFS junction point",
        Some(4) => "Hard link",
        Some(5) => "File copy",
        _ if entry.is_dir() => "Directory",
        _ => "File",
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
        && last
            .chars()
            .next_back()
            .is_some_and(std::path::is_separator)
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
    let options = verify_options();
    if names.is_empty() {
        return rar
            .verify_with_options(options)
            .map_err(|e| CliError::from(e).context("test"));
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
    rar.verify_ids_with_options(&ids, options)
        .map_err(|e| CliError::from(e).context("test"))
}

/// Verification streams every member to a sink, so the in-memory size caps
/// guarding the materializing read API do not apply (official `Rar.exe t`
/// tests members of any size). The dictionary cap stays: it bounds decoder
/// memory.
pub(crate) fn verify_options() -> ExtractOptions {
    ExtractOptions {
        max_unpacked_bytes: None,
        max_total_unpacked_bytes: None,
        ..Default::default()
    }
}

/// One disk-extraction request: the four `x`/`e` arms of both binaries build
/// this value and hand it to [`extract`], so the flag-to-options assembly —
/// including the streaming rule that disk extraction imposes no in-memory
/// size caps — has one owner.
#[derive(Debug, Default)]
pub struct ExtractRequest {
    /// Members to extract (empty = every entry).
    pub names: Vec<String>,
    /// Destination directory (unused by the stdout mode).
    pub dest: PathBuf,
    /// Flat extraction (basename only, like `e`).
    pub flat: bool,
    /// Write the selected members to stdout (like `-so`) instead of files.
    pub stdout: bool,
    /// Extraction worker count (like `-mt<N>`).
    pub threads: Option<usize>,
    /// Raised dictionary cap (like `-mdx<N>`).
    pub max_dict_size: Option<u64>,
    /// Mark of the Web propagation (like `-om`).
    pub mark_web: Option<rar_rs::MarkOfTheWeb>,
    /// Overwrite policy (like `-o+` / `-o-`).
    pub overwrite: Option<String>,
    /// Non-interactive confirmation (the `-o+` default when piped).
    pub assume_yes: bool,
    pub auto_rename: bool,
    pub keep_broken: bool,
    pub skip_links: bool,
    pub allow_unsafe_links: bool,
    pub set_creation_time: bool,
    pub set_access_time: bool,
}

impl ExtractRequest {
    /// The library options for this request. Disk extraction is fully
    /// streaming, so the in-memory size caps do not apply (matching UnRAR
    /// and WinRAR, which extract members of any size); the dictionary cap
    /// stays, because it bounds decoder memory and WinRAR itself refuses
    /// dictionaries over 4 GiB unless `-mdx` raises it.
    fn options(&self) -> ExtractOptions {
        ExtractOptions {
            flat_paths: self.flat,
            max_unpacked_bytes: None,
            max_total_unpacked_bytes: None,
            max_dict_size: self
                .max_dict_size
                .or(Some(ExtractOptions::DEFAULT_MAX_DICT_SIZE)),
            skip_existing: output::skip_existing(
                self.overwrite.as_deref(),
                self.assume_yes,
                self.auto_rename,
            ),
            auto_rename: self.auto_rename,
            keep_broken: self.keep_broken,
            set_creation_time: self.set_creation_time,
            set_access_time: self.set_access_time,
            skip_links: self.skip_links,
            allow_unsafe_links: self.allow_unsafe_links,
            ..Default::default()
        }
    }
}

/// Run one extraction request: install the thread budget and Mark of the
/// Web, then either stream the selected members to stdout or extract them to
/// disk. The report is `None` for the stdout mode (nothing lands on disk).
pub fn extract(
    rar: &mut ArchiveReader,
    request: &ExtractRequest,
) -> CliResult<Option<ExtractionReport>> {
    if let Some(threads) = request.threads {
        rar_rs::set_extraction_threads(threads);
    }
    rar.set_mark_of_the_web(request.mark_web.clone());
    if request.stdout {
        extract_to_stdout(rar, &request.names, request.max_dict_size)?;
        return Ok(None);
    }
    let report = extract_members(rar, &request.dest, &request.names, request.options())?;
    Ok(Some(report))
}

/// Extract the whole archive, or only the members whose stored path, mask or
/// directory prefix matches one of `names` (see [`crate::selector`]), using
/// the same options. Directory entries are selected too, so selecting a
/// stored directory also materializes an empty one. The writer's own report
/// says which files were written and which the skip-existing policy left
/// untouched (the `Skipping` lines print from it); "written" counts files and
/// created links, not directories or `-ol-` links. A name matching nothing is
/// a hard error, so a mistyped selector is never silently swallowed or
/// treated as a destination directory.
pub fn extract_members(
    rar: &mut ArchiveReader,
    dest: &Path,
    names: &[String],
    options: ExtractOptions,
) -> CliResult<ExtractionReport> {
    let wanted: Vec<EntryId> = if names.is_empty() {
        rar.entries().map(|entry| entry.id()).collect()
    } else {
        let wanted = crate::selector::select_entries(
            rar.entries()
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
        wanted
    };

    let report = rar
        .extract_ids_with_options(&wanted, dest, options)
        .map_err(CliError::from)?;
    for path in report.skipped() {
        crate::info!("Skipping {}", display_name(&path.to_string_lossy()));
    }
    Ok(report)
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
/// (`p`). The selector follows the shared member-selection rules (stored
/// path, mask or directory prefix).
pub fn print_members(
    rar: &mut ArchiveReader,
    file: Option<&str>,
    max_dict_size: Option<u64>,
) -> CliResult<()> {
    let wanted: Vec<_> = if let Some(file) = file {
        crate::selector::select_entries(
            rar.entries()
                .filter(|entry| !entry.is_dir())
                .map(|entry| (entry.id(), entry.metadata().name())),
            std::slice::from_ref(&file.to_string()),
        )
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
            .map_err(|error| CliError::from(error).context(name))?;
    }
    out.flush().map_err(CliError::from)
}

#[cfg(test)]
mod tests {
    use super::{ExtractRequest, extract_names_and_dest, inferred_archive_paths, verify_options};
    use rar_rs::ExtractOptions;

    /// `t` streams every member to a sink, so the materializing read caps
    /// must be off (otherwise > 4 GiB members / > 32 GiB archives fail while
    /// official `Rar.exe t` succeeds); the dictionary cap bounds decoder
    /// memory and stays.
    #[test]
    fn verify_options_drop_the_read_size_caps() {
        let options = verify_options();
        assert_eq!(options.max_unpacked_bytes, None);
        assert_eq!(options.max_total_unpacked_bytes, None);
        assert_eq!(
            options.max_dict_size,
            Some(ExtractOptions::DEFAULT_MAX_DICT_SIZE)
        );
    }

    /// Disk extraction streams, so the request clears the in-memory size caps
    /// (the old `rar x`/`rar e` arms kept the 4 GiB/32 GiB read defaults,
    /// refusing members `unrar x` extracted) and keeps the dictionary cap;
    /// an explicit `-mdx` cap wins over the default.
    #[test]
    fn extraction_request_clears_the_read_size_caps() {
        let options = ExtractRequest::default().options();
        assert_eq!(options.max_unpacked_bytes, None);
        assert_eq!(options.max_total_unpacked_bytes, None);
        assert_eq!(
            options.max_dict_size,
            Some(ExtractOptions::DEFAULT_MAX_DICT_SIZE)
        );

        let raised = 8 * 1024 * 1024 * 1024;
        let request = ExtractRequest {
            max_dict_size: Some(raised),
            ..ExtractRequest::default()
        };
        assert_eq!(request.options().max_dict_size, Some(raised));
    }

    /// A positional extraction destination is recognized through
    /// `std::path::is_separator`: `/` everywhere, `\` only on Windows. On
    /// Unix `dest\` is a valid member selector and must not create a
    /// literal `dest\` directory.
    #[test]
    fn positional_destination_follows_the_host_separator() {
        let (names, dest) =
            extract_names_and_dest(&["dest/".into()], None, None, None, None, "p.rar").unwrap();
        assert!(names.is_empty());
        assert_eq!(dest, std::path::Path::new("dest"));

        let (names, dest) =
            extract_names_and_dest(&["dest\\".into()], None, None, None, None, "p.rar").unwrap();
        if cfg!(windows) {
            assert!(names.is_empty());
            assert_eq!(dest, std::path::Path::new("dest"));
        } else {
            assert_eq!(names, ["dest\\"]);
            assert_eq!(dest, std::path::Path::new("."));
        }
    }

    /// Inference appends the `.rar` / `.part1.rar` names through raw
    /// `OsString`s: `<path>.rar` replaces an existing extension while
    /// `<path>.part1.rar` keeps the spelling, and a non-UTF-8 Unix path is
    /// not rewritten to U+FFFD.
    #[test]
    fn inferred_archive_paths_keep_the_raw_path_bytes() {
        use std::path::{Path, PathBuf};

        assert_eq!(
            inferred_archive_paths(Path::new("foo")),
            vec![PathBuf::from("foo.rar"), PathBuf::from("foo.part1.rar")]
        );
        assert_eq!(
            inferred_archive_paths(Path::new("dir/bar.tar")),
            vec![
                PathBuf::from("dir/bar.rar"),
                PathBuf::from("dir/bar.tar.part1.rar"),
            ]
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let candidates =
                inferred_archive_paths(Path::new(std::ffi::OsStr::from_bytes(b"caf\xE9")));
            assert_eq!(candidates[0].as_os_str().as_bytes(), b"caf\xE9.rar");
            assert_eq!(candidates[1].as_os_str().as_bytes(), b"caf\xE9.part1.rar");
        }
    }
}
