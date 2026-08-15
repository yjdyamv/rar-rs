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
    /// Assume yes on all queries (like `-y`)
    #[arg(short = 'y', long, global = true)]
    yes: bool,
    /// Quiet mode: suppress informational messages (like `-idq` / `-inul`)
    #[arg(long, global = true)]
    quiet: bool,
    /// Send informational messages to stderr (like `-ierr`)
    #[arg(long, global = true)]
    err: bool,
    /// Work directory (like `-w<path>`)
    #[arg(long = "work-dir", global = true)]
    work_dir: Option<String>,
    /// Misc switches (-ow, -tsp, -ilog, -ver, and accepted no-ops)
    #[command(flatten)]
    misc: common::MiscSwitches,
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
    /// Change archive parameters (like `rar ch`; currently `-cl`/`-cu`
    /// member name case conversion)
    #[command(visible_alias = "ch")]
    Change(ChangeArgs),
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
    /// Set the archive comment (from stdin, or `-z<file>`)
    #[command(visible_alias = "c")]
    CommentSet(CommentArgs),
    /// Write the archive comment to stdout
    #[command(visible_alias = "cw")]
    CommentWrite(ArchiveArgs),
    /// Print file to stdout (like `rar p`)
    #[command(visible_alias = "p")]
    Print(PrintArgs),
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

/// `rar ch` parameters: member name case conversion (-cl / -cu).
#[derive(Args)]
struct ChangeArgs {
    #[command(flatten)]
    password: common::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    /// Convert stored names to lowercase (-cl)
    #[arg(long = "lowercase")]
    lowercase: bool,
    /// Convert stored names to uppercase (-cu)
    #[arg(long = "uppercase")]
    uppercase: bool,
}

/// Archive path plus an optional member to print.
#[derive(Args)]
struct PrintArgs {
    #[command(flatten)]
    password: common::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(value_name = "FILE")]
    file: Option<String>,
}

/// Comment setting: stdin by default, or `-z<file>`.
#[derive(Args)]
struct CommentArgs {
    #[command(flatten)]
    password: common::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    /// Read the comment from a file (like `-z<file>`)
    #[arg(long = "comment-file")]
    comment_file: Option<String>,
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
    /// Append the archive base name as a destination subdirectory
    /// (like `-ad`)
    #[arg(long = "append-dir")]
    append_dir: bool,
    /// Overwrite mode (like `-o+` / `-o-`)
    #[arg(
        long = "overwrite",
        value_name = "MODE",
        value_parser = ["always", "never"]
    )]
    overwrite: Option<String>,
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
    /// Dictionary size for compression (like `-md<size>`)
    #[arg(long = "dict-size", value_name = "SIZE")]
    dict_size: Option<String>,
    /// Extraction dictionary cap (like `-mdx<size>`; accepted, no effect)
    #[arg(long = "dict-extract", value_name = "SIZE")]
    dict_extract: Option<String>,
    /// Save/restore file times (like `-ts[m,c,a][+,-,1]`; repeatable)
    #[arg(long = "ts", value_name = "SPEC", action = clap::ArgAction::Append)]
    ts_specs: Vec<String>,
    /// Keep the archive's original modification time when updating
    /// (like `-tk`)
    #[arg(long = "keep-time")]
    keep_time: bool,
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
    /// Dictionary size for compression (like `-md<size>[k|m|g]`; no unit
    /// means MiB, valid values 128K..4G powers of two)
    #[arg(long = "dict-size", value_name = "SIZE")]
    dict_size: Option<String>,
    /// Extraction dictionary cap (like `-mdx<size>`; accepted, no effect
    /// for RAR5 which is capped at 4 GiB)
    #[arg(long = "dict-extract", value_name = "SIZE")]
    dict_extract: Option<String>,
    /// Save/restore file times (like `-ts[m,c,a][+,-,1]`; repeatable)
    #[arg(long = "ts", value_name = "SPEC", action = clap::ArgAction::Append)]
    ts_specs: Vec<String>,
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
    /// Store full paths without the drive letter (like `-ep2`)
    #[arg(long = "full-paths")]
    full_paths: bool,
    /// Store full paths including the drive letter (like `-ep3`)
    #[arg(long = "full-paths-drive")]
    full_paths_drive: bool,
    /// Do not recurse into directories
    #[arg(long = "no-recurse")]
    no_recurse: bool,
    /// Recurse subdirectories (like `-r`; the default for directory args)
    #[arg(long = "recurse")]
    recurse: bool,
    /// Recurse, but wildcards only match names without path separators
    /// (like `-r0`)
    #[arg(long = "recurse-zero")]
    recurse_zero: bool,
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
    /// Exclude masks read from a list file (repeatable, like `-x@listfile`)
    #[arg(long = "exclude-list", value_name = "FILE", action = clap::ArgAction::Append)]
    exclude_list_files: Vec<String>,
    /// Include masks read from a list file (repeatable, like `-n@listfile`)
    #[arg(long = "include-list", value_name = "FILE", action = clap::ArgAction::Append)]
    include_list_files: Vec<String>,
    /// Only process files modified after this date (like `-ta<date>`,
    /// YYYYMMDDHHMMSS, trailing parts optional)
    #[arg(long = "after", value_name = "DATE")]
    after: Option<String>,
    /// Only process files modified before this date (like `-tb<date>`)
    #[arg(long = "before", value_name = "DATE")]
    before: Option<String>,
    /// Only process files newer than this period (like `-tn[mods]<time>`,
    /// period is `[<ndays>d][<nhours>h][<nminutes>m][<nseconds>s]`;
    /// may be repeated, all filters must match)
    #[arg(long = "tn-filter", value_name = "PERIOD")]
    tn_filters: Vec<String>,
    /// Only process files older than this period (like `-to[mods]<time>`)
    #[arg(long = "to-filter", value_name = "PERIOD")]
    to_filters: Vec<String>,
    /// Set the archive time to the newest member (like `-tl`)
    #[arg(long = "set-latest-time")]
    latest_time: bool,
    /// Keep the archive's original modification time when updating an
    /// existing archive (like `-tk`)
    #[arg(long = "keep-time")]
    keep_time: bool,
    /// Generate the archive name from the current date (like `-ag[format]`;
    /// `*` in the name is replaced, `YYYY`/`MM`/`DD`/`HH`/`MM`/`SS` in the
    /// format are substituted)
    #[arg(long = "auto-name", num_args = 0..=1, default_missing_value = "")]
    auto_name: Option<String>,
    /// Save symbolic links as links instead of the file (like `-ol`)
    #[arg(long = "links")]
    store_links: bool,
    /// Save hard links as links instead of the file (like `-oh`)
    #[arg(long = "hardlinks")]
    store_hardlinks: bool,
    /// Only process files smaller than this size (like `-sl<size>`, units
    /// b/k/m/g)
    #[arg(long = "size-less", value_name = "SIZE", value_parser = parse_size)]
    size_less: Option<u64>,
    /// Only process files larger than this size (like `-sm<size>`)
    #[arg(long = "size-more", value_name = "SIZE", value_parser = parse_size)]
    size_more: Option<u64>,
    /// Do not add empty directories (like `-ed`)
    #[arg(long = "no-empty-dirs")]
    no_empty_dirs: bool,
    /// Do not show the archive comment (like `-c-`; accepted, comments are
    /// never displayed by this tool)
    #[arg(long = "no-comment", global = true)]
    no_comment: bool,
    /// Read one member from stdin under this name (like `-si<name>`)
    #[arg(long = "stdin-name", value_name = "NAME")]
    stdin_name: Option<String>,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(value_name = "FILES", required_unless_present = "stdin_name")]
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
    } else if let Some(num) = s.strip_suffix('g').or_else(|| s.strip_suffix('G')) {
        num.parse::<u64>()
            .map(|n| n * 1024 * 1024 * 1024)
            .map_err(|_| format!("invalid size: {s}"))
    } else {
        s.parse::<u64>().map_err(|_| format!("invalid size: {s}"))
    }
}

