//! rar — create and modify RAR5 archives.

use std::env;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        usage();
        process::exit(1);
    }

    let result = match args[1].as_str() {
        "a" | "create" => cmd_create(&args[2..]),
        "u" | "update" => cmd_update(&args[2..]),
        "f" | "freshen" => cmd_freshen(&args[2..]),
        "m" | "move" => cmd_move(&args[2..]),
        "d" | "delete" => cmd_delete(&args[2..]),
        "rn" | "rename" => cmd_rename(&args[2..]),
        rest if rest.len() > 1 && rest.starts_with('i') && args.len() >= 2 => cmd_find(&args[1..]),
        "k" | "lock" => cmd_lock(&args[2..]),
        "rr" => cmd_rr(&args[2..]),
        "r" | "repair" => cmd_repair(&args[2..]),
        "s" | "s-" | "sfx" => cmd_sfx(&args[1..]),
        "rc" => cmd_rebuild_volumes(&args[2..]),
        "c" => cmd_comment_set(&args[2..]),
        "cw" => cmd_comment_write(&args[2..]),
        "x" | "extract" => cmd_extract(&args[2..]),
        "e" => cmd_extract_flat(&args[2..]),
        "t" | "test" => cmd_test(&args[2..]),
        "l" | "list" => cmd_list(&args[2..]),
        "v" => cmd_verbose_list(&args[2..]),
        "i" | "info" => cmd_info(&args[2..]),
        "-h" | "--help" | "help" => {
            usage();
            Ok(())
        }
        _ => {
            eprintln!("unknown command: {}", args[1]);
            usage();
            process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("rar: {e}");
        process::exit(1);
    }
}

fn usage() {
    eprintln!("rar-rs — create and modify RAR5 archives");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  rar a [-m0..-m5] [-p<password>] [-v<size>] [-s] [-htb] [-qo] [-hp]");
    eprintln!("        [-rrN] [-rvN] <archive.rar> <files...>");
    eprintln!("  rar u [-p<password>] <archive.rar> <files...>  Update: add missing files,");
    eprintln!("                                                  replace newer ones");
    eprintln!("  rar f [-p<password>] <archive.rar> <files...>  Freshen: update existing");
    eprintln!("                                                  members only");
    eprintln!("  rar m [-p<password>] <archive.rar> <files...>  Move: add, then delete the");
    eprintln!("                                                  source files");
    eprintln!("  rar d [-p<password>] <archive.rar> <names...>  Delete members without");
    eprintln!("                                                  rebuilding the archive");
    eprintln!("  rar rn <archive.rar> <old1> <new1> ...          Rename archived members");
    eprintln!("  rar k [-p<password>] <archive.rar>              Lock the archive");
    eprintln!("  rar rr [-p<password>] <archive.rar> [percent]   Add a recovery record");
    eprintln!("  rar r <archive.rar>                            Repair with the recovery");
    eprintln!("                                                  record (writes fixed.<name>)");
    eprintln!("  rar rc <archive.part1.rar>                     Rebuild missing volumes from");
    eprintln!("                                                  the .rev files");
    eprintln!("  rar c <archive.rar> < comment.txt              Set the archive comment");
    eprintln!("  rar cw <archive.rar>                           Write the archive comment");
    eprintln!("  rar s [-sfx<module>] <archive.rar>             Convert to SFX");
    eprintln!("  rar s- <archive.sfx>                           Remove the SFX module");
    eprintln!("  rar x [-p<password>] <archive.rar> [dest/]     Extract with full paths");
    eprintln!("  rar e [-p<password>] <archive.rar> [dest/]     Extract without paths");
    eprintln!("  rar t [-p<password>] <archive.rar>             Test archive contents");
    eprintln!("  rar i<string> <archive.rar>                     Find string in members");
    eprintln!("  rar l [-p<password>] <archive.rar>              List archive contents");
    eprintln!("  rar v [-p<password>] <archive.rar>              Verbose list");
    eprintln!("  rar i [-p<password>] <archive.rar>              Show archive info");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -v<size>    Create multi-volume archive (e.g. -v1m, -v100k, -v50000)");
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('k').or_else(|| s.strip_suffix('K')) {
        num.parse::<u64>()
            .map(|n| n * 1024)
            .map_err(|_| format!("invalid size: {s}"))
    } else if let Some(num) = s.strip_suffix('m').or_else(|| s.strip_suffix('M')) {
        num.parse::<u64>()
            .map(|n| n * 1024 * 1024)
            .map_err(|_| format!("invalid size: {s}"))
    } else {
        s.parse::<u64>().map_err(|_| format!("invalid size: {s}"))
    }
}

