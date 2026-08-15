//! rar — create and modify RAR5 archives.

mod common;

use clap::{Args, Parser, Subcommand};
use std::process;

#[derive(Parser)]
#[command(
    name = "rar",
    version,
    about = "create and modify RAR5 archives",
    long_about = "Pure-Rust RAR5 archive tool: create, append, update, delete, rename,\nlock, repair and extract archives, with WinRAR-compatible switches.",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Add files to the archive (creates it when missing)
    #[command(visible_alias = "a")]
    Create(CreateArgs),
    /// Update: add missing files, replace newer ones
    #[command(visible_alias = "u")]
    Update(FilesArgs),
    /// Freshen: update existing members only
    #[command(visible_alias = "f")]
    Freshen(FilesArgs),
    /// Move: add files, then erase the sources
    #[command(visible_alias = "m")]
    Move(FilesArgs),
    /// Delete members without rebuilding the archive
    #[command(visible_alias = "d")]
    Delete(DeleteArgs),
    /// Rename archived members
    #[command(visible_alias = "rn")]
    Rename(RenameArgs),
    /// Lock the archive
    #[command(visible_alias = "k")]
    Lock(ArchiveArgs),
    /// Add a recovery record
    #[command(visible_alias = "rr")]
    Recovery(RecoveryArgs),
    /// Repair the archive with its recovery record
    #[command(visible_alias = "r")]
    Repair(ArchiveArgs),
    /// Rebuild missing volumes from the .rev files
    #[command(visible_alias = "rc")]
    RebuildVolumes(ArchiveArgs),
    /// Convert the archive to SFX
    #[command(visible_alias = "s")]
    Sfx(SfxArgs),
    /// Remove the SFX module from an SFX archive
    #[command(visible_alias = "s-")]
    SfxStrip(ArchiveArgs),
    /// Set the archive comment (from stdin)
    #[command(visible_alias = "c")]
    CommentSet(ArchiveArgs),
    /// Write the archive comment to stdout
    #[command(visible_alias = "cw")]
    CommentWrite(ArchiveArgs),
    /// Extract with full paths
    #[command(visible_alias = "x")]
    Extract(ExtractArgs),
    /// Extract without paths
    #[command(visible_alias = "e")]
    ExtractFlat(ExtractArgs),
    /// Test archive contents
    #[command(visible_alias = "t")]
    Test(ArchiveArgs),
    /// Verbosely list archive contents
    #[command(visible_alias = "v")]
    VerboseList(ArchiveArgs),
    /// List archive contents
    #[command(visible_alias = "l")]
    List(ArchiveArgs),
    /// Show archive info
    #[command(visible_alias = "i")]
    Info(ArchiveArgs),
    /// Unknown commands starting with `i` are the string search: `rar i<string>`
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// `-p<password>` plus an archive path.
#[derive(Args)]
struct ArchiveArgs {
    #[command(flatten)]
    password: common::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
}

/// Archive path plus an optional destination directory.
#[derive(Args)]
struct ExtractArgs {
    #[command(flatten)]
    password: common::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(value_name = "DEST", default_value = ".")]
    dest: String,
    /// Compression threads (like `-mt<N>`; also used for extraction)
    #[arg(long = "threads", value_name = "N", value_parser = parse_threads)]
    threads: Option<usize>,
}

/// Archive path plus one or more source files.
#[derive(Args)]
struct FilesArgs {
    #[command(flatten)]
    password: common::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(value_name = "FILES", required = true)]
    files: Vec<String>,
}

/// Archive path plus the members to delete.
#[derive(Args)]
struct DeleteArgs {
    #[command(flatten)]
    password: common::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(value_name = "NAMES", required = true)]
    names: Vec<String>,
}

/// Archive path plus old/new name pairs.
#[derive(Args)]
struct RenameArgs {
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(value_name = "OLD", required = true, num_args = 2..)]
    pairs: Vec<String>,
}

/// Archive path plus the recovery percentage.
#[derive(Args)]
struct RecoveryArgs {
    #[command(flatten)]
    password: common::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(
        value_name = "PERCENT",
        default_value_t = 10,
        value_parser = clap::value_parser!(u8).range(0..=100)
    )]
    percent: u8,
}

/// SFX conversion arguments.
#[derive(Args)]
struct SfxArgs {
    /// SFX module file (default: $HOME/default.sfx or /usr/lib)
    #[arg(long = "sfx-module", value_name = "MODULE")]
    module: Option<String>,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
}

