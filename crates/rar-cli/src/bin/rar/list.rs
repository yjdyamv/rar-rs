//! Listing, searching, testing and archive info.

use crate::args::{ArchiveArgs, ListArgs};
use crate::common;
use crate::error;
use crate::error::CliResult;
use crate::info;
use crate::ops;

/// Expand `@listfiles` in a list/test command's member filter.
fn filter_names(args: &ListArgs, misc: &common::MiscSwitches) -> Result<Vec<String>, String> {
    crate::listfile::expand(&args.names, misc.list_files.as_deref())
}

/// Run a listing body over the volumes of the set when bare `-v`
/// ([`common::MiscSwitches::auto_volumes`]) is active, otherwise over the single
/// archive path given on the command line.
///
/// WinRAR's bare `-v` "list all volumes" is per-volume: it scans each volume
/// file and prints the file headers found in it (a multi-volume member appears
/// once per volume, exactly like opening that volume on its own). Starting
/// from `set.part3.rar` lists volumes 3..N, so the slice keeps the given
/// volume as the first one.
fn for_each_listed_volume<F>(
    args: &ListArgs,
    misc: &common::MiscSwitches,
    mut list: F,
) -> CliResult<()>
where
    F: FnMut(&rar_rs::ArchiveReader, &str, &[String]) -> CliResult<()>,
{
    let names = filter_names(args, misc).map_err(error::CliError::from)?;
    if !misc.auto_volumes {
        let rar = ops::open_reader_quick(&args.archive, args.password.password.as_deref())?;
        list(&rar, &args.archive, &names)?;
        return write_list_logs(misc, &rar, &args.archive, &names);
    }
    let mut volumes = rar_rs::discover_volumes(std::path::Path::new(&args.archive));
    if volumes.is_empty() {
        volumes.push(std::path::PathBuf::from(&args.archive));
    }
    // `discover_volumes` always starts at part 1; bare `-v` starts at the
    // volume named on the command line.
    if let Some(start) = volumes
        .iter()
        .position(|path| path == std::path::Path::new(&args.archive))
    {
        volumes.drain(..start);
    }
    for path in volumes {
        let name = path.to_string_lossy().into_owned();
        let rar = ops::open_reader_quick(&path, args.password.password.as_deref())?;
        list(&rar, &name, &names)?;
        write_list_logs(misc, &rar, &name, &names)?;
    }
    Ok(())
}
/// Find a string in member contents (like `rar i<string>`).
///
/// The search string is attached to the command: `rar i<str> archive.rar`,
/// with optional modifiers `ic` (case sensitive) and `ih` (hex bytes).
/// `password` comes from the external command's trailing switch block.
pub(crate) fn cmd_find(cmd: &str, args: &[String], password: Option<&str>) -> CliResult<()> {
    if args.is_empty() {
        return Err("usage: rar i<string> <archive.rar>".into());
    }
    let mut rest = &cmd[1..];
    let mut case_sensitive = false;
    let mut hex = false;
    if let Some(r) = rest.strip_prefix("c") {
        case_sensitive = true;
        rest = r;
    } else if let Some(r) = rest.strip_prefix("h") {
        hex = true;
        rest = r;
    } else if let Some(r) = rest.strip_prefix("i") {
        rest = r;
    }
    rest = rest.strip_prefix('=').unwrap_or(rest);
    if rest.is_empty() {
        return Err("usage: rar i<string> <archive.rar>".into());
    }
    let archive_path = &args[0];
    let needle: Vec<u8> = if hex {
        let digits: String = rest.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if !digits.len().is_multiple_of(2) {
            return Err("hex search string must have an even number of digits".into());
        }
        (0..digits.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).unwrap())
            .collect()
    } else {
        rest.as_bytes().to_vec()
    };
    if needle.is_empty() {
        return Err("empty search string".into());
    }
    let mut rar = ops::open_reader(archive_path, password).map_err(|e| format!("open: {e}"))?;
    let entries: Vec<(rar_rs::EntryId, String)> = rar
        .entries()
        .filter(|e| !e.is_dir())
        .map(|e| (e.id(), e.name().to_string()))
        .collect();
    let mut found = 0usize;
    for (id, name) in entries {
        let data = match rar.read_entry(id) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let (haystack, n) = if case_sensitive {
            (data.clone(), needle.clone())
        } else {
            (data.to_ascii_lowercase(), needle.to_ascii_lowercase())
        };
        if !haystack.windows(n.len()).any(|w| w == n.as_slice()) {
            continue;
        }
        found += 1;
        println!("Found  {archive_path} / {name}");
        for line in String::from_utf8_lossy(&data).split('\n') {
            let (h, n2) = if case_sensitive {
                (line.as_bytes().to_vec(), needle.clone())
            } else {
                (
                    line.to_ascii_lowercase().into_bytes(),
                    needle.to_ascii_lowercase(),
                )
            };
            if h.windows(n2.len()).any(|w| w == n2.as_slice()) {
                println!("{line}");
            }
        }
    }
    if found == 0 {
        return Ok(());
    }
    Ok(())
}