fn cmd_create(args: &[String]) -> Result<(), String> {
    let mut level: u8 = 3;
    let mut password: Option<String> = None;
    let mut volume_size: Option<u64> = None;
    let mut solid = false;
    let mut blake2 = false;
    let mut quick_open = false;
    let mut header_encrypt = false;
    let mut recovery_percent: Option<u8> = None;
    let mut recovery_volumes_percent: Option<u8> = None;
    let mut recovery_volume_count: Option<u32> = None;
    let mut positional = Vec::new();

    for arg in args {
        if let Some(rest) = arg.strip_prefix("-m") {
            level = rest
                .parse::<u8>()
                .map_err(|_| format!("invalid compression level: {arg}"))?;
            if level > 5 {
                return Err(format!("compression level must be 0-5, got {level}"));
            }
        } else if let Some(pw) = arg.strip_prefix("-p") {
            password = Some(pw.to_string());
        } else if let Some(sz) = arg.strip_prefix("-v") {
            volume_size = Some(parse_size(sz)?);
        } else if arg == "-s" {
            solid = true;
        } else if arg == "-htb" {
            blake2 = true;
        } else if arg == "-qo" {
            quick_open = true;
        } else if let Some(pw) = arg.strip_prefix("-hp") {
            header_encrypt = true;
            if !pw.is_empty() {
                password = Some(pw.to_string());
            }
        } else if let Some(p) = arg.strip_prefix("-rr") {
            recovery_percent = Some(
                p.parse::<u8>()
                    .map_err(|_| format!("invalid recovery percent: {arg}"))?
                    .min(100),
            );
        } else if let Some(p) = arg.strip_prefix("-rv") {
            if let Some(pct) = p.strip_suffix('%') {
                recovery_volumes_percent = Some(
                    pct.parse::<u8>()
                        .map_err(|_| format!("invalid recovery percent: {arg}"))?
                        .min(100),
                );
            } else {
                recovery_volume_count = Some(
                    p.parse::<u32>()
                        .map_err(|_| format!("invalid recovery volume count: {arg}"))?,
                );
            }
        } else {
            positional.push(arg.as_str());
        }
    }

    if positional.len() < 2 {
        return Err(
            "usage: rar a [-m0..-m5] [-p<password>] [-v<size>] [-s] [-htb] [-qo] [-hp] [-rrN] [-rvN] <archive.rar> <files...>"
                .into(),
        );
    }
    let archive_path = positional[0];
    let files = &positional[1..];

    let opts = rar5::CreateOptions {
        solid,
        quick_open,
        blake2,
        password: password.clone(),
        encrypt_headers: header_encrypt,
        recovery_percent,
        recovery_volumes_percent,
        recovery_volume_count,
        volume_size,
    };

    let existing = std::path::Path::new(archive_path).exists();
    let mut rar = if existing {
        // Append to an existing archive (like `rar a`): existing members
        // are preserved verbatim.
        if volume_size.is_some() {
            return Err("appending to multi-volume archives is not supported".into());
        }
        match password {
            Some(ref pw) => rar5::RarArchive::open_append_with_password(archive_path, pw)
                .map_err(|e| format!("open: {e}"))?,
            None => {
                rar5::RarArchive::open_append(archive_path).map_err(|e| format!("open: {e}"))?
            }
        }
    } else {
        rar5::RarArchive::create_with_options(archive_path, opts)
            .map_err(|e| format!("create: {e}"))?
    };

    for file in files {
        rar.add(file, level)
            .map_err(|e| format!("add {file}: {e}"))?;
    }

    let was_existing = existing;
    rar.close().map_err(|e| format!("close: {e}"))?;
    if volume_size.is_some() {
        let vols = rar5::discover_volumes(std::path::Path::new(archive_path));
        println!(
            "Created {} volume(s) ({} file(s), level {level})",
            vols.len(),
            files.len()
        );
    } else if was_existing {
        println!(
            "Updated {archive_path} ({} file(s), level {level})",
            files.len()
        );
    } else {
        println!(
            "Created {archive_path} ({} file(s), level {level})",
            files.len()
        );
    }
    Ok(())
}