/// Parse a WinRAR `-md<size>[k|m|g]` dictionary size into a RAR5 dict log
/// (`128 KiB << log`). No unit means MiB. Only the RAR5 power-of-two
/// range 128 KiB .. 4 GiB is valid; anything else is rejected with
/// WinRAR's wording (`Unknown option: md...`).
fn parse_dict_log(s: &str) -> Result<u8, String> {
    if s.is_empty() {
        return Err("Unknown option: md".into());
    }
    let (num, mult) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1024u64),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1024 * 1024),
    };
    let bytes = num
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .filter(|b| *b >= 128 * 1024 && *b <= 4 * 1024 * 1024 * 1024 && b.is_power_of_two())
        .ok_or_else(|| format!("Unknown option: md{s}"))?;
    // 128 KiB = 2^17, so log = trailing_zeros - 17 (0..=15).
    Ok((bytes.trailing_zeros() - 17) as u8)
}

/// Parsed `-ts` settings: which times to save and at what precision.
#[cfg(test)]
mod dict_log_tests {
    #[test]
    fn parse_dict_log_accepts_rar5_range() {
        assert_eq!(super::parse_dict_log("128k").unwrap(), 0);
        assert_eq!(super::parse_dict_log("128K").unwrap(), 0);
        assert_eq!(super::parse_dict_log("1m").unwrap(), 3);
        assert_eq!(super::parse_dict_log("64").unwrap(), 9); // no unit = MiB
        assert_eq!(super::parse_dict_log("1g").unwrap(), 13);
        assert_eq!(super::parse_dict_log("2g").unwrap(), 14);
        assert_eq!(super::parse_dict_log("4G").unwrap(), 15);
    }

