//! Clap surface: the `rar` command tree and its switch parsers.

use clap::{Args, Parser, Subcommand};

use crate::common;
use crate::password;

#[derive(Parser)]
#[command(
    name = "rar",
    version,
    about = "create and modify RAR archives",
    long_about = "Pure-Rust RAR4/RAR5/RAR7 archive tool: create, append, update, delete, rename,\nlock, repair and extract archives, with WinRAR-compatible switches.",
    propagate_version = true
)]
pub(crate) struct Cli {
    /// Assume yes on all queries (like `-y`)
    #[arg(short = 'y', long, global = true)]
    pub(crate) yes: bool,
    /// Quiet mode: suppress informational messages (like `-idq` / `-inul`)
    #[arg(long, global = true)]
    pub(crate) quiet: bool,
    /// Send informational messages to stderr (like `-ierr`)
    #[arg(long, global = true)]
    pub(crate) err: bool,
    /// Work directory (like `-w<path>`)
    #[arg(long = "work-dir", global = true)]
    pub(crate) work_dir: Option<String>,
    /// Misc switches (-ow, -tsp, -ilog, -ver, and compatibility switches)
    #[command(flatten)]
    pub(crate) misc: common::MiscSwitches,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
// clap command enums carry large variant payloads by design.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Command {
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
    /// Move files only: like `m`, but directories are neither archived nor
    /// removed
    #[command(name = "mf")]
    MoveFiles(FilesArgs),
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
    CommentWrite(CommentWriteArgs),
    /// Set a member's file comment (from stdin, or `-z<file>`)
    #[command(visible_alias = "cf")]
    CommentFileSet(FileCommentArgs),
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
    Test(ListArgs),
    /// Verbosely list archive contents
    #[command(visible_alias = "v")]
    VerboseList(ListArgs),
    /// List archive contents
    #[command(visible_alias = "l")]
    List(ListArgs),
    /// List bare (names only, like `lb`)
    #[command(visible_alias = "lb")]
    ListBare(ListArgs),
    /// List technical (like `lt`; `lta` is accepted as an alias — service
    /// records are not listed)
    #[command(visible_aliases = ["lt", "lta"])]
    ListTechnical(ListArgs),
    /// Verbosely list bare (like `vb`)
    #[command(visible_alias = "vb")]
    VerboseListBare(ListArgs),
    /// Verbosely list technical (like `vt`; `vta` is accepted as an alias)
    #[command(visible_aliases = ["vt", "vta"])]
    VerboseListTechnical(ListArgs),
    /// Show archive info
    #[command(visible_alias = "i")]
    Info(ArchiveArgs),
    /// Unknown commands starting with `i` are the string search: `rar i<string>`
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// `-p<password>` plus an archive path.
#[derive(Args)]
pub(crate) struct ArchiveArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
}

/// Write the archive comment to stdout or a file (like `rar cw`).
#[derive(Args)]
pub(crate) struct CommentWriteArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
    /// Write the comment to this file instead of stdout
    #[arg(value_name = "FILE")]
    pub(crate) output: Option<String>,
}

/// Archive path plus optional member filters (test / list commands).
#[derive(Args)]
pub(crate) struct ListArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
    /// Member names to list/test (empty = every member)
    #[arg(value_name = "NAMES")]
    pub(crate) names: Vec<String>,
}

/// `rar ch` parameters: member name case conversion (-cl / -cu).
#[derive(Args)]
pub(crate) struct ChangeArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
    /// Convert stored names to lowercase (-cl)
    #[arg(long = "lowercase")]
    pub(crate) lowercase: bool,
    /// Convert stored names to uppercase (-cu)
    #[arg(long = "uppercase")]
    pub(crate) uppercase: bool,
}

/// `rar rv` parameters: the first volume of the set plus an optional
/// recovery-volume count (`rv3`) or percent (`rv10%`); defaults to 10%.
#[derive(Args)]
pub(crate) struct RecoveryVolumesArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
    /// Recovery volumes: a count (`3`) or percent (`10%`); default 10%
    #[arg(value_name = "COUNT|PCT%", default_value = "10%")]
    pub(crate) count_spec: String,
}