/// Delete members from an archive without rebuilding it (mirrors `rar d`).
fn cmd_delete(args: &[String]) -> Result<(), String> {
    let mut password: Option<String> = None;
    let mut positional = Vec::new();
    for arg in args {
        if let Some(pw) = arg.strip_prefix("-p") {
            password = Some(pw.to_string());
        } else {
            positional.push(arg.as_str());
        }
    }
    if positional.len() < 2 {
        return Err("usage: rar d [-p<password>] <archive.rar> <names...>".into());
    }
    let archive_path = positional[0];
    let names = &positional[1..];

    let mut rar = match password {
        Some(pw) => rar5::RarArchive::open_with_password(archive_path, &pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(archive_path).map_err(|e| format!("open: {e}"))?,
    };
    let n = rar.delete(names).map_err(|e| format!("delete: {e}"))?;
    println!("Deleted {n} file(s) from {archive_path}");
    Ok(())
}

/// Update an archive: add files not present, replace files whose source
/// is newer (like `rar u`).
fn cmd_update(args: &[String]) -> Result<(), String> {
    let mut password: Option<String> = None;
    let mut positional = Vec::new();
    for arg in args {
        if let Some(pw) = arg.strip_prefix("-p") {
            password = Some(pw.to_string());
        } else {
            positional.push(arg.as_str());
        }
    }
    if positional.len() < 2 {
        return Err("usage: rar u [-p<password>] <archive.rar> <files...>".into());
    }
    let archive_path = positional[0];
    let files = &positional[1..];
    if !std::path::Path::new(archive_path).exists() {
        return Err(format!("archive not found: {archive_path}"));
    }

    // Decide per file: skip (unchanged), delete + re-add (newer), add.
    let rar = match &password {
        Some(pw) => rar5::RarArchive::open_with_password(archive_path, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(archive_path).map_err(|e| format!("open: {e}"))?,
    };
    let mut to_delete = Vec::new();
    let mut to_add = Vec::new();
    for file in files {
        let path = std::path::Path::new(file);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("not a file: {file}"))?
            .replace('\\', "/");
        if let Some(entry) = rar.get_entry(&name) {
            let src_mtime = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as u32)
                .unwrap_or(0);
            if src_mtime > entry.header.mtime {
                to_delete.push(name);
                to_add.push(*file);
            }
            // else: unchanged, skip
        } else {
            to_add.push(*file);
        }
    }
    drop(rar);
    if !to_delete.is_empty() {
        let mut rar = match &password {
            Some(pw) => rar5::RarArchive::open_with_password(archive_path, pw)
                .map_err(|e| format!("open: {e}"))?,
            None => rar5::RarArchive::open(archive_path).map_err(|e| format!("open: {e}"))?,
        };
        let names: Vec<&str> = to_delete.iter().map(|s| s.as_str()).collect();
        rar.delete(&names).map_err(|e| format!("delete: {e}"))?;
    }
    if to_add.is_empty() {
        println!("{archive_path}: no files to update");
        return Ok(());
    }
    // Deleting every member erases the archive file; recreate it when the
    // updated members were the only ones.
    let mut rar = if std::path::Path::new(archive_path).exists() {
        match &password {
            Some(pw) => rar5::RarArchive::open_append_with_password(archive_path, pw)
                .map_err(|e| format!("open: {e}"))?,
            None => {
                rar5::RarArchive::open_append(archive_path).map_err(|e| format!("open: {e}"))?
            }
        }
    } else {
        match &password {
            Some(pw) => rar5::RarArchive::create_with_password(archive_path, pw)
                .map_err(|e| format!("create: {e}"))?,
            None => rar5::RarArchive::create(archive_path).map_err(|e| format!("create: {e}"))?,
        }
    };
    for file in &to_add {
        rar.add(file, 3).map_err(|e| format!("add {file}: {e}"))?;
    }
    rar.close().map_err(|e| format!("close: {e}"))?;
    println!("Updated {archive_path} ({} file(s))", to_add.len());
    Ok(())
}

