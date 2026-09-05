//! rar — create, modify, and inspect RAR4, RAR5, and RAR7 archives.

#[path = "../common.rs"]
mod common;
#[path = "../input.rs"]
mod input;
#[path = "../output.rs"]
mod output;
#[path = "../password.rs"]
mod password;
#[path = "../selector.rs"]
mod selector;
#[path = "../time.rs"]
mod time;

use clap::{Args, Parser, Subcommand};
use std::process;

#[derive(Parser)]
#[command(
    name = "rar",
    version,
    about = "create and modify RAR archives",
    long_about = "Pure-Rust RAR4/RAR5/RAR7 archive tool: create, append, update, delete, rename,\nlock, repair and extract archives, with WinRAR-compatible switches.",
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
    /// Misc switches (-ow, -tsp, -ilog, -ver, and compatibility switches)
    #[command(flatten)]
    misc: common::MiscSwitches,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
// clap command enums carry large variant payloads by design.
#[allow(clippy::large_enum_variant)]
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
    /// Create recovery volumes for an existing volume set
    #[command(visible_alias = "rv")]
    RecoveryVolumes(RecoveryVolumesArgs),
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
    /// List bare (names only, like `lb`)
    #[command(visible_alias = "lb")]
    ListBare(ArchiveArgs),
    /// List technical (like `lt`)
    #[command(visible_alias = "lt")]
    ListTechnical(ArchiveArgs),
    /// Verbosely list bare (like `vb`)
    #[command(visible_alias = "vb")]
    VerboseListBare(ArchiveArgs),
    /// Verbosely list technical (like `vt`)
    #[command(visible_alias = "vt")]
    VerboseListTechnical(ArchiveArgs),
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
    password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
}

/// `rar ch` parameters: member name case conversion (-cl / -cu).
#[derive(Args)]
struct ChangeArgs {
    #[command(flatten)]
    password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    /// Convert stored names to lowercase (-cl)
    #[arg(long = "lowercase")]
    lowercase: bool,
    /// Convert stored names to uppercase (-cu)
    #[arg(long = "uppercase")]
    uppercase: bool,
}

/// `rar rv` parameters: the first volume of the set plus an optional
/// recovery-volume count (`rv3`) or percent (`rv10%`); defaults to 10%.
#[derive(Args)]
struct RecoveryVolumesArgs {
    #[command(flatten)]
    password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    /// Recovery volumes: a count (`3`) or percent (`10%`); default 10%
    #[arg(value_name = "COUNT|PCT%", default_value = "10%")]
    count_spec: String,
}

/// Archive path plus an optional member to print.
#[derive(Args)]
struct PrintArgs {
    #[command(flatten)]
    password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(value_name = "FILE")]
    file: Option<String>,
}

/// Comment setting: stdin by default, or `-z<file>`.
#[derive(Args)]
struct CommentArgs {
    #[command(flatten)]
    password: password::PasswordArgs,
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
    password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(long = "dest", default_value = ".", value_name = "DEST")]
    dest: String,
    /// One or more member names to extract; when omitted, every file member
    /// is extracted (or, with `-so`, written to stdout). Member names match
    /// the full stored path or its basename. They are never treated as a
    /// destination directory — set the destination with `--dest` instead.
    #[arg(value_name = "NAMES", trailing_var_arg = true)]
    names: Vec<String>,
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
    /// Extract to stdout instead of writing files (like `-so`); convenient
    /// for piping a member's contents. All file members are concatenated to
    /// stdout (directories are skipped).
    #[arg(long = "stdout")]
    stdout: bool,
}