/// Archive path plus an optional member to print.
#[derive(Args)]
pub(crate) struct PrintArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
    #[arg(value_name = "FILE")]
    pub(crate) file: Option<String>,
}

/// Comment setting: stdin by default, or `-z<file>`.
#[derive(Args)]
pub(crate) struct CommentArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
}

/// Per-member comment setting: stdin by default, or `-z<file>`.
#[derive(Args)]
pub(crate) struct FileCommentArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
    /// The archive member to annotate
    #[arg(value_name = "MEMBER")]
    pub(crate) member: String,
}

/// Archive path plus an optional destination directory.
#[derive(Args)]
pub(crate) struct ExtractArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
    #[arg(long = "dest", value_name = "DEST")]
    pub(crate) dest: Option<String>,
    /// One or more member names to extract; when omitted, every file member
    /// is extracted (or, with `-so`, written to stdout). Member names match
    /// the full stored path or its basename. A trailing argument ending
    /// with a path separator is treated as the destination directory.
    #[arg(value_name = "NAMES")]
    pub(crate) names: Vec<String>,
    /// Compression threads (like `-mt<N>`; also used for extraction)
    #[arg(long = "threads", value_name = "N", value_parser = parse_threads)]
    pub(crate) threads: Option<usize>,
    /// Alternate destination: `-ad` appends the archive base name to the
    /// destination, `-ad1` uses the archive's directory with the base name,
    /// `-ad2` the archive's directory itself
    #[arg(
        long = "append-dir",
        value_name = "1|2",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = ""
    )]
    pub(crate) append_dir: Option<String>,
    /// Overwrite mode (like `-o+` / `-o-`)
    #[arg(
        long = "overwrite",
        value_name = "MODE",
        value_parser = ["always", "never"]
    )]
    pub(crate) overwrite: Option<String>,
    /// Extract to stdout instead of writing files (like `-so`); convenient
    /// for piping a member's contents. All file members are concatenated to
    /// stdout (directories are skipped).
    #[arg(long = "stdout")]
    pub(crate) stdout: bool,
    /// Keep partially extracted files when a member fails to decode
    /// (like `-kb`; the default deletes them)
    #[arg(long = "keep-broken")]
    pub(crate) keep_broken: bool,
    /// Rename the destination automatically when it already exists
    /// (like `-or`): `name.ext` becomes `name(1).ext`, ...
    #[arg(long = "auto-rename")]
    pub(crate) auto_rename: bool,
    /// Output path (like `-op<path>`); overrides the `--dest` base
    #[arg(long = "output-path", value_name = "PATH")]
    pub(crate) output_path: Option<String>,
}

/// Archive path plus one or more source files.
#[derive(Args)]
pub(crate) struct FilesArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
    #[arg(value_name = "FILES", required = true)]
    pub(crate) files: Vec<String>,
    /// Dictionary size for compression (like `-md<size>`)
    #[arg(long = "dict-size", value_name = "SIZE")]
    pub(crate) dict_size: Option<String>,
    /// Archive format version (like `-ma5`; `-ma7` forces RAR7/v70)
    #[arg(long = "archive-format", value_name = "VER", hide = true)]
    pub(crate) archive_format: Option<String>,
    /// Extraction dictionary cap (like `-mdx<size>`; accepted for CLI
    /// compatibility and unused by update/move operations)
    #[arg(long = "dict-extract", value_name = "SIZE")]
    pub(crate) dict_extract: Option<String>,
    /// Save/restore file times (like `-ts[m,c,a][+,-,1]`; repeatable)
    #[arg(long = "ts", value_name = "SPEC", action = clap::ArgAction::Append)]
    pub(crate) ts_specs: Vec<String>,
    /// Keep the archive's original modification time, or set it to the
    /// given date (like `-tk[<date>]`, YYYYMMDDHHMMSS with optional
    /// separators)
    #[arg(
        long = "keep-time",
        value_name = "DATE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = ""
    )]
    pub(crate) keep_time: Option<String>,
    /// Save symbolic links as links instead of the file (like `-ol`)
    #[arg(long = "links")]
    pub(crate) store_links: bool,
    /// Save hard links as links instead of the file (like `-oh`)
    #[arg(long = "hardlinks")]
    pub(crate) store_hardlinks: bool,
    /// Advanced compression parameters (like `-mc<par>`)
    #[arg(long = "mc", value_name = "PAR")]
    pub(crate) mc_params: Option<String>,
}