/// Lock the archive (like `rar k`).
fn cmd_lock(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: rar k <archive.rar>".into());
    }
    let mut rar = rar5::RarArchive::open(&args[0]).map_err(|e| format!("open: {e}"))?;
    rar.lock().map_err(|e| format!("lock: {e}"))?;
    println!("Locked {archive}", archive = args[0]);
    Ok(())
}

/// Add an inline recovery record (like `rar rr`).
fn cmd_rr(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: rar rr <archive.rar> [percent]".into());
    }
    let percent = args
        .get(1)
        .and_then(|p| p.parse::<u8>().ok())
        .unwrap_or(10)
        .min(100);
    let mut rar = rar5::RarArchive::open(&args[0]).map_err(|e| format!("open: {e}"))?;
    rar.add_recovery_record(percent)
        .map_err(|e| format!("rr: {e}"))?;
    println!(
        "Recovery record {percent}% added to {archive}",
        archive = args[0]
    );
    Ok(())
}

/// Rename archived members (like `rar rn`): pairs of old/new names.
fn cmd_rename(args: &[String]) -> Result<(), String> {
    if args.len() < 3 || !(args.len() - 1).is_multiple_of(2) {
        return Err("usage: rar rn <archive.rar> <old1> <new1> [<old2> <new2> ...]".into());
    }
    let archive_path = &args[0];
    let pairs: Vec<(&str, &str)> = args[1..]
        .chunks(2)
        .map(|c| (c[0].as_str(), c[1].as_str()))
        .collect();
    let mut rar = rar5::RarArchive::open(archive_path).map_err(|e| format!("open: {e}"))?;
    let n = rar.rename(&pairs).map_err(|e| format!("rename: {e}"))?;
    println!("Renamed {n} file(s) in {archive_path}");
    Ok(())
}