/// Creation switches, keeping the rar spellings (`-m3`, `-psecret`,
/// `-v100k`, `-x<mask>`, ...) plus long aliases.
#[derive(Args)]
struct CreateArgs {
    /// Compression level 0-5
    #[arg(
        short = 'm',
        long = "level",
        value_name = "N",
        default_value_t = 3,
        value_parser = clap::value_parser!(u8).range(0..=5)
    )]
    level: u8,
    #[command(flatten)]
    password: common::PasswordArgs,
    /// Volume size (e.g. 1m, 100k)
    #[arg(short = 'v', long = "volume-size", value_name = "SIZE", value_parser = parse_size)]
    volume_size: Option<u64>,
    /// Solid archive
    #[arg(short = 's', long)]
    solid: bool,
    /// BLAKE2sp hash records
    #[arg(long = "blake2")]
    blake2: bool,
    /// Quick-open record
    #[arg(long = "quick-open")]
    quick_open: bool,
    /// Header encryption (optionally with a password)
    #[arg(long = "header-encrypt", num_args = 0..=1, default_missing_value = "")]
    header_encrypt: Option<String>,
    /// Recovery record percentage
    #[arg(
        long = "recovery-percent",
        value_name = "N",
        value_parser = clap::value_parser!(u8).range(0..=100)
    )]
    recovery_percent: Option<u8>,
    /// Recovery volumes: count or percentage (`20` or `20%`)
    #[arg(long = "recovery-volumes", value_name = "N|N%", value_parser = parse_recovery_volumes)]
    recovery_volumes: Option<RecoveryVolumes>,
    /// Compression threads
    #[arg(long = "threads", value_name = "N", value_parser = parse_threads)]
    threads: Option<usize>,
    /// Store basename-only names (no directory entries)
    #[arg(long = "basename-only")]
    basename_only: bool,
    /// Exclude the base directory from names (wildcard paths)
    #[arg(long = "exclude-base-dir")]
    strip_base: bool,
    /// Do not recurse into directories
    #[arg(long = "no-recurse")]
    no_recurse: bool,
    /// Convert stored names to lowercase
    #[arg(long = "lowercase")]
    lowercase: bool,
    /// Convert stored names to uppercase
    #[arg(long = "uppercase")]
    uppercase: bool,
    /// Prefix for stored names
    #[arg(long = "archive-path", value_name = "PATH")]
    path_prefix: Option<String>,
    /// Exclude mask (repeatable)
    #[arg(long = "exclude", value_name = "MASK", action = clap::ArgAction::Append)]
    exclude_masks: Vec<String>,
    /// Include mask (repeatable)
    #[arg(long = "include", value_name = "MASK", action = clap::ArgAction::Append)]
    include_masks: Vec<String>,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(value_name = "FILES", required = true)]
    files: Vec<String>,
}

/// Recovery volumes parameter: an exact count or a percentage.
#[derive(Clone, Copy)]
enum RecoveryVolumes {
    Count(u32),
    Percent(u8),
}

fn parse_recovery_volumes(s: &str) -> Result<RecoveryVolumes, String> {
    if let Some(pct) = s.strip_suffix('%') {
        let v = pct
            .parse::<u8>()
            .map_err(|_| format!("invalid recovery percent: {s}"))?;
        if v > 100 {
            return Err(format!("invalid recovery percent: {s}"));
        }
        return Ok(RecoveryVolumes::Percent(v));
    }
    s.parse::<u32>()
        .map(RecoveryVolumes::Count)
        .map_err(|_| format!("invalid recovery volume count: {s}"))
}

fn parse_threads(s: &str) -> Result<usize, String> {
    let n = s
        .parse::<usize>()
        .map_err(|_| format!("invalid thread count: {s}"))?;
    if n == 0 {
        return Err(format!("invalid thread count: {s}"));
    }
    Ok(n)
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

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    let args: Vec<String> = raw
        .iter()
        .skip(1)
        .map(|a| common::normalize_switch(a))
        .collect();
    let cli = Cli::parse_from(std::iter::once("rar".to_string()).chain(args));
    if let Err(e) = run(cli) {
        eprintln!("rar: {e}");
        process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Create(args) => cmd_create(&args),
        Command::Update(args) => cmd_update(&args),
        Command::Freshen(args) => cmd_freshen(&args),
        Command::Move(args) => cmd_move(&args),
        Command::Delete(args) => cmd_delete(&args),
        Command::Rename(args) => cmd_rename(&args),
        Command::Lock(args) => cmd_lock(&args),
        Command::Recovery(args) => cmd_rr(&args),
        Command::Repair(args) => cmd_repair(&args),
        Command::RebuildVolumes(args) => cmd_rebuild_volumes(&args),
        Command::Sfx(args) => cmd_sfx(&args),
        Command::SfxStrip(args) => cmd_sfx_strip(&args),
        Command::CommentSet(args) => cmd_comment_set(&args),
        Command::CommentWrite(args) => cmd_comment_write(&args),
        Command::Extract(args) => cmd_extract(&args),
        Command::ExtractFlat(args) => cmd_extract_flat(&args),
        Command::Test(args) => cmd_test(&args),
        Command::VerboseList(args) => cmd_verbose_list(&args),
        Command::List(args) => cmd_list(&args),
        Command::Info(args) => cmd_info(&args),
        Command::External(ext) => {
            let name = ext.first().cloned().unwrap_or_default();
            // `i<string>` (and `ic`/`ih` variants) find strings in members.
            if name.len() > 1 && name.starts_with('i') {
                cmd_find(&name, &ext[1..])
            } else {
                Err(format!("unknown command: {name}"))
            }
        }
    }
}