/// Verbose list (like `rar v`): adds the packed size, ratio and checksum
/// columns.
pub(crate) fn cmd_verbose_list(
    args: &ListArgs,
    misc: &common::MiscSwitches,
    service: ops::ServiceListing,
) -> CliResult<()> {
    for_each_listed_volume(args, misc, |rar, name, names| {
        ops::list_entries(rar, name, names, true, service);
        Ok(())
    })
}

/// Test archive contents (like `rar t`), optionally filtered to the
/// requested member names.
pub(crate) fn cmd_test(args: &ListArgs, misc: &common::MiscSwitches) -> CliResult<()> {
    let names = filter_names(args, misc).map_err(error::CliError::from)?;
    let mut rar = ops::open_reader(&args.archive, args.password.password.as_deref())
        .map_err(|e| e.context("open"))?;
    let report = ops::verify_members(&mut rar, &names)?;
    info!("{} OK, {} failed", report.passed(), report.failed());
    if report.failed() == 0 {
        Ok(())
    } else {
        for failure in report.failures() {
            let name = rar
                .entry(failure.entry_id())
                .map(|entry| entry.name().to_string())
                .unwrap_or_else(|_| "<unknown>".to_string());
            info!("{name}: {}", failure.error());
        }
        // Surface the first failure's category so scripts see CRC failures
        // (exit 3) or wrong passwords (exit 11) instead of a generic error.
        let code = report
            .failures()
            .first()
            .map_or(error::EXIT_FATAL, |failure| {
                error::exit_code_for(failure.error().code())
            });
        Err(error::CliError::with_code("test failed", code))
    }
}

pub(crate) fn cmd_list(
    args: &ListArgs,
    misc: &common::MiscSwitches,
    service: ops::ServiceListing,
) -> CliResult<()> {
    for_each_listed_volume(args, misc, |rar, name, names| {
        ops::list_entries(rar, name, names, false, service);
        Ok(())
    })
}

/// Bare list (`lb` / `vb`): member names only.
pub(crate) fn cmd_list_bare(
    args: &ListArgs,
    misc: &common::MiscSwitches,
    service: ops::ServiceListing,
) -> CliResult<()> {
    for_each_listed_volume(args, misc, |rar, name, names| {
        ops::list_bare(rar, name, names, service);
        Ok(())
    })
}

/// Technical list (`lt` / `vt`): mtime, attributes, sizes, ratio, CRC and
/// method per member, in the spirit of the official `rar lt`.
pub(crate) fn cmd_list_technical(
    args: &ListArgs,
    misc: &common::MiscSwitches,
    service: ops::ServiceListing,
) -> CliResult<()> {
    for_each_listed_volume(args, misc, |rar, name, names| {
        ops::list_technical(rar, name, names, service);
        Ok(())
    })
}

/// `-log` for the listing commands: every listed member name.
fn write_list_logs(
    misc: &common::MiscSwitches,
    rar: &rar_rs::ArchiveReader,
    archive: &str,
    names: &[String],
) -> CliResult<()> {
    let logs = crate::log::specs_from(misc)?;
    if logs.is_empty() {
        return Ok(());
    }
    let listed: Vec<String> = rar
        .entries()
        .filter(|entry| ops::matches_filter(entry.name(), names))
        .map(|entry| entry.name().to_string())
        .collect();
    crate::log::write_logs(&logs, &[std::path::PathBuf::from(archive)], &listed)?;
    Ok(())
}