/// Freshen the archive (like `rar f`): update members that already exist
/// when the source is newer; never add new members.
fn cmd_freshen(args: &[String]) -> Result<(), String> {
    let mut password: Option<String> = None;
    let mut positional = Vec::new();
    for arg in args {
        if let Some(pw) = arg.strip_prefix("-p") {
            password = Some(pw.to_string());
        } else {
            positional.push(arg.as_str());
        }
    }
    if positional.len() < 2 {
        return Err("usage: rar f [-p<password>] <archive.rar> <files...>".into());
    }
    let archive_path = positional[0];
    let files = &positional[1..];
    if !std::path::Path::new(archive_path).exists() {
        return Err(format!("archive not found: {archive_path}"));
    }
    let rar = match password {
        Some(ref pw) => rar5::RarArchive::open_with_password(archive_path, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(archive_path).map_err(|e| format!("open: {e}"))?,
    };
    let mut to_delete = Vec::new();
    let mut to_add = Vec::new();
    for file in files {
        let path = std::path::Path::new(file);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| format!("not a file: {file}"))?
            .replace('\\', "/");
        if let Some(entry) = rar.get_entry(&name) {
            let src_mtime = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as u32)
                .unwrap_or(0);
            if src_mtime > entry.header.mtime {
                to_delete.push(name);
                to_add.push(*file);
            }
        }
        // Missing members are skipped by freshen.
    }
    drop(rar);
    if to_delete.is_empty() {
        println!("{archive_path}: no files to freshen");
        return Ok(());
    }
    {
        let mut rar = match &password {
            Some(pw) => rar5::RarArchive::open_with_password(archive_path, pw)
                .map_err(|e| format!("open: {e}"))?,
            None => rar5::RarArchive::open(archive_path).map_err(|e| format!("open: {e}"))?,
        };
        let names: Vec<&str> = to_delete.iter().map(|s| s.as_str()).collect();
        rar.delete(&names).map_err(|e| format!("delete: {e}"))?;
    }
    // Deleting every member erases the archive file; recreate it when the
    // freshened members were the only ones.
    let mut rar = if std::path::Path::new(archive_path).exists() {
        match &password {
            Some(pw) => rar5::RarArchive::open_append_with_password(archive_path, pw)
                .map_err(|e| format!("open: {e}"))?,
            None => {
                rar5::RarArchive::open_append(archive_path).map_err(|e| format!("open: {e}"))?
            }
        }
    } else {
        match &password {
            Some(pw) => rar5::RarArchive::create_with_password(archive_path, pw)
                .map_err(|e| format!("create: {e}"))?,
            None => rar5::RarArchive::create(archive_path).map_err(|e| format!("create: {e}"))?,
        }
    };
    for file in &to_add {
        rar.add(file, 3).map_err(|e| format!("add {file}: {e}"))?;
    }
    rar.close().map_err(|e| format!("close: {e}"))?;
    println!("Freshened {archive_path} ({} file(s))", to_add.len());
    Ok(())
}