fn cmd_create(args: &CreateArgs) -> Result<(), String> {
    if let Some(threads) = args.threads {
        rar5::set_compression_threads(threads);
        rar5::set_extraction_threads(threads);
    }
    let password = args.password.password.clone();
    let mut recovery_volumes_percent = None;
    let mut recovery_volume_count = None;
    if let Some(rv) = args.recovery_volumes {
        match rv {
            RecoveryVolumes::Count(n) => recovery_volume_count = Some(n),
            RecoveryVolumes::Percent(p) => recovery_volumes_percent = Some(p),
        }
    }
    let case = match (args.lowercase, args.uppercase) {
        (true, false) => Some(rar5::name_policy::CaseKind::Lower),
        (false, true) => Some(rar5::name_policy::CaseKind::Upper),
        _ => None,
    };
    let header_encrypt = args.header_encrypt.is_some();
    let mut password = password;
    if let Some(pw) = &args.header_encrypt
        && !pw.is_empty()
    {
        password = Some(pw.clone());
    }

    let archive_path = &args.archive;
    let files = &args.files;

    let opts = rar5::CreateOptions {
        solid: args.solid,
        quick_open: args.quick_open,
        blake2: args.blake2,
        password: password.clone(),
        encrypt_headers: header_encrypt,
        recovery_percent: args.recovery_percent,
        recovery_volumes_percent,
        recovery_volume_count,
        volume_size: args.volume_size,
    };

    let existing = std::path::Path::new(archive_path).exists();
    let mut rar = if existing {
        // Append to an existing archive (like `rar a`): existing members
        // are preserved verbatim.
        if args.volume_size.is_some() {
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

    let policy = rar5::name_policy::NamePolicy {
        path_prefix: args.path_prefix.clone(),
        basename_only: args.basename_only,
        strip_base: args.strip_base,
        no_recurse: args.no_recurse,
        case: case.map(|c| match c {
            rar5::name_policy::CaseKind::Lower => rar5::name_policy::CaseKind::Lower,
            rar5::name_policy::CaseKind::Upper => rar5::name_policy::CaseKind::Upper,
        }),
        include_masks: args.include_masks.clone(),
        exclude_masks: args.exclude_masks.clone(),
    };
    let collected = rar5::name_policy::collect(&policy, files, args.level)
        .map_err(|e| format!("collect: {e}"))?;
    let entries: Vec<rar5::BatchEntry<'_>> = collected
        .iter()
        .map(|c| {
            if c.is_dir {
                rar5::BatchEntry::Directory {
                    path: &c.path,
                    name: Some(&c.name),
                }
            } else {
                rar5::BatchEntry::File {
                    path: &c.path,
                    name: Some(&c.name),
                    level: c.level,
                }
            }
        })
        .collect();
    rar.add_batch(&entries).map_err(|e| format!("add: {e}"))?;

    let was_existing = existing;
    rar.close().map_err(|e| format!("close: {e}"))?;
    if args.volume_size.is_some() {
        let vols = rar5::discover_volumes(std::path::Path::new(archive_path));
        println!(
            "Created {} volume(s) ({} file(s), level {})",
            vols.len(),
            files.len(),
            args.level
        );
    } else if was_existing {
        println!(
            "Updated {archive_path} ({} file(s), level {})",
            files.len(),
            args.level
        );
    } else {
        println!(
            "Created {archive_path} ({} file(s), level {})",
            files.len(),
            args.level
        );
    }
    Ok(())
}

/// Delete members from an archive without rebuilding it (mirrors `rar d`).
fn cmd_delete(args: &DeleteArgs) -> Result<(), String> {
    let archive_path = &args.archive;
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(archive_path, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(archive_path).map_err(|e| format!("open: {e}"))?,
    };
    let names: Vec<&str> = args.names.iter().map(|s| s.as_str()).collect();
    let n = rar.delete(&names).map_err(|e| format!("delete: {e}"))?;
    println!("Deleted {n} file(s) from {archive_path}");
    Ok(())
}

/// Update an archive: add files not present, replace files whose source
/// is newer (like `rar u`).
fn cmd_update(args: &FilesArgs) -> Result<(), String> {
    cmd_update_freshen(args, false, "Updated")
}

/// Freshen the archive (like `rar f`): update members that already exist
/// when the source is newer; never add new members.
fn cmd_freshen(args: &FilesArgs) -> Result<(), String> {
    cmd_update_freshen(args, true, "Freshened")
}

/// Shared update/freshen implementation: members whose source mtime is
/// newer than the archived one are deleted and re-added. With `freshen`,
/// members missing from the archive are skipped; otherwise they are added.
fn cmd_update_freshen(args: &FilesArgs, freshen: bool, verb: &str) -> Result<(), String> {
    let archive_path = &args.archive;
    let files = &args.files;
    let password = &args.password.password;
    if !std::path::Path::new(archive_path).exists() {
        return Err(format!("archive not found: {archive_path}"));
    }

    // Decide per file: skip (unchanged), delete + re-add (newer), add.
    let rar = match password {
        Some(pw) => rar5::RarArchive::open_with_password(archive_path, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(archive_path).map_err(|e| format!("open: {e}"))?,
    };
    let mut to_delete = Vec::new();
    let mut to_add = Vec::new();
    for file in files {
        let path = std::path::Path::new(file);
        let name = arg_to_name(file);
        if let Some(entry) = rar.get_entry(&name) {
            let src_mtime = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as u32)
                .unwrap_or(0);
            if src_mtime > entry.header.mtime {
                to_delete.push(name);
                to_add.push(file.clone());
            }
            // else: unchanged, skip
        } else if !freshen {
            to_add.push(file.clone());
        }
        // Freshen skips members missing from the archive.
    }
    drop(rar);
    if to_delete.is_empty() {
        println!("{archive_path}: no files to {verb}");
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
    if to_add.is_empty() {
        println!("{archive_path}: no files to {verb}");
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
        let name = arg_to_name(file);
        rar.add_as(file, &name, 3)
            .map_err(|e| format!("add {file}: {e}"))?;
    }
    rar.close().map_err(|e| format!("close: {e}"))?;
    println!("{verb} {archive_path} ({} file(s))", to_add.len());
    Ok(())
}

/// Lock the archive (like `rar k`).
fn cmd_lock(args: &ArchiveArgs) -> Result<(), String> {
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    rar.lock().map_err(|e| format!("lock: {e}"))?;
    println!("Locked {archive}", archive = args.archive);
    Ok(())
}

/// Add an inline recovery record (like `rar rr`).
fn cmd_rr(args: &RecoveryArgs) -> Result<(), String> {
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    rar.add_recovery_record(args.percent)
        .map_err(|e| format!("rr: {e}"))?;
    println!(
        "Recovery record {}% added to {archive}",
        args.percent,
        archive = args.archive
    );
    Ok(())
}

/// Rename archived members (like `rar rn`): pairs of old/new names.
fn cmd_rename(args: &RenameArgs) -> Result<(), String> {
    if !args.pairs.len().is_multiple_of(2) {
        return Err("usage: rar rn <archive.rar> <old1> <new1> [<old2> <new2> ...]".into());
    }
    let archive_path = &args.archive;
    let pairs: Vec<(&str, &str)> = args
        .pairs
        .chunks(2)
        .map(|c| (c[0].as_str(), c[1].as_str()))
        .collect();
    let mut rar = rar5::RarArchive::open(archive_path).map_err(|e| format!("open: {e}"))?;
    let n = rar.rename(&pairs).map_err(|e| format!("rename: {e}"))?;
    println!("Renamed {n} file(s) in {archive_path}");
    Ok(())
}

/// Move files into the archive (like `rar m`): add them, then erase the
/// sources after a successful close.
fn cmd_move(args: &FilesArgs) -> Result<(), String> {
    let archive_path = &args.archive;
    let files = &args.files;
    let password = &args.password.password;
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
        let name = arg_to_name(file);
        rar.add_as(file, &name, 3)
            .map_err(|e| format!("add {file}: {e}"))?;
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
fn cmd_find(cmd: &str, args: &[String]) -> Result<(), String> {
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
fn cmd_verbose_list(args: &ArchiveArgs) -> Result<(), String> {
    let rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("{e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("{e}"))?,
    };
    common::print_verbose_list(&rar)
}

/// Repair an archive with its inline recovery record (like `rar r`).
/// Writes `fixed.<name>` when damage was found and repaired.
fn cmd_repair(args: &ArchiveArgs) -> Result<(), String> {
    let archive_path = &args.archive;
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
fn cmd_rebuild_volumes(args: &ArchiveArgs) -> Result<(), String> {
    let first = &args.archive;
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
fn cmd_comment_set(args: &ArchiveArgs) -> Result<(), String> {
    use std::io::Read;
    let mut comment = Vec::new();
    std::io::stdin()
        .read_to_end(&mut comment)
        .map_err(|e| format!("stdin: {e}"))?;
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    rar.set_comment(&comment)
        .map_err(|e| format!("comment: {e}"))?;
    if comment.is_empty() {
        println!("Comment removed from {archive}", archive = args.archive);
    } else {
        println!("Comment added to {archive}", archive = args.archive);
    }
    Ok(())
}

/// Write the archive comment to stdout (like `rar cw`).
fn cmd_comment_write(args: &ArchiveArgs) -> Result<(), String> {
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    if let Some(comment) = rar.get_comment().map_err(|e| format!("cw: {e}"))? {
        use std::io::Write;
        std::io::stdout()
            .write_all(&comment)
            .map_err(|e| format!("stdout: {e}"))?;
    }
    Ok(())
}

/// Convert an archive to or from SFX (like `rar s` / `rar s-`).
fn cmd_sfx_strip(args: &ArchiveArgs) -> Result<(), String> {
    let archive_path = &args.archive;
    let input = std::fs::read(archive_path).map_err(|e| format!("read: {e}"))?;
    let sfx_offset = rar5::sfx_offset_of(&input)
        .ok_or_else(|| format!("{archive_path} is not an SFX archive"))?;
    let base = archive_path
        .strip_suffix(".sfx")
        .or_else(|| archive_path.strip_suffix(".SFX"))
        .map(|b| b.to_string())
        .unwrap_or_else(|| format!("{archive_path}.plain"));
    let out_path = format!("{base}.rar");
    std::fs::write(&out_path, &input[sfx_offset..]).map_err(|e| format!("write: {e}"))?;
    println!("Removed SFX module: {out_path}");
    Ok(())
}

/// Convert an archive to SFX (like `rar s`).
fn cmd_sfx(args: &SfxArgs) -> Result<(), String> {
    let archive_path = &args.archive;
    let input = std::fs::read(archive_path).map_err(|e| format!("read: {e}"))?;

    // Creation: prepend the SFX module.
    let module_path = match &args.module {
        Some(m) => m.clone(),
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
fn cmd_extract(args: &ExtractArgs) -> Result<(), String> {
    if let Some(threads) = args.threads {
        rar5::set_extraction_threads(threads);
    }
    let dest = &args.dest;
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    let count = rar.list().len();
    rar.extract_all(dest).map_err(|e| format!("{e}"))?;
    println!("Extracted {count} file(s) to {dest}");
    Ok(())
}

/// Extract without archived paths (like `rar e`).
fn cmd_extract_flat(args: &ExtractArgs) -> Result<(), String> {
    if let Some(threads) = args.threads {
        rar5::set_extraction_threads(threads);
    }
    let dest = &args.dest;
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    let count = rar.list().len();
    rar.extract_all_with_options(
        dest,
        rar5::ExtractOptions {
            flat_paths: true,
            ..Default::default()
        },
    )
    .map_err(|e| format!("{e}"))?;
    println!("Extracted {count} file(s) to {dest}");
    Ok(())
}

/// Test archive contents (like `rar t`).
fn cmd_test(args: &ArchiveArgs) -> Result<(), String> {
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
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

/// Normalize a path argument into an archive name: relative paths stay as
/// given, absolute paths drop the leading slash (like `rar`).
fn arg_to_name(arg: &str) -> String {
    rar5::name_policy::arg_to_name(arg)
}


fn cmd_list(args: &ArchiveArgs) -> Result<(), String> {
    let rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("{e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("{e}"))?,
    };

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

fn cmd_info(args: &ArchiveArgs) -> Result<(), String> {
    let rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("{e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("{e}"))?,
    };

    let files: Vec<_> = rar.list().iter().filter(|e| !e.is_dir()).collect();
    let dirs: Vec<_> = rar.list().iter().filter(|e| e.is_dir()).collect();
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