/// Archive path plus the members to delete.
#[derive(Args)]
pub(crate) struct DeleteArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
    #[arg(value_name = "NAMES")]
    pub(crate) names: Vec<String>,
}

/// Archive path plus old/new name pairs.
#[derive(Args)]
pub(crate) struct RenameArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
    #[arg(value_name = "OLD", required = true, num_args = 2..)]
    pub(crate) pairs: Vec<String>,
}

/// Archive path plus the recovery percentage.
#[derive(Args)]
pub(crate) struct RecoveryArgs {
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
    #[arg(
        value_name = "PERCENT",
        default_value_t = 10,
        value_parser = clap::value_parser!(u8).range(0..=100)
    )]
    pub(crate) percent: u8,
}

/// SFX conversion arguments.
#[derive(Args)]
pub(crate) struct SfxArgs {
    /// SFX module file (default: $HOME/default.sfx or /usr/lib)
    #[arg(long = "sfx-module", value_name = "MODULE")]
    pub(crate) module: Option<String>,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
}

/// Creation switches, keeping the rar spellings (`-m3`, `-psecret`,
/// `-v100k`, `-x<mask>`, ...) plus long aliases.
#[derive(Args)]
pub(crate) struct CreateArgs {
    /// Compression level 0-5
    #[arg(
        short = 'm',
        long = "level",
        value_name = "N",
        default_value_t = 3,
        value_parser = clap::value_parser!(u8).range(0..=5)
    )]
    pub(crate) level: u8,
    #[command(flatten)]
    pub(crate) password: password::PasswordArgs,
    /// Volume size (e.g. 1m, 100k)
    #[arg(short = 'v', long = "volume-size", value_name = "SIZE", value_parser = parse_size)]
    pub(crate) volume_size: Option<u64>,
    /// Solid archive
    #[arg(short = 's', long)]
    pub(crate) solid: bool,
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
    pub(crate) solid_reset: String,
    /// BLAKE2sp hash records
    #[arg(long = "blake2")]
    pub(crate) blake2: bool,
    /// Quick-open record
    #[arg(long = "quick-open")]
    pub(crate) quick_open: bool,
    /// Disable quick-open (`-qo-`; the default, kept for switch parity)
    #[arg(long = "no-quick-open", hide = true)]
    pub(crate) no_quick_open: bool,
    /// Header encryption (optionally with a password as `-hp{pwd}` /
    /// `--header-encrypt={pwd}`; a bare `-hp` turns it on with the `-p`
    /// password). The value is attached (WinRAR style), so a following
    /// position argument is never swallowed.
    #[arg(
        long = "header-encrypt",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = ""
    )]
    pub(crate) header_encrypt: Option<String>,
    /// Dictionary size for compression (like `-md<size>[k|m|g]`; no unit
    /// means MiB, valid values 128K..4G powers of two)
    #[arg(long = "dict-size", value_name = "SIZE")]
    pub(crate) dict_size: Option<String>,
    /// Archive format version (like `-ma5`; `-ma7` forces RAR7/v70 — an
    /// extension beyond WinRAR 7.23, which has no such switch)
    #[arg(long = "archive-format", value_name = "VER", hide = true)]
    pub(crate) archive_format: Option<String>,
    /// Extraction dictionary cap (like `-mdx<size>`; accepted for CLI
    /// compatibility and unused while creating archives)
    #[arg(long = "dict-extract", value_name = "SIZE")]
    pub(crate) dict_extract: Option<String>,
    /// Save/restore file times (like `-ts[m,c,a][+,-,1]`; repeatable)
    #[arg(long = "ts", value_name = "SPEC", action = clap::ArgAction::Append)]
    pub(crate) ts_specs: Vec<String>,
    /// Recovery record percentage
    #[arg(
        long = "recovery-percent",
        value_name = "N",
        value_parser = clap::value_parser!(u8).range(0..=100)
    )]
    pub(crate) recovery_percent: Option<u8>,
    /// Recovery volumes: count or percentage (`20` or `20%`)
    #[arg(long = "recovery-volumes", value_name = "N|N%", value_parser = parse_recovery_volumes)]
    pub(crate) recovery_volumes: Option<RecoveryVolumes>,
    /// Compression threads
    #[arg(long = "threads", value_name = "N", value_parser = parse_threads)]
    pub(crate) threads: Option<usize>,
    /// Store basename-only names (no directory entries)
    #[arg(long = "basename-only")]
    pub(crate) basename_only: bool,
    /// Exclude the base directory from names (wildcard paths)
    #[arg(long = "exclude-base-dir")]
    pub(crate) strip_base: bool,
    /// Store full paths without the drive letter (like `-ep2`)
    #[arg(long = "full-paths")]
    pub(crate) full_paths: bool,
    /// Store full paths including the drive letter (like `-ep3`)
    #[arg(long = "full-paths-drive")]
    pub(crate) full_paths_drive: bool,
    /// Do not recurse into directories
    #[arg(long = "no-recurse")]
    pub(crate) no_recurse: bool,
    /// Recurse subdirectories (like `-r`; the default for directory args)
    #[arg(long = "recurse")]
    pub(crate) recurse: bool,
    /// Recurse, but wildcards only match names without path separators
    /// (like `-r0`)
    #[arg(long = "recurse-zero")]
    pub(crate) recurse_zero: bool,
    /// Convert stored names to lowercase
    #[arg(long = "lowercase")]
    pub(crate) lowercase: bool,
    /// Convert stored names to uppercase
    #[arg(long = "uppercase")]
    pub(crate) uppercase: bool,
    /// Prefix for stored names
    #[arg(long = "archive-path", value_name = "PATH")]
    pub(crate) path_prefix: Option<String>,
    /// Exclude mask (repeatable)
    #[arg(long = "exclude", value_name = "MASK", action = clap::ArgAction::Append)]
    pub(crate) exclude_masks: Vec<String>,
    /// Include mask (repeatable)
    #[arg(long = "include", value_name = "MASK", action = clap::ArgAction::Append)]
    pub(crate) include_masks: Vec<String>,
    /// Exclude masks read from a list file (repeatable, like `-x@listfile`)
    #[arg(long = "exclude-list", value_name = "FILE", action = clap::ArgAction::Append)]
    pub(crate) exclude_list_files: Vec<String>,
    /// Include masks read from a list file (repeatable, like `-n@listfile`)
    #[arg(long = "include-list", value_name = "FILE", action = clap::ArgAction::Append)]
    pub(crate) include_list_files: Vec<String>,
    /// Only process files modified after this date (like `-ta<date>`,
    /// YYYYMMDDHHMMSS, trailing parts optional)
    #[arg(long = "after", value_name = "DATE")]
    pub(crate) after: Option<String>,
    /// Only process files modified before this date (like `-tb<date>`)
    #[arg(long = "before", value_name = "DATE")]
    pub(crate) before: Option<String>,
    /// Only process files newer than this period (like `-tn[mods]<time>`,
    /// period is `[<ndays>d][<nhours>h][<nminutes>m][<nseconds>s]`;
    /// may be repeated, all filters must match)
    #[arg(long = "tn-filter", value_name = "PERIOD")]
    pub(crate) tn_filters: Vec<String>,
    /// Only process files older than this period (like `-to[mods]<time>`)
    #[arg(long = "to-filter", value_name = "PERIOD")]
    pub(crate) to_filters: Vec<String>,
    /// Set the archive time to the newest member (like `-tl`)
    #[arg(long = "set-latest-time")]
    pub(crate) latest_time: bool,
    /// Keep the archive's original modification time, or set it to the
    /// given date (like `-tk[<date>]`, YYYYMMDDHHMMSS with optional
    /// separators)
    #[arg(
        long = "keep-time",
        value_name = "DATE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = ""
    )]
    pub(crate) keep_time: Option<String>,
    /// Generate the archive name from the current date (like `-ag[format]`;
    /// `*` in the name is replaced, `YYYY`/`MM`/`DD`/`HH`/`MM`/`SS` in the
    /// format are substituted)
    #[arg(
        long = "auto-name",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = ""
    )]
    pub(crate) auto_name: Option<String>,
    /// Create a self-extracting archive, optionally with a specific SFX
    /// module (like `-sfx[name]`)
    #[arg(
        long = "sfx-module",
        value_name = "MODULE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = ""
    )]
    pub(crate) sfx_module: Option<String>,
    /// Save symbolic links as links instead of the file (like `-ol`)
    #[arg(long = "links")]
    pub(crate) store_links: bool,
    /// Save hard links as links instead of the file (like `-oh`)
    #[arg(long = "hardlinks")]
    pub(crate) store_hardlinks: bool,
    /// Only process files smaller than this size (like `-sl<size>`, units
    /// b/k/m/g)
    #[arg(long = "size-less", value_name = "SIZE", value_parser = parse_size)]
    pub(crate) size_less: Option<u64>,
    /// Only process files larger than this size (like `-sm<size>`)
    #[arg(long = "size-more", value_name = "SIZE", value_parser = parse_size)]
    pub(crate) size_more: Option<u64>,
    /// Do not add empty directories (like `-ed`)
    #[arg(long = "no-empty-dirs")]
    pub(crate) no_empty_dirs: bool,
    /// Do not show the archive comment (like `-c-`; accepted, comments are
    /// never displayed by this tool)
    #[arg(long = "no-comment", global = true)]
    pub(crate) no_comment: bool,
    /// Store files matching these types without compression (like
    /// `-ms[list]`; semicolon-separated extensions or wildcard masks,
    /// repeatable)
    #[arg(long = "store-types", value_name = "LIST", action = clap::ArgAction::Append)]
    pub(crate) store_types: Vec<String>,
    /// Delete source files after archiving (like `-df`)
    #[arg(long = "delete-after")]
    pub(crate) delete_after: bool,
    /// Test the archive after creating it (like `-t`)
    #[arg(long = "test-after")]
    pub(crate) test_after: bool,
    /// Exclude this path prefix from stored names (like `-ep4<path>`)
    #[arg(long = "exclude-prefix", value_name = "PATH")]
    pub(crate) exclude_prefix: Option<String>,
    /// Synchronize archive contents: delete members not present in the
    /// file list (like `-as`)
    #[arg(long = "sync-archive")]
    pub(crate) sync_archive: bool,
    /// Disable name sorting for solid archives (like `-ds`)
    #[arg(long = "no-sort")]
    pub(crate) no_sort: bool,
    /// Solid archive parameters (like `-s<par>`; accepted, `-s` alone
    /// already enables solid mode)
    #[arg(long = "solid-params", value_name = "PAR")]
    #[allow(dead_code)]
    pub(crate) solid_params: Option<String>,
    /// Advanced compression parameters (like `-mc<par>`)
    #[arg(long = "mc", value_name = "PAR")]
    pub(crate) mc_params: Option<String>,
    /// Long-distance matching control (like `-mcl`; accepted). Long-range
    /// matching is always enabled for `-m2`…`-m5`, so this is a no-op that
    /// matches WinRAR 7.23's own behaviour.
    #[arg(long = "long-match", value_name = "PAR")]
    #[allow(dead_code)]
    pub(crate) long_match: Option<String>,
    /// Move deleted files to the Recycle Bin (like `-dr`; unsupported)
    #[arg(long = "recycle-bin")]
    pub(crate) recycle_bin: bool,
    /// Securely wipe files after archiving (like `-dw`; unsupported)
    #[arg(long = "wipe")]
    pub(crate) wipe: bool,
    /// Read one member from stdin under this name (like `-si<name>`)
    #[arg(long = "stdin-name", value_name = "NAME")]
    pub(crate) stdin_name: Option<String>,
    #[arg(value_name = "ARCHIVE")]
    pub(crate) archive: String,
    #[arg(value_name = "FILES")]
    pub(crate) files: Vec<String>,
}