pub(crate) fn cmd_info(args: &ArchiveArgs) -> CliResult<()> {
    let rar = ops::open_reader_quick(&args.archive, args.password.password.as_deref())?;

    let files: Vec<_> = rar.entries().filter(|e| !e.is_dir()).collect();
    let dirs: Vec<_> = rar.entries().filter(|e| e.is_dir()).collect();
    let total_size: u64 = files.iter().map(|e| e.size()).sum();
    let total_packed: u64 = files.iter().map(|e| e.compressed_size()).sum();

    println!("Archive: {}", args.archive);
    println!("Files:   {}", files.len());
    println!("Dirs:    {}", dirs.len());
    println!("Size:    {} bytes", total_size);
    println!("Packed:  {} bytes", total_packed);
    if total_size > 0 {
        println!(
            "Ratio:   {:.1}%",
            total_packed as f64 / total_size as f64 * 100.0
        );
    }

    Ok(())
}

/// Split an archive name into its Windows-style base name and the extension
/// WinRAR sorts by. The extension is the text after the *last* dot of the
/// base name, so `.hidden` has the extension `hidden` and a trailing dot
/// (`c.`) has none — both measured against `rar a -s`.
pub(crate) fn name_extension(name: &str) -> (&str, &str) {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, ext)) => (stem, ext),
        None => (base, ""),
    }
}

/// WinRAR's in-group member order: base name first (case-insensitively),
/// then the member's size, then the whole name so members with the same base
/// name in different directories stay in a fixed order. Measured against
/// `rar a -s`: `zzz\a.log` sorts before `b.log`, which sorts before
/// `aaa\c.log` (the directory prefix does not participate), and two members
/// with the same base name are ordered by size, smaller first, with the
/// whole name deciding equal sizes.
fn solid_name_key(name: &str, size: u64) -> (String, u64, String) {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    (base.to_lowercase(), size, name.to_lowercase())
}

/// Uncompressed size of a collected entry, or 0 when it cannot be read (a
/// link, a vanished path): the tie-break only orders members that share a
/// base name, so a wrong guess never reorders unrelated members.
fn collected_size(c: &crate::name_policy::Collected) -> u64 {
    std::fs::metadata(&c.path).map(|m| m.len()).unwrap_or(0)
}

/// Default solid member order (no `rarfiles.lst`): WinRAR groups members by
/// extension, case-insensitively and with the empty extension first, and
/// sorts each group by [`solid_name_key`]. Directory entries are not passed
/// here — the caller appends them last, like WinRAR.
pub(crate) fn sort_solid_by_extension(collected: &mut [crate::name_policy::Collected]) {
    collected.sort_by_cached_key(|c| {
        let (_, ext) = name_extension(&c.name);
        let (base, size, full) = solid_name_key(&c.name, collected_size(c));
        (ext.to_lowercase(), base, size, full)
    });
}

/// Depth of an archive name in the directory tree (`sub/f.txt` is 1). Used to
/// emit directory entries the way WinRAR does.
fn name_depth(name: &str) -> usize {
    name.bytes().filter(|b| *b == b'/' || *b == b'\\').count()
}

/// Directory entries, deepest first: WinRAR writes them level by level from
/// the bottom of the tree, so `a/b/c` precedes `a/b`, which precedes `a`
/// (measured against `rar a -s` with nested trees). Inside one level the
/// caller's order is kept (`sort_solid_by_extension` when sorting is on, the
/// collection order otherwise), so this is a stable sort by depth.
pub(crate) fn sort_directories_deepest_first(collected: &mut [crate::name_policy::Collected]) {
    collected.sort_by_key(|c| std::cmp::Reverse(name_depth(&c.name)));
}

/// Whether a rarfiles.lst mask selects a member. WinRAR matches the mask
/// against the *base* name only: `f*.txt` selects `sub/f1.txt`, and a mask
/// that carries a directory part (`sub/*.txt`) selects nothing. Masks count
/// the same as with `-x`/`-n`, so `*` also crosses separators.
fn rarfiles_mask_matches(mask: &str, name: &str) -> bool {
    let name = name.trim_start_matches("./");
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    crate::name_policy::mask_match(mask, base)
}