/// Move files into the archive (like `rar m`): add them, then erase the
/// sources after a successful close.
fn cmd_move(args: &[String]) -> Result<(), String> {
    let mut password: Option<String> = None;
    let mut positional = Vec::new();
    for arg in args {
        if let Some(pw) = arg.strip_prefix("-p") {
            password = Some(pw.to_string());
        } else {
            positional.push(arg.as_str());
        }
    }
    if positional.len() < 2 {
        return Err("usage: rar m [-p<password>] <archive.rar> <files...>".into());
    }
    let archive_path = positional[0];
    let files = &positional[1..];
    for file in files {
        let path = std::path::Path::new(file);
        if !path.exists() {
            return Err(format!("path not found: {file}"));
        }
    }
    let mut rar = if std::path::Path::new(archive_path).exists() {
        match &password {
            Some(pw) => rar5::RarArchive::open_append_with_password(archive_path, pw)
                .map_err(|e| format!("open: {e}"))?,
            None => {
                rar5::RarArchive::open_append(archive_path).map_err(|e| format!("open: {e}"))?
            }
        }
    } else {
        match &password {
            Some(pw) => rar5::RarArchive::create_with_password(archive_path, pw)
                .map_err(|e| format!("create: {e}"))?,
            None => rar5::RarArchive::create(archive_path).map_err(|e| format!("create: {e}"))?,
        }
    };
    for file in files {
        rar.add(file, 3).map_err(|e| format!("add {file}: {e}"))?;
    }
    rar.close().map_err(|e| format!("close: {e}"))?;
    for file in files {
        let path = std::path::Path::new(file);
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(path);
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
    println!("Moved {} file(s) to {archive_path}", files.len());
    Ok(())
}

/// Find a string in member contents (like `rar i<string>`).
///
/// The search string is attached to the command: `rar i<str> archive.rar`,
/// with optional modifiers `ic` (case sensitive) and `ih` (hex bytes).
fn cmd_find(args: &[String]) -> Result<(), String> {
    if args.len() < 2 {
        return Err("usage: rar i<string> <archive.rar>".into());
    }
    let cmd = &args[0];
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
    let archive_path = &args[1];
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
    let mut rar = rar5::RarArchive::open(archive_path).map_err(|e| format!("open: {e}"))?;
    let entries: Vec<String> = rar
        .list()
        .iter()
        .filter(|e| !e.is_dir())
        .map(|e| e.name().to_string())
        .collect();
    let mut found = 0usize;
    for name in entries {
        let data = match rar.read(&name) {
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
fn cmd_verbose_list(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: rar v <archive.rar>".into());
    }
    let rar = rar5::RarArchive::open(&args[0]).map_err(|e| format!("{e}"))?;
    println!(
        "{:>10}  {:>10}  {:>6}  {:>10}  {:<8}  Name",
        "Size", "Packed", "Ratio", "Checksum", "Method"
    );
    println!("{}", "-".repeat(70));
    let mut total_size = 0u64;
    let mut total_packed = 0u64;
    for entry in rar.list() {
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
        rar.list().len()
    );
    Ok(())
}

/// Repair an archive with its inline recovery record (like `rar r`).
/// Writes `fixed.<name>` when damage was found and repaired.
fn cmd_repair(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: rar r <archive.rar>".into());
    }
    let archive_path = &args[0];
    let input = std::fs::read(archive_path).map_err(|e| format!("read: {e}"))?;
    let repaired = rar5::repair_archive(&input).map_err(|e| format!("repair: {e}"))?;
    if repaired == input {
        println!("All OK");
        return Ok(());
    }
    // The official tool refuses an obviously truncated archive with a
    // clear error; validate the repaired bytes with our own reader.
    let name = std::path::Path::new(archive_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive.rar".to_string());
    let fixed_path = format!("fixed.{name}");
    std::fs::write(&fixed_path, &repaired).map_err(|e| format!("write: {e}"))?;
    if let Err(e) = rar5::RarArchive::open(&fixed_path) {
        let _ = std::fs::remove_file(&fixed_path);
        return Err(format!("repair produced an unreadable archive: {e}"));
    }
    println!("Repaired {archive_path} -> {fixed_path}");
    Ok(())
}

/// Rebuild missing volumes from the `.rev` recovery volumes (like `rar rc`).
fn cmd_rebuild_volumes(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: rar rc <archive.part1.rar>".into());
    }
    let first = &args[0];
    let rebuilt = rar5::rebuild_missing_volumes(std::path::Path::new(first))
        .map_err(|e| format!("rc: {e}"))?;
    if rebuilt.is_empty() {
        println!("All volumes present");
    } else {
        for path in &rebuilt {
            println!("Rebuilt {}", path.display());
        }
    }
    Ok(())
}

/// Set the archive comment from stdin (like `rar c`); empty input removes
/// the comment.
fn cmd_comment_set(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: rar c <archive.rar> < comment.txt".into());
    }
    use std::io::Read;
    let mut comment = Vec::new();
    std::io::stdin()
        .read_to_end(&mut comment)
        .map_err(|e| format!("stdin: {e}"))?;
    let mut rar = rar5::RarArchive::open(&args[0]).map_err(|e| format!("open: {e}"))?;
    rar.set_comment(&comment)
        .map_err(|e| format!("comment: {e}"))?;
    if comment.is_empty() {
        println!("Comment removed from {archive}", archive = args[0]);
    } else {
        println!("Comment added to {archive}", archive = args[0]);
    }
    Ok(())
}

/// Write the archive comment to stdout (like `rar cw`).
fn cmd_comment_write(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: rar cw <archive.rar>".into());
    }
    let mut rar = rar5::RarArchive::open(&args[0]).map_err(|e| format!("open: {e}"))?;
    if let Some(comment) = rar.get_comment().map_err(|e| format!("cw: {e}"))? {
        use std::io::Write;
        std::io::stdout()
            .write_all(&comment)
            .map_err(|e| format!("stdout: {e}"))?;
    }
    Ok(())
}