/// Archive path plus one or more source files.
#[derive(Args)]
struct FilesArgs {
    #[command(flatten)]
    password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(value_name = "FILES", required = true)]
    files: Vec<String>,
    /// Dictionary size for compression (like `-md<size>`)
    #[arg(long = "dict-size", value_name = "SIZE")]
    dict_size: Option<String>,
    /// Archive format version (like `-ma5`; `-ma7` forces RAR7/v70)
    #[arg(long = "archive-format", value_name = "VER", hide = true)]
    archive_format: Option<String>,
    /// Extraction dictionary cap (like `-mdx<size>`; accepted for CLI
    /// compatibility and unused by update/move operations)
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
    password: password::PasswordArgs,
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
    password: password::PasswordArgs,
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
    password: password::PasswordArgs,
    /// Volume size (e.g. 1m, 100k)
    #[arg(short = 'v', long = "volume-size", value_name = "SIZE", value_parser = parse_size)]
    volume_size: Option<u64>,
    /// Solid archive
    #[arg(short = 's', long)]
    solid: bool,
    /// Split the solid chain (WinRAR `-s` modifiers `-sd`/`-sv`/`-se`).
    /// `continuous` (default, like `-sd`) keeps the statistics across the
    /// whole archive; `volume` resets them at each volume boundary (like
    /// `-sv`); `extension` resets when the member's file extension changes
    /// (like `-se`). Implies `-s`.
    #[arg(
        long = "solid-reset",
        value_name = "MODE",
        default_value = "continuous",
        value_parser = ["continuous", "volume", "extension"]
    )]
    solid_reset: String,
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
    /// Archive format version (like `-ma5`; `-ma7` forces RAR7/v70 — an
    /// extension beyond WinRAR 7.23, which has no such switch)
    #[arg(long = "archive-format", value_name = "VER", hide = true)]
    archive_format: Option<String>,
    /// Extraction dictionary cap (like `-mdx<size>`; accepted for CLI
    /// compatibility and unused while creating archives)
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
    /// Store files matching these types without compression (like
    /// `-ms[list]`; semicolon-separated extensions or wildcard masks,
    /// repeatable)
    #[arg(long = "store-types", value_name = "LIST", action = clap::ArgAction::Append)]
    store_types: Vec<String>,
    /// Delete source files after archiving (like `-df`)
    #[arg(long = "delete-after")]
    delete_after: bool,
    /// Test the archive after creating it (like `-t`)
    #[arg(long = "test-after")]
    test_after: bool,
    /// Exclude this path prefix from stored names (like `-ep4<path>`)
    #[arg(long = "exclude-prefix", value_name = "PATH")]
    exclude_prefix: Option<String>,
    /// Synchronize archive contents: delete members not present in the
    /// file list (like `-as`)
    #[arg(long = "sync-archive")]
    sync_archive: bool,
    /// Disable name sorting for solid archives (like `-ds`)
    #[arg(long = "no-sort")]
    no_sort: bool,
    /// Solid archive parameters (like `-s<par>`; accepted, `-s` alone
    /// already enables solid mode)
    #[arg(long = "solid-params", value_name = "PAR")]
    #[allow(dead_code)]
    solid_params: Option<String>,
    /// CRC32 file checksums (like `-htc`; the default)
    #[arg(long = "hash-crc")]
    #[allow(dead_code)]
    hash_crc: bool,
    /// Advanced compression parameters (like `-mc<par>`; accepted)
    #[arg(long = "mc", value_name = "PAR")]
    #[allow(dead_code)]
    mc_params: Option<String>,
    /// Encryption parameters (like `-me<par>`; accepted)
    #[arg(long = "me", value_name = "PAR")]
    #[allow(dead_code)]
    me_params: Option<String>,
    /// Long-distance matching control (like `-mcl`; accepted). Long-range
    /// matching is always enabled for `-m2`…`-m5`, so this is a no-op that
    /// matches WinRAR 7.23's own behaviour.
    #[arg(long = "long-match", value_name = "PAR")]
    #[allow(dead_code)]
    long_match: Option<String>,
    /// Only add files with the Archive attribute set (like `-ao`;
    /// Windows-only, accepted)
    #[arg(long = "archive-attr")]
    #[allow(dead_code)]
    archive_attr: bool,
    /// Set the NTFS Compressed attribute on extracted files (like `-oc`;
    /// accepted)
    #[arg(long = "ntfs-compressed")]
    #[allow(dead_code)]
    ntfs_compressed: bool,
    /// Use large memory pages (like `-mlp`; accepted)
    #[arg(long = "large-pages")]
    #[allow(dead_code)]
    large_pages: bool,
    /// Open shared files (like `-dh`; accepted)
    #[arg(long = "shared-files")]
    #[allow(dead_code)]
    shared_files: bool,
    /// Move deleted files to the Recycle Bin (like `-dr`; unsupported)
    #[arg(long = "recycle-bin")]
    recycle_bin: bool,
    /// Securely wipe files after archiving (like `-dw`; unsupported)
    #[arg(long = "wipe")]
    wipe: bool,
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
    let (num, multiplier) = match s.chars().last() {
        Some('k' | 'K') => (&s[..s.len() - 1], 1024),
        Some('m' | 'M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('g' | 'G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .map_err(|_| format!("invalid size: {s}"))?
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size is too large: {s}"))
}

/// Resolve a `-ma<ver>` archive-format request into the format version and
/// the v70 forcing options. `-ma4` selects the legacy RAR3/4 container
/// (`Rar40`); `-ma5` is the default RAR5 format (a no-op, like WinRAR's
/// accepted-but-inert `-ma5`); `-ma7` forces RAR7 (v70) members with the
/// `-md` dictionary (default 32 MiB) declared in the header — an
/// extension beyond WinRAR 7.23, which only writes v70 above 4 GiB. Any
/// other version is rejected like WinRAR rejects unknown options.
/// Returns `(format_version, force_v70, dict_bytes)`.
fn archive_format_force_v70(
    ma: Option<&str>,
    dict_size_log: Option<u8>,
    dict_size_bytes: Option<u64>,
) -> Result<(rar_rs::ArchiveVersion, bool, Option<u64>), String> {
    match ma {
        Some("4") => Ok((rar_rs::ArchiveVersion::Rar40, false, None)),
        None | Some("5") => Ok((rar_rs::ArchiveVersion::Rar50, false, dict_size_bytes)),
        Some("7") => {
            let bytes = dict_size_bytes
                .or_else(|| dict_size_log.map(|l| (128u64 * 1024) << l))
                .unwrap_or(32 * 1024 * 1024);
            Ok((rar_rs::ArchiveVersion::Rar50, true, Some(bytes)))
        }
        Some(other) => Err(format!("Unknown option: ma{other}")),
    }
}

/// Whether an archive member name matches one `-ms<list>` entry: a bare
/// extension (`bin` matches `*.bin`) or a wildcard mask (`*.bin`, `a?c`)
/// matched against the basename.
/// Expand source arguments with the same name policy used by create, while
/// ensuring a directory argument never feeds the destination archive back
/// into the operation.
fn collect_inputs(
    policy: &rar_rs::name_policy::NamePolicy,
    files: &[String],
    level: u8,
    archive_path: &str,
) -> Result<Vec<rar_rs::name_policy::Collected>, String> {
    let mut collected =
        rar_rs::name_policy::collect(policy, files, level).map_err(|e| format!("collect: {e}"))?;
    if let Ok(abs_archive) = std::fs::canonicalize(archive_path) {
        collected.retain(
            |item| !matches!(std::fs::canonicalize(&item.path), Ok(path) if path == abs_archive),
        );
    }
    Ok(collected)
}

fn store_type_matches(mask: &str, name: &str) -> bool {
    if mask.contains('*') || mask.contains('?') {
        let base = name.rsplit('/').next().unwrap_or(name);
        return wildcard_match(mask, base);
    }
    name.ends_with(&format!(".{mask}"))
}

/// Simple `*`/`?` wildcard match (case-insensitive, like WinRAR's masks).
fn wildcard_match(mask: &str, name: &str) -> bool {
    fn inner(m: &[u8], n: &[u8]) -> bool {
        match (m.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => inner(&m[1..], n) || (!n.is_empty() && inner(m, &n[1..])),
            (Some(&c), Some(&nc)) if c == b'?' || c.eq_ignore_ascii_case(&nc) => {
                inner(&m[1..], &n[1..])
            }
            _ => false,
        }
    }
    inner(mask.as_bytes(), name.as_bytes())
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    // Configuration sources (priority: command line > RARINISWITCHES >
    // rar.ini); `-cfg-` disables the file and the environment variable.
    let no_config = raw.iter().skip(1).any(|a| a == "-cfg-");
    let command = common::command_name(&raw);
    let defaults: Vec<String> = common::default_switches(command.as_deref(), no_config)
        .iter()
        .map(|a| common::normalize_switch(a))
        .collect();
    let cli_args: Vec<String> = raw
        .iter()
        .skip(1)
        .map(|a| common::normalize_switch(a))
        .collect();
    let args = common::merge_default_switches(defaults, cli_args);
    if let Err(e) = password::reject_bare_password(&args) {
        eprintln!("rar: {e}");
        process::exit(1);
    }
    // `rar -iver` prints the version and exits (no subcommand needed).
    if args.iter().any(|a| a == "--version-info") {
        println!("RAR 7.23 CLI parity (rar-rs {})", env!("CARGO_PKG_VERSION"));
        return;
    }
    let cli = Cli::parse_from(std::iter::once("rar".to_string()).chain(args));
    output::QUIET.store(cli.quiet, std::sync::atomic::Ordering::Relaxed);
    output::ERR.store(cli.err, std::sync::atomic::Ordering::Relaxed);
    if let Some(dir) = &cli.work_dir
        && let Err(e) = std::env::set_current_dir(dir)
    {
        eprintln!("rar: cannot change to work directory {dir}: {e}");
        process::exit(1);
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
    if misc.erase_disk {
        return Err("-vd/--erase-disk is not supported; no disk was erased".into());
    }
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
        Command::RecoveryVolumes(args) => cmd_recovery_volumes(&args),
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
        Command::ListBare(args) => cmd_list_bare(&args),
        Command::ListTechnical(args) => cmd_list_technical(&args),
        Command::VerboseListBare(args) => cmd_list_bare(&args),
        Command::VerboseListTechnical(args) => cmd_list_technical(&args),
        Command::Info(args) => cmd_info(&args),
        Command::External(ext) => {
            let name = ext.first().cloned().unwrap_or_default();
            // `i<string>` (and `ic`/`ih` variants) find strings in members.
            if name.len() > 1 && name.starts_with('i') {
                cmd_find(&name, &ext[1..])
            // WinRAR's canonical `rv[N]` embeds the count in the command
            // token (`rar rv3 data.part01.rar`); route those here.
            } else if name.len() > 2
                && name.starts_with("rv")
                && name[2..].chars().all(|c| c.is_ascii_digit() || c == '%')
            {
                let spec = name[2..].to_string();
                cmd_recovery_volumes(&RecoveryVolumesArgs {
                    password: password::PasswordArgs { password: None },
                    archive: ext.get(1).cloned().unwrap_or_default(),
                    count_spec: if spec.is_empty() { "10%".into() } else { spec },
                })
            } else {
                Err(format!("unknown command: {name}"))
            }
        }
    }
}

fn cmd_create(args: &CreateArgs, misc: &common::MiscSwitches) -> Result<(), String> {
    if let Some(threads) = args.threads {
        rar_rs::set_compression_threads(threads);
        rar_rs::set_extraction_threads(threads);
    }
    if args.wipe {
        return Err("-dw/--wipe is not supported; no source files were deleted".into());
    }
    if args.recycle_bin {
        return Err("-dr/--recycle-bin is not supported; no source files were deleted".into());
    }
    let mut password = args.password.password.clone();
    // `-p-` normalizes to an empty value and explicitly disables password
    // use. Bare `-p` was rejected before clap parsing.
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
        (true, false) => Some(rar_rs::name_policy::CaseKind::Lower),
        (false, true) => Some(rar_rs::name_policy::CaseKind::Upper),
        _ => None,
    };
    let header_encrypt = args.header_encrypt.is_some();
    if let Some(pw) = &args.header_encrypt
        && !pw.is_empty()
    {
        password = Some(pw.clone());
    }
    let ts = time::parse_ts_specs(&args.ts_specs)?;

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
        let (y, mo, d) = time::civil_from_days(days);
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

    let (dict_size_log, dict_size_bytes) = match args.dict_size.as_deref() {
        Some(s) => rar_rs::parse_dict_size(s).ok_or_else(|| format!("Unknown option: md{s}"))?,
        None => (None, None),
    };
    let (format_version, force_v70, v70_dict_bytes) = archive_format_force_v70(
        args.archive_format.as_deref(),
        dict_size_log,
        dict_size_bytes,
    )?;
    // The typed API has no `force_v70` test seam: `-ma7` selects the Rar70
    // format (every member v70, 32 MiB default dictionary), while
    // `-ma5`/default with a > 4 GiB `-md` keeps the legacy auto mode (v70
    // members only when a member's effective dictionary exceeds 4 GiB —
    // exactly the bytes the writer produced before).
    let typed_format = if force_v70 {
        rar_rs::ArchiveVersion::Rar70
    } else {
        format_version
    };
    // The `-md` dictionary in bytes. RAR4 never receives one: its writer
    // picks the per-member window internally, and the legacy CLI silently
    // ignored `-md` there.
    let dictionary = if typed_format == rar_rs::ArchiveVersion::Rar40 {
        None
    } else {
        v70_dict_bytes
            .or(dict_size_bytes)
            .or_else(|| dict_size_log.map(|log| (128u64 * 1024) << log))
            .map(|bytes| {
                rar_rs::DictionarySize::try_from(bytes).map_err(|e| format!("dictionary: {e}"))
            })
            .transpose()?
    };
    // `--solid-reset` implies solid mode unless `-s-`-style off (the CLI
    // has no explicit off switch, matching WinRAR: `-sd`/`-sv`/`-se` all
    // enable solid creation).
    let solid_mode = match (
        args.solid_reset.as_str(),
        args.solid || args.solid_params.is_some() || args.solid_reset != "continuous",
    ) {
        (_, false) => rar_rs::SolidMode::Disabled,
        ("volume", _) => rar_rs::SolidMode::PerVolume,
        ("extension", _) => rar_rs::SolidMode::PerExtension,
        _ => rar_rs::SolidMode::Continuous,
    };
    let opts = rar_rs::WriterOptions::new()
        .format_version(typed_format)
        .solid_mode(solid_mode)
        .quick_open(args.quick_open)
        .blake2(args.blake2)
        .encrypt_headers(header_encrypt);
    let opts = if let Some(pw) = &password {
        opts.password(pw.clone())
    } else {
        opts
    };
    let opts = if let Some(percent) = args.recovery_percent {
        opts.recovery_percent(percent)
    } else {
        opts
    };
    let opts = if let Some(percent) = recovery_volumes_percent {
        opts.recovery_volumes_percent(percent)
    } else {
        opts
    };
    let opts = if let Some(count) = recovery_volume_count {
        opts.recovery_volume_count(count)
    } else {
        opts
    };
    let opts = if let Some(size) = args.volume_size {
        opts.volume_size(size)
    } else {
        opts
    };
    let opts = if let Some(size) = dictionary {
        opts.dictionary_size(size)
    } else {
        opts
    };
    let opts = if let Some(threads) = args.threads {
        opts.thread_count(
            rar_rs::ThreadCount::try_from(threads).map_err(|e| format!("threads: {e}"))?,
        )
    } else {
        opts
    };
    let opts = opts
        .save_ctime(ts.save_ctime)
        .save_atime(ts.save_atime)
        .save_mtime(ts.save_mtime)
        .save_owner(misc.owner)
        .save_streams(misc.save_streams)
        .time_precision_seconds(ts.precision_seconds);

    let existing = std::path::Path::new(archive_path).exists();
    // -tk: keep the archive's original modification time on update.
    let orig_mtime = if args.keep_time && existing {
        std::fs::metadata(archive_path)
            .and_then(|m| m.modified())
            .ok()
    } else {
        None
    };
    // The writer is opened lazily: for an existing archive the
    // same-named members are replaced (deleted) first, so the append
    // handle is only opened after that rewrite; a new archive is created
    // immediately.
    let created: Option<rar_rs::ArchiveWriter> = if existing {
        None
    } else {
        Some(
            rar_rs::ArchiveWriter::create_with(archive_path, opts.clone())
                .map_err(|e| format!("create: {e}"))?,
        )
    };

    let mut include_masks = args.include_masks.clone();
    let mut exclude_masks = args.exclude_masks.clone();
    for file in &args.include_list_files {
        include_masks.extend(read_mask_file(file)?);
    }
    for file in &args.exclude_list_files {
        exclude_masks.extend(read_mask_file(file)?);
    }
    let policy = rar_rs::name_policy::NamePolicy {
        path_prefix: args.path_prefix.clone(),
        exclude_prefix: args.exclude_prefix.clone(),
        basename_only: args.basename_only,
        strip_base: args.strip_base,
        full_paths: args.full_paths,
        full_paths_drive: args.full_paths_drive,
        no_recurse: args.no_recurse,
        wildcard_top_only: args.recurse_zero,
        case: case.map(|c| match c {
            rar_rs::name_policy::CaseKind::Lower => rar_rs::name_policy::CaseKind::Lower,
            rar_rs::name_policy::CaseKind::Upper => rar_rs::name_policy::CaseKind::Upper,
        }),
        include_masks,
        exclude_masks,
    };
    let mut collected = collect_inputs(&policy, files, args.level, archive_path)?;
    // -ms<list>: files matching one of the listed types (extensions or
    // wildcard masks, semicolon-separated, repeatable) are stored without
    // compression (level 0), like WinRAR.
    if !args.store_types.is_empty() {
        let masks: Vec<String> = args
            .store_types
            .iter()
            .flat_map(|l| l.split(';'))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        for c in collected.iter_mut() {
            if !c.is_dir && masks.iter().any(|m| store_type_matches(m, &c.name)) {
                c.level = 0;
            }
        }
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
        let mut seen: std::collections::HashMap<(u64, u64), String> =
            std::collections::HashMap::new();
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
        drop(created);
        if !existing {
            let _ = std::fs::remove_file(archive_path);
        }
        info!("WARNING: No files");
        process::exit(10);
    }
    // Directory entries always come after the files, like WinRAR.
    let (file_entries, dir_entries): (Vec<_>, Vec<_>) =
        collected.into_iter().partition(|c| !c.is_dir);
    let mut collected: Vec<_> = file_entries;
    // `-se` (reset the solid chain on a file-extension change) is handled
    // per-member inside the writer via `maybe_reset_solid_for_extension`, so
    // WinRAR's input order is preserved (we do NOT reorder by extension here
    // — WinRAR keeps order and resets only when the extension changes).
    // rarfiles.lst: user-defined add order for solid archives (mask list
    // with optional `$default`); matched files are grouped by the
    // highest-priority mask, where a mask whose matches are a subset of
    // another mask's wins regardless of position (WinRAR semantics).
    // `-ds` disables the sorting (like WinRAR).
    if (args.solid || args.solid_params.is_some()) && !args.no_sort {
        let masks = input::read_rarfiles_lst();
        if !masks.is_empty() {
            apply_rarfiles_order(&mut collected, &masks);
        }
    }
    collected.extend(dir_entries);
    // -tsp: snapshot source access times before reading the files.
    #[cfg(unix)]
    let ts_preserve_atimes: Vec<(std::path::PathBuf, std::time::SystemTime)> = if misc.ts_preserve {
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
    let mut write_entries: Vec<rar_rs::WriteEntry<'_>> = Vec::with_capacity(collected.len());
    for c in &collected {
        let options = rar_rs::EntryWriteOptions::new().compression_level(
            rar_rs::CompressionLevel::try_from(c.level).map_err(|e| format!("level: {e}"))?,
        );
        if c.is_dir {
            write_entries.push(rar_rs::WriteEntry::Directory {
                path: &c.path,
                name: Some(&c.name),
            });
        } else {
            write_entries.push(rar_rs::WriteEntry::File {
                path: &c.path,
                name: Some(&c.name),
                options,
            });
        }
    }
    // WinRAR `rar a` semantics on an existing archive: members with the
    // same name as an incoming file are replaced — deleted first through
    // the editor role, then re-added through the typed append below; every
    // other member is preserved verbatim. The append handle is opened only
    // after the rewrite.
    let mut writer = if existing {
        use std::collections::HashSet;
        if args.volume_size.is_some() {
            return Err("appending to multi-volume archives is not supported".into());
        }
        let incoming: HashSet<String> = collected
            .iter()
            .map(|c| c.name.clone())
            .chain(args.stdin_name.iter().cloned())
            .collect();
        let mut editor = open_editor(archive_path, password.as_deref())?;
        let to_drop: Vec<String> = editor
            .entries()
            .map(|entry| entry.name().to_string())
            .filter(|n| incoming.contains(n))
            .collect();
        if !to_drop.is_empty() {
            let refs: Vec<&str> = to_drop.iter().map(|s| s.as_str()).collect();
            let plan = editor_delete_plan(&editor, &refs).map_err(|e| format!("replace: {e}"))?;
            editor.apply(plan).map_err(|e| format!("replace: {e}"))?;
        }
        // Deleting every member erases the archive file (like `rar d`);
        // when the replacement removed the only members, recreate it
        // instead of appending to a file that no longer exists.
        if std::path::Path::new(archive_path).exists() {
            let mut append_opts = rar_rs::AppendOptions::new();
            if let Some(pw) = &password {
                append_opts = append_opts.password(pw.clone());
            }
            if let Some(size) = dictionary {
                append_opts = append_opts.dictionary_size(size);
            }
            rar_rs::ArchiveWriter::append_with(archive_path, append_opts)
                .map_err(|e| format!("open: {e}"))?
        } else {
            rar_rs::ArchiveWriter::create_with(archive_path, opts.clone())
                .map_err(|e| format!("create: {e}"))?
        }
    } else {
        created.expect("new archive opened above")
    };
    writer
        .add_batch(&write_entries)
        .map_err(|e| format!("add: {e}"))?;
    // Link redirects are recorded after their data members (the reference
    // target name is what matters, not the order).
    for (name, redir_type, target) in &redirects {
        writer
            .add_redirect(name, *redir_type, target)
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
        let stdin_options = rar_rs::EntryWriteOptions::new().compression_level(
            rar_rs::CompressionLevel::try_from(args.level).map_err(|e| format!("level: {e}"))?,
        );
        writer
            .add_bytes(&name, &data, stdin_options)
            .map_err(|e| format!("add stdin: {e}"))?;
    }

    let was_existing = existing;
    let write_report = writer.finish().map_err(|e| format!("close: {e}"))?;
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
    // -df: delete the source files after archiving (the archive keeps
    // them; directories are left in place, like WinRAR).
    if args.delete_after {
        for c in &collected {
            if !c.is_dir {
                let _ = std::fs::remove_file(&c.path);
            }
        }
    }
    // -t: test the archive right after creating it without materializing
    // member contents or extracting to a temporary directory.
    if args.test_after {
        let mut ar =
            open_reader(archive_path, password.as_deref()).map_err(|e| format!("open: {e}"))?;
        let report: rar_rs::VerificationReport =
            ar.verify().map_err(|e| format!("test failed: {e}"))?;
        if report.failed() != 0 {
            return Err(format!("test failed: {} member(s) failed", report.failed()));
        }
    }
    // -as: synchronize the archive contents — drop members that are not
    // part of the file list (only meaningful when appending to an
    // existing archive; a freshly created archive only holds the list).
    if args.sync_archive && was_existing {
        use std::collections::HashSet;
        let keep: HashSet<String> = collected
            .iter()
            .map(|c| c.name.clone())
            .chain(args.stdin_name.iter().cloned())
            .collect();
        let mut editor = open_editor(archive_path, password.as_deref())?;
        let stale: Vec<String> = editor
            .entries()
            .map(|entry| entry.name().to_string())
            .filter(|n| !keep.contains(n))
            .collect();
        if !stale.is_empty() {
            let refs: Vec<&str> = stale.iter().map(|s| s.as_str()).collect();
            let plan = editor_delete_plan(&editor, &refs).map_err(|e| format!("sync: {e}"))?;
            editor.apply(plan).map_err(|e| format!("sync: {e}"))?;
        }
    }
    if args.volume_size.is_some() {
        info!(
            "Created {} volume(s) ({} file(s), level {})",
            write_report.volume_paths().len(),
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
/// Open an [`ArchiveEditor`] for the CLI, honoring the password switch.
fn open_editor(
    path: impl AsRef<std::path::Path>,
    password: Option<&str>,
) -> Result<rar_rs::ArchiveEditor, String> {
    match password {
        Some(pw) if !pw.is_empty() => {
            rar_rs::ArchiveEditor::open_with_password(path, pw).map_err(|e| format!("open: {e}"))
        }
        _ => rar_rs::ArchiveEditor::open(path).map_err(|e| format!("open: {e}")),
    }
}

/// Resolve delete names onto an [`rar_rs::EditPlan`] with the legacy `rar d`
/// semantics: every name deletes the first matching member that is not
/// already selected, so repeated names delete successive duplicates, and a
/// missing name fails the whole plan before any rewrite starts.
fn editor_delete_plan(
    editor: &rar_rs::ArchiveEditor,
    names: &[&str],
) -> Result<rar_rs::EditPlan, rar_rs::RarError> {
    let mut plan = rar_rs::EditPlan::new();
    let mut chosen: Vec<rar_rs::EntryId> = Vec::new();
    for name in names {
        let id = editor
            .entries_named(name)
            .map(|entry| entry.id())
            .find(|id| !chosen.contains(id))
            .ok_or_else(|| rar_rs::RarError::MemberNotFound {
                name: (*name).to_string(),
            })?;
        chosen.push(id);
        plan = plan.delete(id);
    }
    Ok(plan)
}

/// Resolve rename pairs onto an [`rar_rs::EditPlan`]: each old name targets
/// the first member with that stored name (trailing `/` ignored) that is
/// not already renamed in the plan; directory expansion to descendants is
/// handled by the rewrite core.
fn editor_rename_plan(
    editor: &rar_rs::ArchiveEditor,
    pairs: &[(&str, &str)],
) -> Result<rar_rs::EditPlan, rar_rs::RarError> {
    let mut plan = rar_rs::EditPlan::new();
    let mut chosen: Vec<rar_rs::EntryId> = Vec::new();
    for (old, new) in pairs {
        let old_norm = old.trim_end_matches('/');
        let id = editor
            .entries()
            .find(|entry| {
                entry.name().trim_end_matches('/') == old_norm && !chosen.contains(&entry.id())
            })
            .map(|entry| entry.id())
            .ok_or_else(|| rar_rs::RarError::MemberNotFound {
                name: (*old).to_string(),
            })?;
        chosen.push(id);
        plan = plan.rename(id, (*new).to_string());
    }
    Ok(plan)
}

/// Resolve chained rename pairs onto an [`rar_rs::EditPlan`] mirroring the
/// legacy name-based `rename` resolution exactly: each old name targets the
/// first member whose stored name — or already-planned rename in this call —
/// equals it, so version chains (`a.txt -> a.txt;1 -> a.txt;2`) and repeated
/// old names resolve like the legacy sequential rewrite while staying
/// index-addressed.
fn editor_chained_rename_plan(
    editor: &rar_rs::ArchiveEditor,
    pairs: &[(&str, &str)],
) -> Result<rar_rs::EditPlan, rar_rs::RarError> {
    let mut plan = rar_rs::EditPlan::new();
    let mut planned: std::collections::HashMap<rar_rs::EntryId, String> =
        std::collections::HashMap::new();
    for (old, new) in pairs {
        let old_norm = old.trim_end_matches('/');
        let id = editor
            .entries()
            .find(|entry| {
                planned
                    .get(&entry.id())
                    .map(String::as_str)
                    .unwrap_or_else(|| entry.name())
                    .trim_end_matches('/')
                    == old_norm
            })
            .map(|entry| entry.id())
            .ok_or_else(|| rar_rs::RarError::MemberNotFound {
                name: (*old).to_string(),
            })?;
        planned.insert(id, (*new).to_string());
        plan = plan.rename(id, (*new).to_string());
    }
    Ok(plan)
}

fn cmd_delete(args: &DeleteArgs) -> Result<(), String> {
    let archive_path = &args.archive;
    let names: Vec<&str> = args.names.iter().map(|s| s.as_str()).collect();
    let mut editor = open_editor(archive_path, args.password.password.as_deref())?;
    let plan = editor_delete_plan(&editor, &names).map_err(|e| format!("delete: {e}"))?;
    let deleted = editor
        .apply(plan)
        .map_err(|e| format!("delete: {e}"))?
        .deleted();
    info!("Deleted {deleted} file(s) from {archive_path}");
    Ok(())
}

/// A same-directory archive copy that is removed unless successfully
/// installed over the original archive.
struct StagedArchive {
    path: std::path::PathBuf,
    committed: bool,
}

impl StagedArchive {
    fn copy_from(original: &std::path::Path) -> Result<Self, String> {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let parent = original
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let file_name = original
            .file_name()
            .ok_or_else(|| format!("invalid archive path: {}", original.display()))?
            .to_string_lossy();
        for _ in 0..100 {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".{file_name}.rar-rs-update-{}-{id}.tmp",
                std::process::id()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    drop(file);
                    if let Err(error) = std::fs::copy(original, &path) {
                        let _ = std::fs::remove_file(&path);
                        return Err(format!("stage archive copy: {error}"));
                    }
                    return Ok(Self {
                        path,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("create staged archive: {error}")),
            }
        }
        Err("could not allocate a unique staged archive path".into())
    }

    fn commit(mut self, original: &std::path::Path) -> Result<(), String> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("sync staged archive: {error}"))?;
        replace_archive_file(&self.path, original)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagedArchive {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn replace_archive_file(
    staged: &std::path::Path,
    original: &std::path::Path,
) -> Result<(), String> {
    std::fs::rename(staged, original).map_err(|error| format!("replace archive: {error}"))
}

#[cfg(windows)]
fn replace_archive_file(
    staged: &std::path::Path,
    original: &std::path::Path,
) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    let original: Vec<u16> = original.as_os_str().encode_wide().chain(Some(0)).collect();
    let staged: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
    let replaced = unsafe {
        windows_sys::Win32::Storage::FileSystem::ReplaceFileW(
            original.as_ptr(),
            staged.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        Err(format!(
            "replace archive: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn replace_archive_file(
    _staged: &std::path::Path,
    _original: &std::path::Path,
) -> Result<(), String> {
    Err("transactional archive replacement is not supported on this platform".into())
}

fn update_archive_transactionally(
    archive: &std::path::Path,
    operation: impl FnOnce(&std::path::Path) -> Result<(), String>,
) -> Result<(), String> {
    let staged = StagedArchive::copy_from(archive)?;
    operation(&staged.path)?;
    staged.commit(archive)
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

/// Shared transactional update/freshen implementation. Source arguments are
/// expanded with the create command's collector. Every mutation is applied to
/// a same-directory copy and the original is atomically replaced only after
/// the complete delete/rename/append/close sequence succeeds.
fn cmd_update_freshen(
    args: &FilesArgs,
    freshen: bool,
    verb: &str,
    misc: &common::MiscSwitches,
) -> Result<(), String> {
    let archive_path = std::path::Path::new(&args.archive);
    let password = &args.password.password;
    if !archive_path.exists() {
        return Err(format!("archive not found: {}", archive_path.display()));
    }
    if rar_rs::discover_volumes(archive_path).len() > 1 {
        return Err("transactional update of multi-volume archives is not supported".into());
    }

    // Validate all operation options before allocating the staged copy.
    let (dict_size_log, dict_size_bytes) = match args.dict_size.as_deref() {
        Some(spec) => {
            rar_rs::parse_dict_size(spec).ok_or_else(|| format!("Unknown option: md{spec}"))?
        }
        None => (None, None),
    };
    let (format_version, force_v70, v70_dict_bytes) = archive_format_force_v70(
        args.archive_format.as_deref(),
        dict_size_log,
        dict_size_bytes,
    )?;
    // The typed API has no `force_v70` seam: `-ma7` selects `Rar70` (every
    // member v70); `-ma5`/default keep the legacy auto v50/v70 semantics
    // for > 4 GiB `-md` requests. RAR4 never takes a dictionary.
    let typed_format = if force_v70 {
        rar_rs::ArchiveVersion::Rar70
    } else {
        format_version
    };
    let dictionary = if typed_format == rar_rs::ArchiveVersion::Rar40 {
        None
    } else {
        v70_dict_bytes
            .or(dict_size_bytes)
            .or_else(|| dict_size_log.map(|log| (128u64 * 1024) << log))
            .map(|bytes| {
                rar_rs::DictionarySize::try_from(bytes)
                    .map_err(|error| format!("dictionary: {error}"))
            })
            .transpose()?
    };
    let ts = time::parse_ts_specs(&args.ts_specs)?;
    let original_mtime = if args.keep_time {
        Some(
            std::fs::metadata(archive_path)
                .and_then(|metadata| metadata.modified())
                .map_err(|error| format!("read archive modification time: {error}"))?,
        )
    } else {
        None
    };

    let collected = collect_inputs(
        &rar_rs::name_policy::NamePolicy::default(),
        &args.files,
        3,
        &args.archive,
    )?;
    let archive =
        open_reader(archive_path, password.as_deref()).map_err(|error| format!("open: {error}"))?;
    let mut to_delete = Vec::new();
    let mut to_add = Vec::new();
    for item in &collected {
        let source_mtime = std::fs::metadata(&item.path)
            .and_then(|metadata| metadata.modified())
            .map_err(|error| format!("read source metadata {}: {error}", item.path.display()))?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let source_mtime = u32::try_from(source_mtime).unwrap_or(u32::MAX);
        if let Some(entry) = archive.entries_named(&item.name).next() {
            if source_mtime > entry.mtime() {
                to_delete.push(item.name.clone());
                to_add.push(item);
            }
        } else if !freshen {
            to_add.push(item);
        }
    }
    drop(archive);

    if to_delete.is_empty() && to_add.is_empty() {
        info!("{}: no files to {verb}", archive_path.display());
        return Ok(());
    }

    let updated_count = to_add.len();
    update_archive_transactionally(archive_path, |staged_path| {
        if !to_delete.is_empty() {
            if let Some(version_spec) = &misc.version_control {
                // Version control chains renames inside one call with
                // map-aware resolution (a.txt -> a.txt;1 -> a.txt;2), then
                // drops versions above the cap. Both run through the editor
                // role in two atomic rewrites — renames re-emit only the
                // headers (never recompressing), while the drop may
                // recompress the solid chain, so they stay split exactly
                // like the legacy sequential calls.
                let mut editor = open_editor(staged_path, password.as_deref())
                    .map_err(|error| format!("open staged archive: {error}"))?;
                let max_versions = if version_spec.is_empty() {
                    None
                } else {
                    version_spec.parse::<u32>().ok().filter(|count| *count > 0)
                };
                let mut renames = Vec::new();
                let mut to_drop = Vec::new();
                for name in &to_delete {
                    let mut versions: Vec<(u32, String)> = editor
                        .entries()
                        .filter_map(|entry| {
                            let member = entry.name();
                            if member == *name {
                                Some((0, member.to_string()))
                            } else if let Some(suffix) = member.strip_prefix(&format!("{name};"))
                                && let Ok(version) = suffix.parse::<u32>()
                            {
                                Some((version, member.to_string()))
                            } else {
                                None
                            }
                        })
                        .collect();
                    versions.sort_by_key(|(version, _)| *version);
                    for (version, member) in versions.iter().rev() {
                        let new_suffix = version
                            .checked_add(1)
                            .ok_or_else(|| format!("version number overflow for {member}"))?;
                        if max_versions.is_some_and(|limit| new_suffix > limit) {
                            to_drop.push(member.clone());
                        } else {
                            let new_name = if *version == 0 {
                                format!("{name};1")
                            } else {
                                format!("{name};{new_suffix}")
                            };
                            renames.push((member.clone(), new_name));
                        }
                    }
                }
                if !renames.is_empty() {
                    let pairs: Vec<(&str, &str)> = renames
                        .iter()
                        .map(|(old, new)| (old.as_str(), new.as_str()))
                        .collect();
                    let plan = editor_chained_rename_plan(&editor, &pairs)
                        .map_err(|error| format!("rename staged members: {error}"))?;
                    editor
                        .apply(plan)
                        .map_err(|error| format!("rename staged members: {error}"))?;
                }
                if !to_drop.is_empty() {
                    let names: Vec<&str> = to_drop.iter().map(String::as_str).collect();
                    let plan = editor_delete_plan(&editor, &names)
                        .map_err(|error| format!("delete staged versions: {error}"))?;
                    editor
                        .apply(plan)
                        .map_err(|error| format!("delete staged versions: {error}"))?;
                }
            } else {
                // Plain replacement delete (no version control) runs through
                // the editor role in one atomic rewrite.
                let mut editor = open_editor(staged_path, password.as_deref())
                    .map_err(|error| format!("open staged archive: {error}"))?;
                let names: Vec<&str> = to_delete.iter().map(String::as_str).collect();
                let plan = editor_delete_plan(&editor, &names)
                    .map_err(|error| format!("delete staged members: {error}"))?;
                editor
                    .apply(plan)
                    .map_err(|error| format!("delete staged members: {error}"))?;
            }
        }

        let mut staged = if staged_path.exists() {
            let mut append_opts = rar_rs::AppendOptions::new();
            if let Some(value) = password {
                append_opts = append_opts.password(value.clone());
            }
            if let Some(size) = dictionary {
                append_opts = append_opts.dictionary_size(size);
            }
            rar_rs::ArchiveWriter::append_with(staged_path, append_opts)
                .map_err(|error| format!("open staged archive for append: {error}"))?
        } else {
            let mut writer_opts = rar_rs::WriterOptions::new()
                .format_version(typed_format)
                .save_ctime(ts.save_ctime)
                .save_atime(ts.save_atime)
                .save_mtime(ts.save_mtime)
                .save_owner(misc.owner)
                .save_streams(misc.save_streams)
                .time_precision_seconds(ts.precision_seconds);
            if let Some(value) = password {
                writer_opts = writer_opts.password(value.clone());
            }
            if let Some(size) = dictionary {
                writer_opts = writer_opts.dictionary_size(size);
            }
            rar_rs::ArchiveWriter::create_with(staged_path, writer_opts)
                .map_err(|error| format!("recreate staged archive: {error}"))?
        };
        let mut write_entries: Vec<rar_rs::WriteEntry<'_>> = Vec::with_capacity(to_add.len());
        for item in &to_add {
            let options = rar_rs::EntryWriteOptions::new().compression_level(
                rar_rs::CompressionLevel::try_from(item.level)
                    .map_err(|error| format!("level: {error}"))?,
            );
            if item.is_dir {
                write_entries.push(rar_rs::WriteEntry::Directory {
                    path: &item.path,
                    name: Some(&item.name),
                });
            } else {
                write_entries.push(rar_rs::WriteEntry::File {
                    path: &item.path,
                    name: Some(&item.name),
                    options,
                });
            }
        }
        staged
            .add_batch(&write_entries)
            .map_err(|error| format!("append staged members: {error}"))?;
        staged
            .finish()
            .map_err(|error| format!("close staged archive: {error}"))?;

        if let Some(modified) = original_mtime {
            std::fs::File::options()
                .write(true)
                .open(staged_path)
                .and_then(|file| file.set_times(std::fs::FileTimes::new().set_modified(modified)))
                .map_err(|error| format!("restore staged archive time: {error}"))?;
        }
        Ok(())
    })?;

    info!(
        "{verb} {} ({updated_count} file(s))",
        archive_path.display()
    );
    Ok(())
}

/// Lock the archive (like `rar k`).
fn cmd_lock(args: &ArchiveArgs) -> Result<(), String> {
    let mut editor = open_editor(&args.archive, args.password.password.as_deref())
        .map_err(|e| format!("open: {e}"))?;
    editor.lock().map_err(|e| format!("lock: {e}"))?;
    info!("Locked {archive}", archive = args.archive);
    Ok(())
}

/// Add an inline recovery record (like `rar rr`).
fn cmd_rr(args: &RecoveryArgs) -> Result<(), String> {
    let mut editor = open_editor(&args.archive, args.password.password.as_deref())?;
    editor
        .apply(rar_rs::EditPlan::new().set_recovery(args.percent))
        .map_err(|e| format!("rr: {e}"))?;
    info!(
        "Recovery record {}% added to {archive}",
        args.percent,
        archive = args.archive
    );
    Ok(())
}

/// Create recovery volumes for an existing multi-volume set (like
/// `rar rv[N]`): `N` is the number of `.rev` files, or `N%` the percent
/// of data volumes (default 10%). The count is capped at 10x the data
/// volume count and the `.rev` files are named with the set's padding,
/// matching WinRAR. Only the raw volume bytes are read, so encrypted
/// sets need no password.
fn cmd_recovery_volumes(args: &RecoveryVolumesArgs) -> Result<(), String> {
    let first = std::path::Path::new(&args.archive);
    let volumes = rar_rs::discover_volumes(first);
    let nd = volumes.len();
    if nd <= 1 {
        return Err(format!(
            "rv: {} is not part of a multi-volume set",
            first.display()
        ));
    }
    let spec = args.count_spec.trim();
    let rec_count = if let Some(pct) = spec.strip_suffix('%') {
        let pct: u64 = pct
            .parse()
            .map_err(|_| format!("invalid recovery percent: {spec}"))?;
        if pct > 1000 {
            return Err(format!("invalid recovery percent: {spec}"));
        }
        rar_rs::recovery::rev50::plan_recovery_volume_count(nd, pct)
            .map_err(|e| format!("rv: {e}"))?
    } else {
        spec.parse::<usize>()
            .map_err(|_| format!("invalid recovery volume count: {spec}"))?
    };

    let written = rar_rs::recovery::rev50::build_recovery_volumes_for_set(&volumes, rec_count)
        .map_err(|e| format!("rv: {e}"))?;
    for path in &written {
        info!("Creating {}", path.display());
    }
    info!("{} recovery volume(s) created", written.len());
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
    let mut editor = open_editor(archive_path, None)?;
    let plan = editor_rename_plan(&editor, &pairs).map_err(|e| format!("rename: {e}"))?;
    let renamed = editor
        .apply(plan)
        .map_err(|e| format!("rename: {e}"))?
        .renamed();
    info!("Renamed {renamed} file(s) in {archive_path}");
    Ok(())
}

/// Move files into the archive (like `rar m`): add them through the typed
/// writer, then erase the sources after a successful commit.
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
    let (dict_size_log, dict_size_bytes) = match args.dict_size.as_deref() {
        Some(s) => rar_rs::parse_dict_size(s).ok_or_else(|| format!("Unknown option: md{s}"))?,
        None => (None, None),
    };
    let (format_version, force_v70, v70_dict_bytes) = archive_format_force_v70(
        args.archive_format.as_deref(),
        dict_size_log,
        dict_size_bytes,
    )?;
    let typed_format = if force_v70 {
        rar_rs::ArchiveVersion::Rar70
    } else {
        format_version
    };
    let dictionary = if typed_format == rar_rs::ArchiveVersion::Rar40 {
        None
    } else {
        v70_dict_bytes
            .or(dict_size_bytes)
            .or_else(|| dict_size_log.map(|log| (128u64 * 1024) << log))
            .map(|bytes| {
                rar_rs::DictionarySize::try_from(bytes)
                    .map_err(|error| format!("dictionary: {error}"))
            })
            .transpose()?
    };
    let mut writer = if std::path::Path::new(archive_path).exists() {
        let mut append_opts = rar_rs::AppendOptions::new();
        if let Some(pw) = password {
            append_opts = append_opts.password(pw.clone());
        }
        if let Some(size) = dictionary {
            append_opts = append_opts.dictionary_size(size);
        }
        rar_rs::ArchiveWriter::append_with(archive_path, append_opts)
            .map_err(|e| format!("open: {e}"))?
    } else {
        let ts = time::parse_ts_specs(&args.ts_specs)?;
        let mut writer_opts = rar_rs::WriterOptions::new()
            .format_version(typed_format)
            .save_ctime(ts.save_ctime)
            .save_atime(ts.save_atime)
            .save_mtime(ts.save_mtime)
            .save_owner(misc.owner)
            .save_streams(misc.save_streams)
            .time_precision_seconds(ts.precision_seconds);
        if let Some(pw) = password {
            writer_opts = writer_opts.password(pw.clone());
        }
        if let Some(size) = dictionary {
            writer_opts = writer_opts.dictionary_size(size);
        }
        rar_rs::ArchiveWriter::create_with(archive_path, writer_opts)
            .map_err(|e| format!("create: {e}"))?
    };
    let options = rar_rs::EntryWriteOptions::new().compression_level(
        rar_rs::CompressionLevel::try_from(3).map_err(|e| format!("level: {e}"))?,
    );
    for file in files {
        let name = arg_to_name(file);
        writer
            .add_path_as(file, &name, options)
            .map_err(|e| format!("add {file}: {e}"))?;
    }
    writer.finish().map_err(|e| format!("close: {e}"))?;
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
    let mut rar = open_reader(archive_path, None).map_err(|e| format!("open: {e}"))?;
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

fn open_reader(
    path: impl AsRef<std::path::Path>,
    password: Option<&str>,
) -> Result<rar_rs::ArchiveReader, rar_rs::RarError> {
    let mut options = rar_rs::OpenOptions::new();
    if let Some(password) = password {
        options = options.password(password);
    }
    rar_rs::ArchiveReader::open_with(path, options)
}

/// Verbose list (like `rar v`): adds the packed size, ratio and checksum
/// columns.
fn cmd_verbose_list(args: &ArchiveArgs) -> Result<(), String> {
    let rar = open_reader(&args.archive, args.password.password.as_deref())
        .map_err(|e| format!("{e}"))?;
    output::print_verbose_list(&rar)
}

/// Repair an archive with its inline recovery record (like `rar r`).
/// Writes `fixed.<name>` when damage was found and repaired.
fn cmd_repair(args: &ArchiveArgs) -> Result<(), String> {
    let archive_path = &args.archive;
    let name = std::path::Path::new(archive_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive.rar".to_string());
    let fixed_path = format!("fixed.{name}");
    // Streaming repair: bounded memory regardless of archive size; the
    // repaired archive is staged and renamed atomically by the library.
    let repaired = if is_rar4_file(std::path::Path::new(archive_path)) {
        rar_rs::repair_legacy_archive_path(
            std::path::Path::new(archive_path),
            std::path::Path::new(&fixed_path),
        )
        .map_err(|e| format!("repair: {e}"))?
    } else {
        rar_rs::repair_archive_path(
            std::path::Path::new(archive_path),
            std::path::Path::new(&fixed_path),
        )
        .map_err(|e| format!("repair: {e}"))?
    };
    if !repaired {
        info!("All OK");
        return Ok(());
    }
    // The official tool refuses an obviously truncated archive with a
    // clear error; validate the repaired bytes with our own reader.
    if let Err(e) = rar_rs::RarArchive::open(&fixed_path) {
        let _ = std::fs::remove_file(&fixed_path);
        return Err(format!("repair produced an unreadable archive: {e}"));
    }
    info!("Repaired {archive_path} -> {fixed_path}");
    Ok(())
}

/// Rebuild missing volumes from the `.rev` recovery volumes (like `rar rc`).
fn cmd_rebuild_volumes(args: &ArchiveArgs) -> Result<(), String> {
    let first = &args.archive;
    let rebuilt = rar_rs::rebuild_missing_volumes(std::path::Path::new(first))
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
    let mut editor = open_editor(&args.archive, args.password.password.as_deref())?;
    let remove = comment.is_empty();
    editor
        .apply(rar_rs::EditPlan::new().set_comment(comment))
        .map_err(|e| format!("comment: {e}"))?;
    if remove {
        info!("Comment removed from {archive}", archive = args.archive);
    } else {
        info!("Comment added to {archive}", archive = args.archive);
    }
    Ok(())
}

/// Write the archive comment to stdout (like `rar cw`).
fn cmd_comment_write(args: &ArchiveArgs) -> Result<(), String> {
    let mut rar = match &args.password.password {
        Some(pw) => rar_rs::RarArchive::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        None => rar_rs::RarArchive::open(&args.archive).map_err(|e| format!("open: {e}"))?,
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
    let sfx_offset = rar_rs::sfx_offset_of(&input)
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

/// Locate a `default.sfx` module: `$HOME`, `/usr/lib`, `/usr/local/lib`,
/// or the installed WinRAR directory (Windows: `%ProgramFiles%\WinRAR`,
/// `%ProgramFiles(x86)%\WinRAR`, or the registry-installed path).
fn find_sfx_module() -> Option<String> {
    #[cfg_attr(not(windows), allow(unused_mut))] // mut only for the Windows registry candidates
    let mut candidates: Vec<Option<String>> = vec![
        std::env::var("HOME")
            .ok()
            .map(|h| format!("{h}/default.sfx")),
        Some("/usr/lib/default.sfx".to_string()),
        Some("/usr/local/lib/default.sfx".to_string()),
    ];
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RegCloseKey, RegOpenKeyW, RegQueryValueExW,
        };
        let mut reg_paths: Vec<String> = Vec::new();
        for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
            let mut hkey = std::ptr::null_mut();
            let key: Vec<u16> = "Software\\WinRAR"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let status = unsafe { RegOpenKeyW(root, key.as_ptr(), &mut hkey) };
            if status == 0 {
                let mut buf = [0u16; 1024];
                let mut size = (buf.len() * 2) as u32;
                let value: Vec<u16> = "exe32".encode_utf16().chain(std::iter::once(0)).collect();
                let status = unsafe {
                    RegQueryValueExW(
                        hkey,
                        value.as_ptr(),
                        std::ptr::null(),
                        std::ptr::null_mut(),
                        buf.as_mut_ptr() as *mut u8,
                        &mut size,
                    )
                };
                if status == 0 {
                    let len = (size / 2) as usize;
                    let dir = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
                    reg_paths.push(format!("{dir}\\Default.SFX"));
                    reg_paths.push(format!("{dir}\\WinCon.SFX"));
                }
                unsafe { RegCloseKey(hkey) };
            }
        }
        for pf in [
            std::env::var("ProgramFiles").ok(),
            std::env::var("ProgramFiles(x86)").ok(),
        ]
        .into_iter()
        .flatten()
        {
            reg_paths.push(format!("{pf}\\WinRAR\\Default.SFX"));
            reg_paths.push(format!("{pf}\\WinRAR\\WinCon.SFX"));
        }
        candidates.extend(reg_paths.into_iter().map(Some));
    }
    candidates
        .into_iter()
        .flatten()
        .find(|p| std::path::Path::new(p).exists())
}

/// Change archive parameters (like `rar ch`): member name case conversion
/// with `-cl` / `-cu`.
fn cmd_change(args: &ChangeArgs) -> Result<(), String> {
    let kind = match (args.lowercase, args.uppercase) {
        (true, false) => rar_rs::name_policy::CaseKind::Lower,
        (false, true) => rar_rs::name_policy::CaseKind::Upper,
        _ => return Err("usage: rar ch [-cl | -cu] <archive.rar>".into()),
    };
    let mut editor = match &args.password.password {
        Some(pw) if !pw.is_empty() => rar_rs::ArchiveEditor::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        _ => rar_rs::ArchiveEditor::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    let names: Vec<String> = editor
        .entries()
        .map(|entry| entry.name().to_string())
        .collect();
    let mut pairs = Vec::new();
    for name in names {
        let converted = match kind {
            rar_rs::name_policy::CaseKind::Lower => name.to_lowercase(),
            rar_rs::name_policy::CaseKind::Upper => name.to_uppercase(),
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
    let plan = editor_rename_plan(&editor, &pairs_ref).map_err(|e| format!("ch: {e}"))?;
    let renamed = editor
        .apply(plan)
        .map_err(|e| format!("ch: {e}"))?
        .renamed();
    info!(
        "Converted {renamed} name(s) in {archive}",
        archive = args.archive
    );
    Ok(())
}

/// Print a member to stdout (like `rar p`).
fn cmd_print(args: &PrintArgs) -> Result<(), String> {
    use std::io::Write;
    let mut rar = open_reader(&args.archive, args.password.password.as_deref())
        .map_err(|e| format!("open: {e}"))?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let options = rar_rs::ExtractOptions {
        max_unpacked_bytes: None,
        max_total_unpacked_bytes: None,
        ..Default::default()
    };
    let wanted: Vec<_> = if let Some(file) = &args.file {
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
    if wanted.is_empty() && args.file.is_some() {
        let file = args.file.as_deref().unwrap_or_default();
        return Err(format!(
            "no archive members matched the requested name(s): {file}"
        ));
    }
    for id in wanted {
        let name = rar
            .entry(id)
            .map_err(|e| format!("resolve archive member: {e}"))?
            .name()
            .to_string();
        rar.copy_entry_to_with_options(id, &mut out, options)
            .map_err(|e| format!("{name}: {e}"))?;
    }
    out.flush().map_err(|e| format!("stdout: {e}"))
}

/// Extract with full paths (like `rar x`).
fn cmd_extract(args: &ExtractArgs) -> Result<(), String> {
    if let Some(threads) = args.threads {
        rar_rs::set_extraction_threads(threads);
    }
    let dest = extract_dest(args)?;
    let mut rar = open_reader(&args.archive, args.password.password.as_deref())
        .map_err(|e| format!("open: {e}"))?;
    // `-so`: write the extracted members to stdout (one stream) instead of
    // to disk — handy for piping. Directories carry no data.
    if args.stdout {
        return extract_to_stdout(&mut rar, &args.names);
    }
    let skip = args.overwrite.as_deref() == Some("never");
    let count = if args.names.is_empty() {
        rar.extract_all_with_options(
            &dest,
            rar_rs::ExtractOptions {
                skip_existing: skip,
                ..Default::default()
            },
        )
        .map_err(|e| format!("{e}"))?;
        rar.entries().len()
    } else {
        extract_selected(&mut rar, &dest, &args.names, false, skip)?
    };
    info!("Extracted {count} file(s) to {}", dest.display());
    Ok(())
}

/// Extract every file member of an archive to stdout, concatenated, like
/// `rar/unrar x -so`. Informational messages are suppressed so the stream
/// stays clean.
fn extract_to_stdout(rar: &mut rar_rs::ArchiveReader, names: &[String]) -> Result<(), String> {
    use std::io::Write;
    let wanted = selector::select_entries(
        rar.entries()
            .filter(|entry| !entry.is_dir())
            .map(|entry| (entry.id(), entry.metadata().name())),
        names,
    );
    if wanted.is_empty() && !names.is_empty() {
        return Err(format!(
            "no archive members matched the requested name(s): {}",
            names.join(", ")
        ));
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let options = rar_rs::ExtractOptions {
        max_unpacked_bytes: None,
        max_total_unpacked_bytes: None,
        ..Default::default()
    };
    for id in wanted {
        let name = rar
            .entry(id)
            .map_err(|e| format!("resolve archive member: {e}"))?
            .name()
            .to_string();
        rar.copy_entry_to_with_options(id, &mut out, options)
            .map_err(|e| format!("read {name}: {e}"))?;
    }
    out.flush().map_err(|e| format!("stdout: {e}"))
}

/// Destination directory, honoring `-ad` (append the archive base name).
fn extract_dest(args: &ExtractArgs) -> Result<std::path::PathBuf, String> {
    Ok(output::extract_dest(
        &args.dest,
        &args.archive,
        args.append_dir,
    ))
}

/// Extract only the members whose name matches one of `names` (full stored
/// path or basename). Errors clearly when no member matches, so a mistyped
/// name is never silently swallowed or treated as a destination directory.
fn extract_selected(
    rar: &mut rar_rs::ArchiveReader,
    dest: &std::path::Path,
    names: &[String],
    flat: bool,
    skip_existing: bool,
) -> Result<usize, String> {
    let wanted = selector::select_entries(
        rar.entries()
            .filter(|entry| !entry.is_dir())
            .map(|entry| (entry.id(), entry.metadata().name())),
        names,
    );
    if wanted.is_empty() {
        return Err(format!(
            "no archive members matched the requested name(s): {}",
            names.join(", ")
        ));
    }
    for &id in &wanted {
        let member = rar
            .entry(id)
            .map_err(|e| format!("resolve archive member: {e}"))?
            .name()
            .to_string();
        let opts = rar_rs::ExtractOptions {
            flat_paths: flat,
            skip_existing,
            ..Default::default()
        };
        rar.extract_entry_with_options(id, dest, opts)
            .map_err(|e| format!("extract {member}: {e}"))?;
    }
    Ok(wanted.len())
}

/// Extract without archived paths (like `rar e`).
fn cmd_extract_flat(args: &ExtractArgs) -> Result<(), String> {
    if let Some(threads) = args.threads {
        rar_rs::set_extraction_threads(threads);
    }
    let dest = extract_dest(args)?;
    let mut rar = open_reader(&args.archive, args.password.password.as_deref())
        .map_err(|e| format!("open: {e}"))?;
    if args.stdout {
        return extract_to_stdout(&mut rar, &args.names);
    }
    let skip = args.overwrite.as_deref() == Some("never");
    let count = if args.names.is_empty() {
        rar.extract_all_with_options(
            &dest,
            rar_rs::ExtractOptions {
                flat_paths: true,
                skip_existing: skip,
                ..Default::default()
            },
        )
        .map_err(|e| format!("{e}"))?;
        rar.entries().len()
    } else {
        extract_selected(&mut rar, &dest, &args.names, true, skip)?
    };
    info!("Extracted {count} file(s) to {}", dest.display());
    Ok(())
}

/// Test archive contents (like `rar t`).
fn cmd_test(args: &ArchiveArgs) -> Result<(), String> {
    let mut rar = open_reader(&args.archive, args.password.password.as_deref())
        .map_err(|e| format!("open: {e}"))?;
    let report: rar_rs::VerificationReport = rar.verify().map_err(|e| format!("test: {e}"))?;
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
        Err("test failed".into())
    }
}

/// Normalize a path argument into an archive name: relative paths stay as
/// given, absolute paths drop the leading slash (like `rar`).
fn arg_to_name(arg: &str) -> String {
    rar_rs::name_policy::arg_to_name(arg)
}

/// Read one mask per line from a filter list file (like `-x@listfile`);
/// blank lines are skipped.
fn read_mask_file(path: &str) -> Result<Vec<String>, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read mask list {path}: {e}"))?;
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
    let rar = open_reader(&args.archive, args.password.password.as_deref())
        .map_err(|e| format!("{e}"))?;

    println!(
        "{:>10}  {:>10}  {:>6}  {:<8}  Name",
        "Size", "Packed", "Ratio", "Method"
    );
    println!("{}", "-".repeat(60));

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
        rar.entries().len()
    );

    Ok(())
}

/// Bare list (`lb` / `vb`): member names only.
fn cmd_list_bare(args: &ArchiveArgs) -> Result<(), String> {
    let rar = open_reader(&args.archive, args.password.password.as_deref())
        .map_err(|e| format!("{e}"))?;
    for entry in rar.entries() {
        println!("{}", entry.name());
    }
    Ok(())
}

/// Technical list (`lt` / `vt`): mtime, attributes, sizes, ratio, CRC and
/// method per member, in the spirit of the official `rar lt`.
fn cmd_list_technical(args: &ArchiveArgs) -> Result<(), String> {
    let rar = open_reader(&args.archive, args.password.password.as_deref())
        .map_err(|e| format!("{e}"))?;
    println!(
        "{:>10}  {:>10}  {:>6}  {:>10}  {:<8}  {:<19}  Name",
        "Size", "Packed", "Ratio", "Checksum", "Method", "Modified"
    );
    println!("{}", "-".repeat(86));
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
        let secs = entry.mtime();
        let days = (secs as i64) / 86400;
        let tod = secs % 86400;
        let (y, mo, d) = time::civil_from_days(days);
        let modified = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            y,
            mo,
            d,
            tod / 3600,
            (tod % 3600) / 60,
            tod % 60
        );
        println!(
            "{:>10}  {:>10}  {:>6}  {:>10}  {:<8}  {:<19}  {}",
            entry.size(),
            entry.compressed_size(),
            ratio,
            checksum,
            entry.method_name(),
            modified,
            entry.name()
        );
    }
    Ok(())
}

fn cmd_info(args: &ArchiveArgs) -> Result<(), String> {
    let rar = open_reader(&args.archive, args.password.password.as_deref())
        .map_err(|e| format!("{e}"))?;

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

/// Reorder `collected` according to a rarfiles.lst mask list (`None` =
/// `$default` position). Each file is placed in the group of its
/// highest-priority matching mask: the earliest mask wins, except that a
/// mask whose match set is a subset of another's takes priority over it
/// regardless of position (WinRAR rule). Files matching nothing go to
/// `$default`, or to the end when there is no `$default`. The sort is
/// stable, so files inside a group keep their collection order.
fn apply_rarfiles_order(
    collected: &mut Vec<rar_rs::name_policy::Collected>,
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
                .filter(|(_, c)| {
                    pat.is_some_and(|p| {
                        rar_rs::name_policy::mask_match(p, c.name.trim_start_matches("./"))
                    })
                })
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
                    m.as_deref().is_some_and(|p| {
                        rar_rs::name_policy::mask_match(p, c.name.trim_start_matches("./"))
                    })
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
    order.sort_by_key(|&i| best[i]);
    let reordered: Vec<rar_rs::name_policy::Collected> =
        order.into_iter().map(|i| collected[i].clone()).collect();
    *collected = reordered;
}

/// Whether `path` carries the legacy 7-byte `Rar!\x1a\x07\x00` signature
/// (peek at the head, tolerating an SFX stub within the first 8 MiB).
fn is_rar4_file(path: &std::path::Path) -> bool {
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

#[cfg(test)]
mod tests {
    use super::{parse_size, update_archive_transactionally};

    #[test]
    fn size_parsing_checks_multiplication_overflow() {
        assert_eq!(parse_size("2k"), Ok(2 * 1024));
        assert!(parse_size("18446744073709551615g").is_err());
    }

    #[test]
    fn failed_archive_transaction_preserves_original_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive.rar");
        std::fs::write(&archive, b"original").unwrap();

        let result = update_archive_transactionally(&archive, |staged| {
            std::fs::write(staged, b"damaged staged copy").unwrap();
            Err("injected append failure".into())
        });

        assert_eq!(result, Err("injected append failure".into()));
        assert_eq!(std::fs::read(&archive).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn successful_archive_transaction_replaces_original_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive.rar");
        std::fs::write(&archive, b"original").unwrap();

        update_archive_transactionally(&archive, |staged| {
            std::fs::write(staged, b"replacement").map_err(|error| error.to_string())
        })
        .unwrap();

        assert_eq!(std::fs::read(&archive).unwrap(), b"replacement");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