/// Reorder `collected` according to a rarfiles.lst mask list (`None` =
/// `$default` position). Each file is placed in the group of its
/// highest-priority matching mask: the earliest mask wins, except that a
/// mask whose match set is a subset of another's takes priority over it
/// regardless of position (WinRAR rule). Files matching nothing go to
/// `$default`, or to the end when there is no `$default`. Inside a group
/// members follow [`solid_name_key`], like WinRAR (a subset group still moves
/// ahead of its superset, so `f*.cpp` before `*.cpp`).
pub(crate) fn apply_rarfiles_order(
    collected: &mut Vec<crate::name_policy::Collected>,
    masks: &[Option<String>],
) {
    use std::cmp::Ordering;

    // Match set of each mask over the current file set (the subset rule
    // is evaluated against these sets). Masks match the archive name
    // with any leading `./` component stripped.
    let match_sets: Vec<Vec<usize>> = masks
        .iter()
        .map(|m| {
            let pat = m.as_deref();
            collected
                .iter()
                .enumerate()
                .filter(|(_, c)| pat.is_some_and(|p| rarfiles_mask_matches(p, &c.name)))
                .map(|(i, _)| i)
                .collect()
        })
        .collect();
    let default_pos = masks.iter().position(|m| m.is_none());

    // Highest-priority mask per file.
    let best: Vec<usize> = collected
        .iter()
        .map(|c| {
            let matched: Vec<usize> = masks
                .iter()
                .enumerate()
                .filter(|(_, m)| {
                    m.as_deref()
                        .is_some_and(|p| rarfiles_mask_matches(p, &c.name))
                })
                .map(|(mi, _)| mi)
                .collect();
            if matched.is_empty() {
                return default_pos.unwrap_or(masks.len());
            }
            matched
                .into_iter()
                .min_by(|&a, &b| {
                    let a_sub = match_sets[a].iter().all(|x| match_sets[b].contains(x));
                    let b_sub = match_sets[b].iter().all(|x| match_sets[a].contains(x));
                    match (a_sub, b_sub) {
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        _ => a.cmp(&b),
                    }
                })
                .unwrap()
        })
        .collect();

    let mut order: Vec<usize> = (0..collected.len()).collect();
    order.sort_by_cached_key(|&i| {
        let c = &collected[i];
        let (_, ext) = name_extension(&c.name);
        let (base, size, full) = solid_name_key(&c.name, collected_size(c));
        (best[i], ext.to_lowercase(), base, size, full)
    });
    let reordered: Vec<crate::name_policy::Collected> =
        order.into_iter().map(|i| collected[i].clone()).collect();
    *collected = reordered;
}

/// Whether `path` carries the legacy 7-byte `Rar!\x1a\x07\x00` signature
/// (peek at the head, tolerating an SFX stub within the first 8 MiB).
pub(crate) fn is_rar4_file(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    buf.truncate(n);
    let rar4 = b"Rar!\x1a\x07\x00";
    let rar5 = b"Rar!\x1a\x07\x01\x00";
    let first = |needle: &[u8]| buf.windows(needle.len()).position(|w| w == needle);
    match (first(rar5), first(rar4)) {
        (Some(r5), Some(r4)) => r4 < r5, // earliest signature wins (SFX stub)
        (None, Some(_)) => true,
        _ => false,
    }
}

/// Whether `path` is a DOS-era RAR 1.3/1.4 archive: the weak `RE~^`
/// signature is present and no RAR4/RAR5 signature appears anywhere in the
/// scanned window (a strong signature always wins, matching
/// `detect::find_archive_start`).
pub(crate) fn is_rar13_file(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    buf.truncate(n);
    let rar4 = b"Rar!\x1a\x07\x00";
    let rar5 = b"Rar!\x1a\x07\x01\x00";
    let first = |needle: &[u8]| buf.windows(needle.len()).position(|w| w == needle);
    first(rar5).is_none() && first(rar4).is_none() && first(b"RE~^").is_some()
}