/// Convert an archive to or from SFX (like `rar s` / `rar s-`).
fn cmd_sfx(args: &[String]) -> Result<(), String> {
    let strip = args[0] == "s-";
    let mut module: Option<String> = None;
    let mut positional = Vec::new();
    for arg in &args[1..] {
        if let Some(m) = arg.strip_prefix("-sfx") {
            module = Some(m.to_string());
        } else {
            positional.push(arg.as_str());
        }
    }
    if positional.is_empty() {
        return Err(if strip {
            "usage: rar s- <archive.sfx>".into()
        } else {
            "usage: rar s [-sfx<module>] <archive.rar>".into()
        });
    }
    let archive_path = positional[0];
    let input = std::fs::read(archive_path).map_err(|e| format!("read: {e}"))?;
    // Locate the archive start after any embedded stub.
    let sfx_offset = rar5::sfx_offset_of(&input)
        .ok_or_else(|| format!("{archive_path} is not an SFX archive"))?;
    if strip {
        let base = archive_path
            .strip_suffix(".sfx")
            .or_else(|| archive_path.strip_suffix(".SFX"))
            .map(|b| b.to_string())
            .unwrap_or_else(|| format!("{archive_path}.plain"));
        let out_path = format!("{base}.rar");
        std::fs::write(&out_path, &input[sfx_offset..]).map_err(|e| format!("write: {e}"))?;
        println!("Removed SFX module: {out_path}");
        return Ok(());
    }

    // Creation: prepend the SFX module.
    let module_path = match module {
        Some(m) => m,
        None => find_sfx_module()
            .ok_or_else(|| "default.sfx not found (use -sfx<module>)".to_string())?,
    };
    let module_bytes = std::fs::read(&module_path).map_err(|e| format!("read module: {e}"))?;
    let base = std::path::Path::new(archive_path)
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_string());
    let out_path = format!("{base}.sfx");
    let mut out = Vec::with_capacity(module_bytes.len() + input.len());
    out.extend_from_slice(&module_bytes);
    out.extend_from_slice(&input);
    std::fs::write(&out_path, &out).map_err(|e| format!("write: {e}"))?;
    println!("Created {out_path}");
    Ok(())
}

/// Locate a `default.sfx` module: `$HOME`, `/usr/lib`, `/usr/local/lib`.
fn find_sfx_module() -> Option<String> {
    let candidates = [
        std::env::var("HOME")
            .ok()
            .map(|h| format!("{h}/default.sfx")),
        Some("/usr/lib/default.sfx".to_string()),
        Some("/usr/local/lib/default.sfx".to_string()),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|p| std::path::Path::new(p).exists())
}