/// Recovery volumes parameter: an exact count or a percentage.
#[derive(Clone, Copy)]
pub(crate) enum RecoveryVolumes {
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

pub(crate) fn parse_size(s: &str) -> Result<u64, String> {
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

/// Resolve a `-ma<ver>` archive-format request into the single version
/// table knob. `-ma4` selects the legacy RAR3/4 container through its
/// writable unpack version [`rar_rs::ArchiveVersion::V29`]; `-ma2` and
/// `-ma15` write RAR 2.x (v20) and RAR 1.5 (v15) members through the legacy
/// pipeline (solid chains and `-p`/`-hp` encryption included); `-ma5` is the default RAR5 v50 version (a no-op, like
/// WinRAR's accepted-but-inert `-ma5`); `-ma7` forces RAR7 (v70) members
/// with the `-md` dictionary
/// (default 32 MiB) declared in the header — an extension beyond
/// WinRAR 7.23, which only writes v70 above 4 GiB. Any other version is
/// rejected like WinRAR rejects unknown options.
/// Returns `(version, dict_bytes)`, where `dict_bytes` is the declared
/// v70 byte dictionary for `-ma7` (else the plain `-md` bytes, carried
/// for the RAR5 auto v50/v70 mode). Under `-ma7` the dictionary may be
/// any supported byte count; non-power-of-two sizes through 4 GiB land
/// here through [`resolve_dict_switch`].
pub(crate) fn archive_version(
    ma: Option<&str>,
    dict_size_log: Option<u8>,
    dict_size_bytes: Option<u64>,
) -> Result<(rar_rs::ArchiveVersion, Option<u64>), String> {
    match ma {
        Some("4") => Ok((rar_rs::ArchiveVersion::V29, None)),
        Some("2") => Ok((rar_rs::ArchiveVersion::V20, None)),
        Some("15") => Ok((rar_rs::ArchiveVersion::V15, None)),
        Some("13") | Some("14") => Ok((rar_rs::ArchiveVersion::V14, None)),
        None | Some("5") => Ok((rar_rs::ArchiveVersion::V50, dict_size_bytes)),
        Some("7") => {
            let bytes = dict_size_bytes
                .or_else(|| dict_size_log.map(|l| (128u64 * 1024) << l))
                .unwrap_or(32 * 1024 * 1024);
            Ok((rar_rs::ArchiveVersion::V70, Some(bytes)))
        }
        Some(other) => Err(format!("Unknown option: ma{other}")),
    }
}

/// Parse a `-md<size>` switch, resolving it strictly for the RAR5 range
/// (powers of two only, matching WinRAR's rejection of e.g. `-md3m`) and
/// falling back to a raw byte count when the `-ma7` extension forces v70:
/// a non-power-of-two size through 4 GiB (like `6m`) is representable
/// exactly in the v70 byte-dictionary header, so the `-ma7` path accepts
/// it while plain v50 (`-ma5`/default) still reports "Unknown option".
pub(crate) fn resolve_dict_switch(
    spec: &str,
    ma: Option<&str>,
) -> Result<(Option<u8>, Option<u64>), String> {
    if let Some(parsed) = rar_rs::parse_dict_size(spec) {
        return Ok(parsed);
    }
    if ma == Some("7")
        && let Some(bytes) = rar_rs::parse_dict_bytes(spec)
    {
        return Ok((None, Some(bytes)));
    }
    Err(format!("Unknown option: md{spec}"))
}

/// Parse a normalized `-mc<par>` value into a filter policy.
///
/// Grammar: `[channels][mode][+|-]` with `D` (delta), `E` (x86), `L`
/// (long range) and `X` (exhaustive). No sign keeps RAR's automatic
/// choice; `-mc-` disables every mode. WinRAR's parser ignores anything
/// it does not recognize here, so this does too (channels and the mode
/// letter may appear in either order). `-mcl±`/`-mcx±` are accepted
/// without effect: long-range matching is always on for m2–m5 and the
/// exhaustive parser is not implemented.
pub(crate) fn parse_mc_params(spec: &str) -> rar_rs::FilterOptions {
    use rar_rs::FilterMode;

    let mut filters = rar_rs::FilterOptions::default();
    let mut rest = spec;
    let forced = match rest.chars().last() {
        Some('+') => {
            rest = &rest[..rest.len() - 1];
            Some(true)
        }
        Some('-') => {
            rest = &rest[..rest.len() - 1];
            Some(false)
        }
        _ => None,
    };
    let mode_state = match forced {
        None => FilterMode::Auto,
        Some(true) => FilterMode::Forced,
        Some(false) => FilterMode::Disabled,
    };
    let mut channels: Option<u8> = None;
    let mut digits = String::new();
    let mut delta = false;
    let mut x86 = false;
    for ch in rest.chars() {
        match ch {
            '0'..='9' => digits.push(ch),
            'd' | 'D' => delta = true,
            'e' | 'E' => x86 = true,
            // Long-range and exhaustive modes carry no configuration here.
            'l' | 'L' | 'x' | 'X' => {}
            _ => {}
        }
    }
    if let Ok(value) = digits.parse::<u8>()
        && (1..=31).contains(&value)
    {
        channels = Some(value);
    }
    let no_mode = !delta && !x86;
    if no_mode {
        if mode_state == FilterMode::Disabled {
            filters.delta = FilterMode::Disabled;
            filters.x86 = FilterMode::Disabled;
        }
        return filters;
    }
    if delta {
        filters.delta = mode_state;
        filters.delta_channels = channels;
    }
    if x86 {
        filters.x86 = mode_state;
    }
    filters
}

/// Adapt create switches for the `a -f` / `a -u` delegation to the
/// update/freshen pipeline: only the fields those paths consume are
/// carried over (creation-only switches like volumes or recovery are
/// silently ignored, like WinRAR's command-string equivalence implies).
pub(crate) fn as_files_args(args: &CreateArgs) -> FilesArgs {
    FilesArgs {
        password: args.password.clone(),
        archive: args.archive.clone(),
        files: args.files.clone(),
        dict_size: args.dict_size.clone(),
        archive_format: args.archive_format.clone(),
        dict_extract: args.dict_extract.clone(),
        ts_specs: args.ts_specs.clone(),
        keep_time: args.keep_time.clone(),
        store_links: args.store_links,
        store_hardlinks: args.store_hardlinks,
        mc_params: args.mc_params.clone(),
    }
}

/// Whether an archive member name matches one `-ms<list>` entry: a bare
/// extension (`bin` matches `*.bin`) or a wildcard mask (`*.bin`, `a?c`)
/// matched against the basename.
/// Expand source arguments with the same name policy used by create, while
/// ensuring a directory argument never feeds the destination archive back
/// into the operation.
pub(crate) fn collect_inputs(
    policy: &crate::name_policy::NamePolicy,
    files: &[String],
    level: u8,
    archive_path: &str,
) -> Result<Vec<crate::name_policy::Collected>, String> {
    let mut collected =
        crate::name_policy::collect(policy, files, level).map_err(|e| format!("collect: {e}"))?;
    if let Ok(abs_archive) = std::fs::canonicalize(archive_path) {
        collected.retain(
            |item| !matches!(std::fs::canonicalize(&item.path), Ok(path) if path == abs_archive),
        );
    }
    Ok(collected)
}

pub(crate) fn store_type_matches(mask: &str, name: &str) -> bool {
    if mask.contains('*') || mask.contains('?') {
        let base = name.rsplit('/').next().unwrap_or(name);
        return wildcard_match(mask, base);
    }
    name.ends_with(&format!(".{mask}"))
}

/// Simple `*`/`?` wildcard match (case-insensitive, like WinRAR's masks).
pub(crate) fn wildcard_match(mask: &str, name: &str) -> bool {
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