    #[test]
    fn parse_dict_log_rejects_invalid_values() {
        for bad in ["", "3m", "0", "100k", "5g", "64m1", "abc", "1t"] {
            let err = super::parse_dict_log(bad).unwrap_err();
            assert!(err.starts_with("Unknown option: md"), "{bad}: {err}");
        }
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
    common::QUIET.store(cli.quiet, std::sync::atomic::Ordering::Relaxed);
    common::ERR.store(cli.err, std::sync::atomic::Ordering::Relaxed);
    if let Some(dir) = &cli.work_dir {
        if let Err(e) = std::env::set_current_dir(dir) {
            eprintln!("rar: cannot change to work directory {dir}: {e}");
            process::exit(1);
        }
    }
    let _ = cli.yes; // no interactive prompts exist yet; accepted for parity
    let log_errors = cli.misc.log_errors.clone();
    if let Err(e) = run(cli) {
        eprintln!("rar: {e}");
        if let Some(log) = &log_errors {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .and_then(|mut f| {
                    use std::io::Write;
                    f.write_all(format!("rar: {e}\n").as_bytes())
                });
        }
        process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let misc = &cli.misc;
    match cli.command {
        Command::Create(args) => cmd_create(&args, misc),
        Command::Update(args) => cmd_update(&args, misc),
        Command::Freshen(args) => cmd_freshen(&args, misc),
        Command::Move(args) => cmd_move(&args, misc),
        Command::Delete(args) => cmd_delete(&args),
        Command::Rename(args) => cmd_rename(&args),
        Command::Change(args) => cmd_change(&args),
        Command::Lock(args) => cmd_lock(&args),
        Command::Recovery(args) => cmd_rr(&args),
        Command::Repair(args) => cmd_repair(&args),
        Command::RebuildVolumes(args) => cmd_rebuild_volumes(&args),
        Command::Sfx(args) => cmd_sfx(&args),
        Command::SfxStrip(args) => cmd_sfx_strip(&args),
        Command::CommentSet(args) => cmd_comment_set(&args),
        Command::CommentWrite(args) => cmd_comment_write(&args),
        Command::Print(args) => cmd_print(&args),
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

fn cmd_create(args: &CreateArgs, misc: &common::MiscSwitches) -> Result<(), String> {
    if let Some(threads) = args.threads {
        rar5::set_compression_threads(threads);
        rar5::set_extraction_threads(threads);
    }
    let mut password = args.password.password.clone();
    // `-p-` (and bare `-p`, whose interactive prompt we do not simulate)
    // normalizes to an empty password; treat it as "no password" like
    // WinRAR, instead of encrypting with an empty key.
    if password.as_deref() == Some("") {
        password = None;
    }
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
    if let Some(pw) = &args.header_encrypt
        && !pw.is_empty()
    {
        password = Some(pw.clone());
    }
    let ts = common::parse_ts_specs(&args.ts_specs)?;

    let mut archive_path = args.archive.clone();
    // -ag: generate the archive name from the current date (default format
    // YYYYMMDDHHMMSS, like WinRAR): `*` in the name is replaced, otherwise
    // the stamp is inserted before the extension.
    if let Some(fmt) = &args.auto_name {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let days = (now / 86400) as i64;
        let (y, mo, d) = common::civil_from_days(days);
        let tod = now % 86400;
        let stamp = if fmt.is_empty() {
            format!(
                "{y:04}{mo:02}{d:02}{:02}{:02}{:02}",
                tod / 3600,
                (tod % 3600) / 60,
                tod % 60
            )
        } else {
            fmt.replace("YYYY", &format!("{y:04}"))
                .replace("MM", &format!("{mo:02}"))
                .replace("DD", &format!("{d:02}"))
                .replace("HH", &format!("{:02}", tod / 3600))
                .replace("mm", &format!("{:02}", (tod % 3600) / 60))
                .replace("SS", &format!("{:02}", tod % 60))
        };
        archive_path = if archive_path.contains('*') {
            archive_path.replace('*', &stamp)
        } else if let Some(dot) = archive_path.rfind('.') {
            archive_path.insert_str(dot, &stamp);
            archive_path
        } else {
            archive_path.push_str(&stamp);
            archive_path
        };
    }
    let archive_path = &archive_path;
    let files = &args.files;

    let opts = rar5::CreateOptions {
        solid: args.solid,        quick_open: args.quick_open,
        blake2: args.blake2,
        password: password.clone(),
        encrypt_headers: header_encrypt,
        recovery_percent: args.recovery_percent,
        recovery_volumes_percent,
        recovery_volume_count,
        volume_size: args.volume_size,
        dict_size_log: args.dict_size.as_deref().map(parse_dict_log).transpose()?,
        save_ctime: ts.save_ctime,
        save_atime: ts.save_atime,
        save_mtime: ts.save_mtime,
        save_owner: misc.owner,
        time_precision_seconds: ts.precision_seconds,
    };

    let existing = std::path::Path::new(archive_path).exists();
    // -tk: keep the archive's original modification time on update.
    let orig_mtime = if args.keep_time && existing {
        std::fs::metadata(archive_path)
            .and_then(|m| m.modified())
            .ok()
    } else {
        None
    };
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

    let mut include_masks = args.include_masks.clone();
    let mut exclude_masks = args.exclude_masks.clone();
    for file in &args.include_list_files {
        include_masks.extend(read_mask_file(file)?);
    }
    for file in &args.exclude_list_files {
        exclude_masks.extend(read_mask_file(file)?);
    }
    let policy = rar5::name_policy::NamePolicy {
        path_prefix: args.path_prefix.clone(),
        basename_only: args.basename_only,
        strip_base: args.strip_base,
        full_paths: args.full_paths,
        full_paths_drive: args.full_paths_drive,
        no_recurse: args.no_recurse,
        wildcard_top_only: args.recurse_zero,
        case: case.map(|c| match c {
            rar5::name_policy::CaseKind::Lower => rar5::name_policy::CaseKind::Lower,
            rar5::name_policy::CaseKind::Upper => rar5::name_policy::CaseKind::Upper,
        }),
        include_masks,
        exclude_masks,
    };
    let mut collected = rar5::name_policy::collect(&policy, files, args.level)
        .map_err(|e| format!("collect: {e}"))?;
    // Never archive the archive itself (WinRAR skips the output file when
    // a directory argument covers it, e.g. `rar a x.rar .`).
    if let Ok(abs_archive) = std::fs::canonicalize(archive_path) {
        collected.retain(|c| {
            !matches!(std::fs::canonicalize(&c.path), Ok(p) if p == abs_archive)
        });
    }
    // -sl / -sm / -ed: size filters and skip-empty-directories.
    if args.size_less.is_some() || args.size_more.is_some() || args.no_empty_dirs {
        collected.retain(|c| {
            if c.is_dir {
                if args.no_empty_dirs {
                    return std::fs::read_dir(&c.path)
                        .map(|mut it| it.next().is_some())
                        .unwrap_or(true);
                }
                return true;
            }
            let size = std::fs::metadata(&c.path).map(|m| m.len()).unwrap_or(0);
            let less_ok = args.size_less.is_none_or(|s| size < s);
            let more_ok = args.size_more.is_none_or(|s| size > s);
            less_ok && more_ok
        });
    }
    // Time filters (-ta / -tb absolute dates, -tn / -to relative periods):
    // only members whose time falls in the window are added (directories
    // always pass). `-tn<period>` keeps files with time >= now - period
    // (exact match included), `-to<period>` keeps time < now - period
    // (exact match excluded); WinRAR treats an unparsable/empty period as 0
    // seconds. Multiple -tn/-to switches combine with AND logic.
    if args.after.is_some()
        || args.before.is_some()
        || !args.tn_filters.is_empty()
        || !args.to_filters.is_empty()
    {
        let after = args.after.as_deref().map(parse_rar_date).transpose()?;
        let before = args.before.as_deref().map(parse_rar_date).transpose()?;
        // Compare in nanosecond precision (like WinRAR): whole-second
        // truncation would wrongly drop files created within the same
        // second as the run, e.g. `-to` with an empty period.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let filters: Vec<(TimeKind, u64, bool)> = args
            .tn_filters
            .iter()
            .map(|s| {
                let (k, p) = parse_period_filter(s);
                (k, p, true)
            })
            .chain(args.to_filters.iter().map(|s| {
                let (k, p) = parse_period_filter(s);
                (k, p, false)
            }))
            .collect();
        collected.retain(|c| {
            if c.is_dir {
                return true;
            }
            let meta = match std::fs::metadata(&c.path) {
                Ok(m) => m,
                Err(_) => return false,
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let after_ok = after.is_none_or(|a| mtime > u128::from(a) * 1_000_000_000);
            let before_ok = before.is_none_or(|b| mtime < u128::from(b) * 1_000_000_000);
            let period_ok = filters.iter().all(|&(kind, period, is_tn)| {
                let t = file_time(&meta, kind);
                let bound = now.saturating_sub(u128::from(period) * 1_000_000_000);
                if is_tn { t >= bound } else { t < bound }
            });
            after_ok && before_ok && period_ok
        });
    }
    // -ol / -oh: symbolic links and hard links are stored as redirect
    // records instead of their data. The data member of a hard-link group
    // (first occurrence) is archived normally; the rest reference it.
    let mut redirects: Vec<(String, u64, String)> = Vec::new();
    if args.store_links {
        let mut keep = Vec::with_capacity(collected.len());
        for c in collected.drain(..) {
            if c.is_dir {
                keep.push(c);
                continue;
            }
            match std::fs::symlink_metadata(&c.path) {
                Ok(m) if m.file_type().is_symlink() => {
                    if let Ok(target) = std::fs::read_link(&c.path) {
                        redirects.push((c.name.clone(), 1, target.to_string_lossy().into_owned()));
                        continue;
                    }
                    keep.push(c);
                }
                _ => keep.push(c),
            }
        }
        collected = keep;
    }
    if args.store_hardlinks {
        #[cfg(unix)]
        let mut seen: std::collections::HashMap<(u64, u64), String> = std::collections::HashMap::new();
        let mut keep = Vec::with_capacity(collected.len());
        for c in collected.drain(..) {
            if c.is_dir {
                keep.push(c);
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if let Ok(m) = std::fs::metadata(&c.path) {
                    let key = (m.dev(), m.ino());
                    if let Some(first) = seen.get(&key) {
                        redirects.push((c.name.clone(), 4, first.clone()));
                        continue;
                    }
                    seen.insert(key, c.name.clone());
                }
            }
            keep.push(c);
        }
        collected = keep;
    }
    // WinRAR aborts with "WARNING: No files" (exit code 10) and leaves the
    // archive untouched when every candidate was filtered out; a newly
    // created archive file is removed again.
    if collected.is_empty() && args.stdin_name.is_none() && redirects.is_empty() {
        drop(rar);
        if !existing {
            let _ = std::fs::remove_file(archive_path);
        }
        info!("WARNING: No files");
        process::exit(10);
    }
    // -tsp: snapshot source access times before reading the files.
    #[cfg(unix)]
    let ts_preserve_atimes: Vec<(std::path::PathBuf, std::time::SystemTime)> = if misc.ts_preserve
    {
        use std::os::unix::fs::MetadataExt;
        collected
            .iter()
            .filter(|c| !c.is_dir)
            .filter_map(|c| {
                let m = std::fs::metadata(&c.path).ok()?;
                Some((
                    c.path.clone(),
                    std::time::UNIX_EPOCH
                        + std::time::Duration::from_secs(m.atime() as u64)
                        + std::time::Duration::from_nanos(m.atime_nsec() as u64),
                ))
            })
            .collect()
    } else {
        Vec::new()
    };
    #[cfg(not(unix))]
    let ts_preserve_atimes: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
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
    // Link redirects are recorded after their data members (the reference
    // target name is what matters, not the order).
    for (name, redir_type, target) in &redirects {
        rar.add_redirect(name, *redir_type, target)
            .map_err(|e| format!("link {name}: {e}"))?;
    }

    // -si<name>: one member read from stdin.
    if let Some(name) = &args.stdin_name {
        use std::io::Read;
        let mut data = Vec::new();
        std::io::stdin()
            .read_to_end(&mut data)
            .map_err(|e| format!("stdin: {e}"))?;
        let name = name.replace('\\', "/");
        rar.add_bytes(&name, &data, args.level)
            .map_err(|e| format!("add stdin: {e}"))?;
    }

    let was_existing = existing;
    rar.close().map_err(|e| format!("close: {e}"))?;
    // -tsp: restore the source files' access times that were recorded
    // before archiving (reading the files may have refreshed them).
    if misc.ts_preserve {
        #[cfg(unix)]
        for (path, atime) in &ts_preserve_atimes {
            let _ = std::fs::File::options()
                .write(true)
                .open(path)
                .and_then(|f| f.set_times(std::fs::FileTimes::new().set_accessed(*atime)));
        }
        #[cfg(not(unix))]
        {
            let _ = misc;
            let _ = &ts_preserve_atimes;
        }
    }
    // -tk: restore the archive's original modification time.
    if let Some(t) = orig_mtime {
        let _ = std::fs::File::options()
            .write(true)
            .open(archive_path)
            .and_then(|f| f.set_times(std::fs::FileTimes::new().set_modified(t)));
    }
    // -tl: set the archive's modification time to the newest member.
    if args.latest_time && !collected.is_empty() {
        let latest = collected
            .iter()
            .filter(|c| !c.is_dir)
            .filter_map(|c| std::fs::metadata(&c.path).ok()?.modified().ok())
            .max();
        if let Some(t) = latest {
            let _ = std::fs::File::options()
                .write(true)
                .open(archive_path)
                .and_then(|f| f.set_times(std::fs::FileTimes::new().set_modified(t)));
        }
    }
    if args.volume_size.is_some() {
        let vols = rar5::discover_volumes(std::path::Path::new(archive_path));
        info!(
            "Created {} volume(s) ({} file(s), level {})",
            vols.len(),
            files.len(),
            args.level
        );
    } else if was_existing {
        info!(
            "Updated {archive_path} ({} file(s), level {})",
            files.len(),
            args.level
        );
    } else {
        info!(
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
    info!("Deleted {n} file(s) from {archive_path}");
    Ok(())
}

/// Update an archive: add files not present, replace files whose source
/// is newer (like `rar u`).
fn cmd_update(args: &FilesArgs, misc: &common::MiscSwitches) -> Result<(), String> {
    cmd_update_freshen(args, false, "Updated", misc)
}

/// Freshen the archive (like `rar f`): update members that already exist
/// when the source is newer; never add new members.
fn cmd_freshen(args: &FilesArgs, misc: &common::MiscSwitches) -> Result<(), String> {
    cmd_update_freshen(args, true, "Freshened", misc)
}

/// Shared update/freshen implementation: members whose source mtime is
/// newer than the archived one are deleted and re-added. With `freshen`,
/// members missing from the archive are skipped; otherwise they are added.
fn cmd_update_freshen(
    args: &FilesArgs,
    freshen: bool,
    verb: &str,
    misc: &common::MiscSwitches,
) -> Result<(), String> {
    let archive_path = &args.archive;
    let files = &args.files;
    let password = &args.password.password;
    if !std::path::Path::new(archive_path).exists() {
        return Err(format!("archive not found: {archive_path}"));
    }
    // -tk: keep the archive's original modification time.
    let orig_mtime = if args.keep_time {
        std::fs::metadata(archive_path)
            .and_then(|m| m.modified())
            .ok()
    } else {
        None
    };

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
        info!("{archive_path}: no files to {verb}");
        return Ok(());
    }
    {
        let mut rar = match &password {
            Some(pw) => rar5::RarArchive::open_with_password(archive_path, pw)
                .map_err(|e| format!("open: {e}"))?,
            None => rar5::RarArchive::open(archive_path).map_err(|e| format!("open: {e}"))?,
        };
        if let Some(ver_spec) = &misc.version_control {
            // -ver[n]: keep previous versions on update. Existing members
            // `name`, `name;1`, ... shift down the chain (`name;1` is the
            // newest previous version); with a limit `n`, older versions
            // beyond `name;n` are dropped.
            let max_versions: Option<u32> = if ver_spec.is_empty() {
                None
            } else {
                ver_spec.parse::<u32>().ok().filter(|n| *n > 0)
            };
            let mut renames: Vec<(String, String)> = Vec::new();
            let mut to_drop: Vec<String> = Vec::new();
            for name in &to_delete {
                // Collect existing versions of `name`.
                let mut versions: Vec<(u32, String)> = rar
                    .namelist()
                    .iter()
                    .filter_map(|n| {
                        if *n == name {
                            Some((0, n.to_string()))
                        } else if let Some(rest) = n.strip_prefix(&format!("{name};")) {
                            rest.parse::<u32>().ok().map(|v| (v, n.to_string()))
                        } else {
                            None
                        }
                    })
                    .collect();
                if versions.is_empty() {
                    continue;
                }
                versions.sort_by_key(|(v, _)| *v);
                // Shift the old versions up by one, dropping beyond the cap.
                for (v, full) in versions.iter().rev() {
                    let new_suffix = v + 1;
                    if max_versions.is_some_and(|m| new_suffix > m) {
                        to_drop.push(full.clone());
                    } else {
                        let new_name = if *v == 0 {
                            format!("{name};1")
                        } else {
                            format!("{name};{new_suffix}")
                        };
                        renames.push((full.clone(), new_name));
                    }
                }
            }
            if !renames.is_empty() {
                let pairs: Vec<(&str, &str)> =
                    renames.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                rar.rename(&pairs).map_err(|e| format!("rename: {e}"))?;
            }
            if !to_drop.is_empty() {
                let names: Vec<&str> = to_drop.iter().map(|s| s.as_str()).collect();
                rar.delete(&names).map_err(|e| format!("delete: {e}"))?;
            }
        } else {
            let names: Vec<&str> = to_delete.iter().map(|s| s.as_str()).collect();
            rar.delete(&names).map_err(|e| format!("delete: {e}"))?;
        }
    }
    if to_add.is_empty() {
        info!("{archive_path}: no files to {verb}");
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
        let ts = common::parse_ts_specs(&args.ts_specs)?;
        let create_opts = rar5::CreateOptions {
            password: password.clone(),
            dict_size_log: args
                .dict_size
                .as_deref()
                .map(parse_dict_log)
                .transpose()?,
            save_ctime: ts.save_ctime,
            save_atime: ts.save_atime,
            save_mtime: ts.save_mtime,
        save_owner: misc.owner,
            time_precision_seconds: ts.precision_seconds,
            ..Default::default()
        };
        rar5::RarArchive::create_with_options(archive_path, create_opts)
            .map_err(|e| format!("create: {e}"))?
    };
    for file in &to_add {
        let name = arg_to_name(file);
        rar.add_as(file, &name, 3)
            .map_err(|e| format!("add {file}: {e}"))?;
    }
    rar.close().map_err(|e| format!("close: {e}"))?;
    // -tk: restore the original archive mtime after the rewrite.
    if let Some(t) = orig_mtime {
        let _ = std::fs::File::options()
            .write(true)
            .open(archive_path)
            .and_then(|f| f.set_times(std::fs::FileTimes::new().set_modified(t)));
    }
    info!("{verb} {archive_path} ({} file(s))", to_add.len());
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
    info!("Locked {archive}", archive = args.archive);
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
    info!(
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
    info!("Renamed {n} file(s) in {archive_path}");
    Ok(())
}

/// Move files into the archive (like `rar m`): add them, then erase the
/// sources after a successful close.
fn cmd_move(args: &FilesArgs, misc: &common::MiscSwitches) -> Result<(), String> {
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
        let ts = common::parse_ts_specs(&args.ts_specs)?;
        let create_opts = rar5::CreateOptions {
            password: password.clone(),
            dict_size_log: args
                .dict_size
                .as_deref()
                .map(parse_dict_log)
                .transpose()?,
            save_ctime: ts.save_ctime,
            save_atime: ts.save_atime,
            save_mtime: ts.save_mtime,
        save_owner: misc.owner,
            time_precision_seconds: ts.precision_seconds,
            ..Default::default()
        };
        rar5::RarArchive::create_with_options(archive_path, create_opts)
            .map_err(|e| format!("create: {e}"))?
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
    info!("Moved {} file(s) to {archive_path}", files.len());
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
        info!("All OK");
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
    info!("Repaired {archive_path} -> {fixed_path}");
    Ok(())
}

/// Rebuild missing volumes from the `.rev` recovery volumes (like `rar rc`).
fn cmd_rebuild_volumes(args: &ArchiveArgs) -> Result<(), String> {
    let first = &args.archive;
    let rebuilt = rar5::rebuild_missing_volumes(std::path::Path::new(first))
        .map_err(|e| format!("rc: {e}"))?;
    if rebuilt.is_empty() {
        info!("All volumes present");
    } else {
        for path in &rebuilt {
            info!("Rebuilt {}", path.display());
        }
    }
    Ok(())
}

/// Set the archive comment (like `rar c`), from stdin or `-z<file>`;
/// empty input removes the comment.
fn cmd_comment_set(args: &CommentArgs) -> Result<(), String> {
    use std::io::Read;
    let mut comment = Vec::new();
    if let Some(file) = &args.comment_file {
        std::fs::File::open(file)
            .and_then(|mut f| f.read_to_end(&mut comment))
            .map_err(|e| format!("read comment file {file}: {e}"))?;
    } else {
        std::io::stdin()
            .read_to_end(&mut comment)
            .map_err(|e| format!("stdin: {e}"))?;
    }
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    rar.set_comment(&comment)
        .map_err(|e| format!("comment: {e}"))?;
    if comment.is_empty() {
        info!("Comment removed from {archive}", archive = args.archive);
    } else {
        info!("Comment added to {archive}", archive = args.archive);
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
    info!("Removed SFX module: {out_path}");
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
    info!("Created {out_path}");
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

/// Change archive parameters (like `rar ch`): member name case conversion
/// with `-cl` / `-cu`.
fn cmd_change(args: &ChangeArgs) -> Result<(), String> {
    let kind = match (args.lowercase, args.uppercase) {
        (true, false) => rar5::name_policy::CaseKind::Lower,
        (false, true) => rar5::name_policy::CaseKind::Upper,
        _ => return Err("usage: rar ch [-cl | -cu] <archive.rar>".into()),
    };
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    let names: Vec<String> = rar.namelist().into_iter().map(|s| s.to_string()).collect();
    let mut pairs = Vec::new();
    for name in names {
        let converted = match kind {
            rar5::name_policy::CaseKind::Lower => name.to_lowercase(),
            rar5::name_policy::CaseKind::Upper => name.to_uppercase(),
        };
        if converted != name {
            pairs.push((name, converted));
        }
    }
    if pairs.is_empty() {
        info!("{archive}: no names to convert", archive = args.archive);
        return Ok(());
    }
    let pairs_ref: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let n = rar
        .rename(&pairs_ref)
        .map_err(|e| format!("ch: {e}"))?;
    info!("Converted {n} name(s) in {archive}", archive = args.archive);
    Ok(())
}

/// Print a member to stdout (like `rar p`).
fn cmd_print(args: &PrintArgs) -> Result<(), String> {
    use std::io::Write;
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    if let Some(file) = &args.file {
        let data = rar.read(file).map_err(|e| format!("{e}"))?;
        std::io::stdout()
            .write_all(&data)
            .map_err(|e| format!("stdout: {e}"))?;
    } else {
        let names: Vec<String> = rar.namelist().into_iter().map(|s| s.to_string()).collect();
        for name in names {
            let data = rar.read(&name).map_err(|e| format!("{name}: {e}"))?;
            std::io::stdout()
                .write_all(&data)
                .map_err(|e| format!("stdout: {e}"))?;
        }
    }
    Ok(())
}

/// Extract with full paths (like `rar x`).
fn cmd_extract(args: &ExtractArgs) -> Result<(), String> {
    if let Some(threads) = args.threads {
        rar5::set_extraction_threads(threads);
    }
    let dest = extract_dest(args)?;
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    let count = rar.list().len();
    rar.extract_all_with_options(
        &dest,
        rar5::ExtractOptions {
            skip_existing: args.overwrite.as_deref() == Some("never"),
            ..Default::default()
        },
    )
    .map_err(|e| format!("{e}"))?;
    info!("Extracted {count} file(s) to {}", dest.display());
    Ok(())
}

/// Destination directory, honoring `-ad` (append the archive base name).
fn extract_dest(args: &ExtractArgs) -> Result<std::path::PathBuf, String> {
    Ok(common::extract_dest(&args.dest, &args.archive, args.append_dir))
}

/// Extract without archived paths (like `rar e`).
fn cmd_extract_flat(args: &ExtractArgs) -> Result<(), String> {
    if let Some(threads) = args.threads {
        rar5::set_extraction_threads(threads);
    }
    let dest = extract_dest(args)?;
    let mut rar = match &args.password.password {
        Some(pw) => rar5::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar5::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    let count = rar.list().len();
    rar.extract_all_with_options(
        &dest,
        rar5::ExtractOptions {
            flat_paths: true,
            skip_existing: args.overwrite.as_deref() == Some("never"),
            ..Default::default()
        },
    )
    .map_err(|e| format!("{e}"))?;
    info!("Extracted {count} file(s) to {}", dest.display());
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
                info!("  OK  {name}");
                ok += 1;
            }
            Err(e) => {
                info!("  FAIL {name}: {e}");
                fail += 1;
            }
        }
    }
    info!("{ok} OK, {fail} failed");
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

/// Read one mask per line from a filter list file (like `-x@listfile`);
/// blank lines are skipped.
fn read_mask_file(path: &str) -> Result<Vec<String>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("read mask list {path}: {e}"))?;
    Ok(content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

/// Parse a WinRAR date (`-ta`/`-tb`) into unix seconds. Accepts
/// `YYYY[MM[DD[HH[MM[SS]]]]]`; missing trailing parts default to their
/// minimum (month/day 01, time 00:00:00).
fn parse_rar_date(s: &str) -> Result<u32, String> {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    let (y, m, d, hh, mm, ss) = match digits.len() {
        4 => (digits.parse::<i64>().unwrap(), 1, 1, 0, 0, 0),
        6 => (
            digits[0..4].parse().unwrap(),
            digits[4..6].parse().unwrap(),
            1,
            0,
            0,
            0,
        ),
        8 => (
            digits[0..4].parse().unwrap(),
            digits[4..6].parse().unwrap(),
            digits[6..8].parse().unwrap(),
            0,
            0,
            0,
        ),
        10 => (
            digits[0..4].parse().unwrap(),
            digits[4..6].parse().unwrap(),
            digits[6..8].parse().unwrap(),
            digits[8..10].parse().unwrap(),
            0,
            0,
        ),
        12 => (
            digits[0..4].parse().unwrap(),
            digits[4..6].parse().unwrap(),
            digits[6..8].parse().unwrap(),
            digits[8..10].parse().unwrap(),
            digits[10..12].parse().unwrap(),
            0,
        ),
        14 => (
            digits[0..4].parse().unwrap(),
            digits[4..6].parse().unwrap(),
            digits[6..8].parse().unwrap(),
            digits[8..10].parse().unwrap(),
            digits[10..12].parse().unwrap(),
            digits[12..14].parse().unwrap(),
        ),
        _ => return Err(format!("invalid date: {s} (use YYYYMMDDHHMMSS)")),
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 59 {
        return Err(format!("invalid date: {s}"));
    }
    let days = days_from_civil(y, m, d);
    let secs = days * 86400 + i64::from(hh) * 3600 + i64::from(mm) * 60 + i64::from(ss);
    u32::try_from(secs).map_err(|_| format!("date out of range: {s}"))
}

/// Which file timestamp a `-tn`/`-to` filter compares (`m`/`c`/`a`
/// modifiers; `m` is the default, `o` is accepted but has no effect since
/// every filter here uses a single time kind).
#[derive(Clone, Copy)]
enum TimeKind {
    Modified,
    Created,
    Accessed,
}

/// Parse a WinRAR `-tn`/`-to` filter: optional leading `m`/`c`/`a`/`o`
/// modifiers followed by a period `[<ndays>d][<nhours>h][<nminutes>m][<nseconds>s]`.
/// Returns the time kind and the period in seconds. Like WinRAR, an empty
/// or unparsable period is treated as 0 seconds.
fn parse_period_filter(s: &str) -> (TimeKind, u64) {
    let mut kind = TimeKind::Modified;
    let mut idx = 0;
    for ch in s.chars() {
        match ch {
            'm' => kind = TimeKind::Modified,
            'c' => kind = TimeKind::Created,
            'a' => kind = TimeKind::Accessed,
            'o' => {} // OR logic: no effect with a single time kind
            _ => break,
        }
        idx += 1;
    }
    (kind, parse_period(&s[idx..]))
}

/// Parse a period string `[<ndays>d][<nhours>h][<nminutes>m][<nseconds>s]`
/// into seconds. Anything that does not parse (including a bare number or
/// an empty string) yields 0, matching WinRAR.
fn parse_period(s: &str) -> u64 {
    let mut secs: u64 = 0;
    let mut num = String::new();
    let mut seen_unit = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
            continue;
        }
        let mult = match ch {
            'd' => 86400,
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return 0,
        };
        seen_unit = true;
        let n: u64 = num.parse().unwrap_or(0);
        secs = secs.saturating_add(n.saturating_mul(mult));
        num.clear();
    }
    if !num.is_empty() || !seen_unit {
        return 0; // trailing digits without a unit, or no unit at all
    }
    secs
}

/// Read one of the three file timestamps as unix nanoseconds. Access time
/// is not exposed by std on Windows, so it falls back to the mtime there.
fn file_time(meta: &std::fs::Metadata, kind: TimeKind) -> u128 {
    let t = match kind {
        TimeKind::Modified => meta.modified(),
        TimeKind::Created => meta.created(),
        TimeKind::Accessed => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                Ok(std::time::UNIX_EPOCH + std::time::Duration::from_secs(meta.atime() as u64))
            }
            #[cfg(not(unix))]
            {
                meta.modified()
            }
        }
    };
    t.ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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