/// Extract with full paths (like `rar x`).
fn cmd_extract(args: &[String]) -> Result<(), String> {
    let (password, positional) = split_password(args);
    if positional.is_empty() {
        return Err("usage: rar x [-p<password>] <archive.rar> [dest/]".into());
    }
    let dest = if positional.len() > 1 {
        positional[1]
    } else {
        "."
    };
    let mut rar = match &password {
        Some(pw) => rar5::RarArchive::open_with_password(positional[0], pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(positional[0]).map_err(|e| format!("open: {e}"))?,
    };
    let mut count = 0usize;
    let names: Vec<String> = rar.list().iter().map(|e| e.name().to_string()).collect();
    for name in &names {
        rar.extract(name, dest)
            .map_err(|e| format!("{name}: {e}"))?;
        count += 1;
    }
    println!("Extracted {count} file(s) to {dest}");
    Ok(())
}

/// Extract without archived paths (like `rar e`).
fn cmd_extract_flat(args: &[String]) -> Result<(), String> {
    let (password, positional) = split_password(args);
    if positional.is_empty() {
        return Err("usage: rar e [-p<password>] <archive.rar> [dest/]".into());
    }
    let dest = if positional.len() > 1 {
        positional[1]
    } else {
        "."
    };
    let mut rar = match &password {
        Some(pw) => rar5::RarArchive::open_with_password(positional[0], pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(positional[0]).map_err(|e| format!("open: {e}"))?,
    };
    let mut count = 0usize;
    let names: Vec<String> = rar.list().iter().map(|e| e.name().to_string()).collect();
    for name in &names {
        let entry = rar.get_entry(name).unwrap();
        if entry.is_dir() {
            continue;
        }
        let data = rar.read(name).map_err(|e| format!("{name}: {e}"))?;
        let file_name = std::path::Path::new(&name)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let out_path = std::path::Path::new(dest).join(file_name.as_ref());
        std::fs::write(&out_path, &data).map_err(|e| format!("{}: {e}", out_path.display()))?;
        count += 1;
    }
    println!("Extracted {count} file(s) to {dest}");
    Ok(())
}

/// Test archive contents (like `rar t`).
fn cmd_test(args: &[String]) -> Result<(), String> {
    let (password, positional) = split_password(args);
    if positional.is_empty() {
        return Err("usage: rar t [-p<password>] <archive.rar>".into());
    }
    let mut rar = match &password {
        Some(pw) => rar5::RarArchive::open_with_password(positional[0], pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(positional[0]).map_err(|e| format!("open: {e}"))?,
    };
    let names: Vec<String> = rar.list().iter().map(|e| e.name().to_string()).collect();
    let mut ok = 0;
    let mut fail = 0;
    for name in &names {
        let entry = rar.get_entry(name).unwrap();
        if entry.is_dir() {
            continue;
        }
        match rar.read(name) {
            Ok(_) => {
                println!("  OK  {name}");
                ok += 1;
            }
            Err(e) => {
                println!("  FAIL {name}: {e}");
                fail += 1;
            }
        }
    }
    println!("{ok} OK, {fail} failed");
    if fail > 0 {
        return Err("test failed".into());
    }
    Ok(())
}

/// Split leading `-p<password>` switches from positional arguments.
fn split_password(args: &[String]) -> (Option<String>, Vec<&str>) {
    let mut password = None;
    let mut positional = Vec::new();
    for arg in args {
        if let Some(pw) = arg.strip_prefix("-p") {
            password = Some(pw.to_string());
        } else {
            positional.push(arg.as_str());
        }
    }
    (password, positional)
}

fn cmd_list(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: rar l <archive.rar>".into());
    }
    let rar = rar5::RarArchive::open(&args[0]).map_err(|e| format!("{e}"))?;

    println!(
        "{:>10}  {:>10}  {:>6}  {:<8}  Name",
        "Size", "Packed", "Ratio", "Method"
    );
    println!("{}", "-".repeat(60));

    let mut total_size = 0u64;
    let mut total_packed = 0u64;

    for entry in rar.list() {
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

        println!(
            "{:>10}  {:>10}  {:>6}  {:<8}  {}",
            entry.size(),
            entry.compressed_size(),
            ratio,
            entry.method_name(),
            entry.name()
        );

        total_size += entry.size();
        total_packed += entry.compressed_size();
    }

    println!("{}", "-".repeat(60));
    let overall = if total_size > 0 {
        format!("{:.1}%", total_packed as f64 / total_size as f64 * 100.0)
    } else {
        " 0.0%".to_string()
    };
    println!(
        "{total_size:>10}  {total_packed:>10}  {overall:>6}  {:<8}  {} file(s)",
        "",
        rar.list().len()
    );

    Ok(())
}

fn cmd_info(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: rar i <archive.rar>".into());
    }
    let rar = rar5::RarArchive::open(&args[0]).map_err(|e| format!("{e}"))?;

    let files: Vec<_> = rar.list().iter().filter(|e| !e.is_dir()).collect();
    let dirs: Vec<_> = rar.list().iter().filter(|e| e.is_dir()).collect();
    let total_size: u64 = files.iter().map(|e| e.size()).sum();
    let total_packed: u64 = files.iter().map(|e| e.compressed_size()).sum();

    println!("Archive: {}", args[0]);
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
