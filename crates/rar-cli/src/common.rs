//! Shared helpers for the `rar` and `unrar` binaries.

use clap::Args;
use std::collections::{HashMap, HashSet};

/// Long options that may legitimately repeat (kept out of the default
/// switch deduplication).
const REPEATABLE_LONG: &[&str] = &[
    "ts",
    "exclude",
    "include",
    "exclude-list",
    "include-list",
    "tn-filter",
    "to-filter",
    "id",
    "priority",
];

/// Long-option key of a normalized argument (`--name=value` -> `name`).
fn long_key(arg: &str) -> Option<String> {
    let rest = arg.strip_prefix("--")?;
    Some(rest.split('=').next().unwrap_or(rest).to_string())
}

/// Merge lower-priority default switches (configuration file, then
/// `RARINISWITCHES`) with the command line: single-value options given
/// on the command line suppress the same default option (WinRAR
/// priority: command line > RARINISWITCHES > configuration file), and
/// repeated single-value defaults collapse to the last one (so a
/// duplicated `-s` in a configuration source cannot fail every command).
/// The defaults are inserted right after the subcommand token (clap
/// subcommand-scoped options must not appear before the subcommand).
pub fn merge_default_switches(
    defaults: Vec<String>,
    cli_args: Vec<String>,
    value_options: &HashSet<String>,
) -> Vec<String> {
    let cli_keys: HashSet<String> = cli_args
        .iter()
        .filter_map(|a| long_key(a))
        .filter(|k| !REPEATABLE_LONG.contains(&k.as_str()))
        .collect();
    let defaults: Vec<String> = defaults
        .into_iter()
        .filter(|a| match long_key(a) {
            Some(k) if !REPEATABLE_LONG.contains(&k.as_str()) => !cli_keys.contains(&k),
            _ => true,
        })
        .collect();
    let defaults = dedupe_defaults(defaults, value_options);
    if defaults.is_empty() {
        return cli_args;
    }
    let pos = command_index(&cli_args, value_options)
        .map(|p| p + 1)
        .unwrap_or(0);
    let mut merged = Vec::with_capacity(defaults.len() + cli_args.len());
    merged.extend(cli_args[..pos].iter().cloned());
    merged.extend(defaults);
    merged.extend(cli_args[pos..].iter().cloned());
    merged
}

/// Collapse repeated single-value default switches, keeping the last
/// occurrence: `RARINISWITCHES` (appended after the configuration file)
/// wins over `rar.ini`, matching WinRAR's source priority. Repeatable
/// switches and stray values keep their order.
fn dedupe_defaults(defaults: Vec<String>, value_options: &HashSet<String>) -> Vec<String> {
    let mut spans: HashMap<String, (usize, usize)> = HashMap::new();
    let mut drop = vec![false; defaults.len()];
    let mut index = 0;
    while index < defaults.len() {
        let arg = &defaults[index];
        let mut end = index + 1;
        if let Some(key) = long_key(arg)
            && !REPEATABLE_LONG.contains(&key.as_str())
        {
            if !arg.contains('=') && value_options.contains(&format!("--{key}")) {
                end = (index + 2).min(defaults.len());
            }
            if let Some((start, stop)) = spans.insert(key, (index, end)) {
                for slot in &mut drop[start..stop] {
                    *slot = true;
                }
            }
        }
        index = end;
    }
    defaults
        .into_iter()
        .enumerate()
        .filter_map(|(index, arg)| (!drop[index]).then_some(arg))
        .collect()
}

/// Parse a WinRAR `-mdx<size>[k|m|g]` extraction dictionary cap: unlike
/// `-md`, no unit means **GiB** (`-mdx8` = 8 GiB, per the WinRAR docs).
#[allow(dead_code)] // used by the `unrar` binary only
pub fn parse_mdx_size(s: &str) -> Result<u64, String> {
    let (num, mult) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1024u64),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1024 * 1024 * 1024),
    };
    num.parse::<u64>()
        .map_err(|_| format!("invalid dictionary size: {s}"))?
        .checked_mul(mult)
        .ok_or_else(|| format!("dictionary size is too large: {s}"))
}

/// Parse a byte size with an optional binary `k`/`m`/`g`/`t` suffix
/// (case-insensitive; `t` is 2^40). No suffix means bytes. A `0` is accepted
/// here — callers that need a positive size reject it themselves.
///
/// Backs the `-v` / `--size-less` / `--size-more` switches and the extraction
/// size guards (`--max-unpacked` / `--max-total-unpacked`). The official
/// command-line tools have no equivalent of those guards, so they are
/// long-option only rather than WinRAR `-` switches.
pub fn parse_byte_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    // Slicing off the last byte is sound because every suffix arm matched an
    // ASCII character, so `s.len() - 1` is a char boundary in those arms.
    let (num, mult) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1024u64),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        Some('t') | Some('T') => (&s[..s.len() - 1], 1024 * 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .map_err(|_| format!("invalid size: {s}"))?
        .checked_mul(mult)
        .ok_or_else(|| format!("size is too large: {s}"))
}

/// Long options that may consume a separate value token, collected from
/// the whole command tree (root and every subcommand). Options that demand
/// `=` (like `-hp`/`-ad`) never take the following token, so they are not
/// listed.
pub fn value_options(cmd: &clap::Command) -> HashSet<String> {
    fn walk(cmd: &clap::Command, out: &mut HashSet<String>) {
        for arg in cmd.get_arguments() {
            if arg.get_action().takes_values()
                && !arg.is_require_equals_set()
                && let Some(long) = arg.get_long()
            {
                out.insert(format!("--{long}"));
            }
        }
        for sub in cmd.get_subcommands() {
            walk(sub, out);
        }
    }
    let mut out = HashSet::new();
    walk(cmd, &mut out);
    out
}

/// Every subcommand name and alias of `cmd`, used to decide whether a
/// token can be a command when reordering switches that precede it.
pub fn subcommand_names(cmd: &clap::Command) -> HashSet<String> {
    let mut out = HashSet::new();
    for sub in cmd.get_subcommands() {
        out.insert(sub.get_name().to_string());
        out.extend(sub.get_all_aliases().map(str::to_string));
    }
    out
}

/// Index of the subcommand token: WinRAR's first argument that is neither
/// a switch nor a switch value. Values of value-taking long options are
/// skipped, so `--work-dir . a` still lands on `a` and not on `.`.
pub fn command_index(args: &[String], value_options: &HashSet<String>) -> Option<usize> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return None;
        }
        if let Some(name) = arg.strip_prefix("--") {
            if !name.contains('=') && value_options.contains(&format!("--{name}")) {
                index += 1;
            }
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(index);
    }
    None
}

/// The subcommand name of a normalized argument list (without the program
/// name), used to select `switches_<command>` entries.
pub fn command_name(args: &[String], value_options: &HashSet<String>) -> Option<String> {
    command_index(args, value_options).map(|index| args[index].clone())
}

/// Move switches given before the subcommand behind it, matching WinRAR's
/// order-independent switch parsing (`rar -m5 -s a book.rar`). Root-level
/// switches (already accepted before a subcommand) stay where they are so
/// a switch repeated on both sides keeps its old, tolerated behavior;
/// option values stay attached to their switch. Commands this binary does
/// not define (external `i<string>`/`rv<N>` forms, a bare archive path)
/// are left untouched: their remaining arguments are not parsed as
/// switches.
pub fn switches_after_command(args: Vec<String>, cmd: &clap::Command) -> Vec<String> {
    drop_undeclared(reorder_switches(args, cmd), cmd)
}

/// The reordering half of [`switches_after_command`].
fn reorder_switches(args: Vec<String>, cmd: &clap::Command) -> Vec<String> {
    let value_options = value_options(cmd);
    let root: HashSet<String> = cmd
        .get_arguments()
        .filter_map(|arg| arg.get_long().map(|long| format!("--{long}")))
        .collect();
    let Some(index) = command_index(&args, &value_options) else {
        return args;
    };
    if index == 0 || !subcommand_names(cmd).contains(args[index].as_str()) {
        return args;
    }
    let mut root_prefix = Vec::new();
    let mut moved = Vec::new();
    let mut cursor = 0;
    while cursor < index {
        let arg = &args[cursor];
        let key = long_key(arg);
        let takes_value = key
            .as_ref()
            .is_some_and(|key| !arg.contains('=') && value_options.contains(&format!("--{key}")));
        let end = if takes_value {
            (cursor + 2).min(index)
        } else {
            cursor + 1
        };
        let belongs_to_root = key
            .as_ref()
            .is_some_and(|key| root.contains(&format!("--{key}")));
        if belongs_to_root {
            root_prefix.extend_from_slice(&args[cursor..end]);
        } else {
            moved.extend_from_slice(&args[cursor..end]);
        }
        cursor = end;
    }
    let mut reordered = root_prefix;
    reordered.push(args[index].clone());
    reordered.extend(moved);
    reordered.extend(args[index + 1..].iter().cloned());
    reordered
}

/// Drop translated switches the target subcommand does not declare.
///
/// WinRAR's parser accepts every known switch on every command and simply
/// ignores the ones that command does not use (`rar l -m5` lists the archive;
/// `-m5` means nothing there). Clap is strict per command, so without this an
/// irrelevant-but-valid switch was a hard usage error — the parser laxity is
/// one of the few places where copying WinRAR is the *only* compatible
/// behavior, and it is the deliberate exception to this CLI's rule that a
/// switch is never silently dropped.
///
/// Only `--name[=value]` tokens are considered, and each is self-contained
/// (every translation attaches its value with `=`), so dropping one never
/// strands a following token. Non-switch arguments — the archive path, member
/// names, selectors — are never touched. The switches this CLI rejects on
/// purpose (`-dr`, `-dw`, `-vd`) are declared where they would apply, so they
/// still reach their explicit refusal.
fn drop_undeclared(args: Vec<String>, cmd: &clap::Command) -> Vec<String> {
    let value_options = value_options(cmd);
    let Some(index) = command_index(&args, &value_options) else {
        return args;
    };
    // The command token may be the first argument (`rar l -m5`), which is the
    // common case: unlike the reordering pass there is nothing to move, but
    // everything to check.
    if !subcommand_names(cmd).contains(args[index].as_str()) {
        return args;
    }
    // Only the subcommand's own arguments and the root's *global* switches
    // are legal after the command token; a root-level non-global option
    // (`--volume-size`) written after the command is rejected by clap, and
    // WinRAR would have ignored it on a command that does not use it.
    let mut allowed: HashSet<String> = cmd
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .filter_map(|arg| arg.get_long().map(|long| format!("--{long}")))
        .collect();
    if let Some(sub) = cmd.find_subcommand(&args[index]) {
        allowed.extend(
            sub.get_arguments()
                .filter_map(|arg| arg.get_long().map(|long| format!("--{long}"))),
        );
    }
    // Markers this CLI consumes before clap ever parses: a bare `-p` becomes
    // `--password-prompt`, which `password::reject_bare_password` inspects to
    // refuse an unencrypted archive. Clap has no such option, so it must not
    // be filtered away.
    allowed.insert("--password-prompt".to_string());
    args.into_iter()
        .filter(|arg| match long_key(arg) {
            Some(key) => allowed.contains(&format!("--{key}")),
            None => true,
        })
        .collect()
}

/// Move a normalized switch block that precedes an external subcommand token
/// (`i<string>`, `rv<N>`) behind it. Clap only knows the declared
/// subcommands, so `rar -p123 iREADME arc` would otherwise be rejected as a
/// root option while the external handlers expect their switches after the
/// token. Any non-switch token before the external one (a real subcommand)
/// leaves the arguments untouched.
///
/// The `unrar` binary shares this module but has no external commands.
#[allow(dead_code)]
pub fn switches_after_external_command(args: Vec<String>) -> Vec<String> {
    let is_external = |arg: &String| -> bool {
        (arg.len() > 1 && arg.starts_with('i'))
            || (arg.len() > 2
                && arg.starts_with("rv")
                && arg[2..].chars().all(|c| c.is_ascii_digit() || c == '%'))
    };
    let Some(index) = args.iter().position(is_external) else {
        return args;
    };
    if index == 0 || !args[..index].iter().all(|arg| arg.starts_with("--")) {
        return args;
    }
    let mut reordered = Vec::with_capacity(args.len());
    reordered.push(args[index].clone());
    reordered.extend(args[..index].iter().cloned());
    reordered.extend(args[index + 1..].iter().cloned());
    reordered
}

/// Read the configuration file (`rar.ini` next to the executable on
/// Windows, `~/.rarrc` on Unix) and return the `switches` /
/// `switches_<command>` entries (raw, unnormalized).
fn config_file_switches(command: Option<&str>) -> Vec<String> {
    let path: Option<std::path::PathBuf> = {
        #[cfg(windows)]
        {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|d| d.join("rar.ini")))
        }
        #[cfg(unix)]
        {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".rarrc"))
        }
        #[cfg(not(any(windows, unix)))]
        {
            None
        }
    };
    let Some(path) = path else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut global = Vec::new();
    let mut specific = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let switches: Vec<String> = value.split_whitespace().map(|s| s.to_string()).collect();
        if key.trim() == "switches" {
            global = switches;
        } else if let Some(rest) = key.trim().strip_prefix("switches_")
            && Some(rest) == command
        {
            specific = switches;
        }
    }
    global.into_iter().chain(specific).collect()
}

/// Default switches for a run: configuration file entries plus the
/// `RARINISWITCHES` environment variable (raw, unnormalized), with
/// `-cfg-` disabling both.
pub fn default_switches(command: Option<&str>, no_config: bool) -> Vec<String> {
    if no_config {
        return Vec::new();
    }
    let mut out = config_file_switches(command);
    if let Some(env) = std::env::var_os("RARINISWITCHES") {
        out.extend(
            env.to_string_lossy()
                .split_whitespace()
                .map(|s| s.to_string()),
        );
    }
    out
}
/// Lower-value WinRAR switches accepted for CLI parity. Most are no-ops
/// in this implementation (platform-specific or informational); a few
/// are wired in `rar`/`unrar`:
/// - `owner` (`-ow`): save owner/group on Unix (numeric ids)
/// - `ts_preserve` (`-tsp`): restore source access times on Unix
/// - `log_errors` (`-ilog`): append errors to a log file
#[derive(Args, Default)]
pub struct MiscSwitches {
    /// Message detail flags (`-idc`/`-idd`/`-idn`/`-idp`; accepted,
    /// `-idq` is the only effective message switch here)
    #[arg(global = true, long = "id", value_name = "FLAG", action = clap::ArgAction::Append)]
    #[allow(dead_code)]
    pub id_flags: Vec<String>,
    /// Clear the Archive attribute after archiving (`-ac`; Windows-only)
    #[arg(global = true, long = "clear-attr")]
    #[allow(dead_code)]
    pub clear_attr: bool,
    /// Ignore file attributes (`-ai`; we never set them on extract)
    #[arg(global = true, long = "ignore-attr")]
    #[allow(dead_code)]
    pub ignore_attr: bool,
    /// Exclude/include attribute mask (`-e[+]<attr>`; Windows-only)
    #[arg(
        global = true,
        long = "exclude-attrs",
        value_name = "MASK",
        overrides_with = "exclude_attrs"
    )]
    #[allow(dead_code)]
    pub exclude_attrs: Option<String>,
    /// Save NTFS streams (`-os`; Windows-only)
    #[arg(global = true, long = "save-streams")]
    pub save_streams: bool,
    /// Only add files with the Archive attribute set (`-ao`; Windows-only,
    /// accepted)
    #[arg(global = true, long = "archive-attr")]
    #[allow(dead_code)]
    pub archive_attr: bool,
    /// Set the NTFS Compressed attribute on extracted files (`-oc`;
    /// Windows-only, accepted)
    #[arg(global = true, long = "ntfs-compressed")]
    #[allow(dead_code)]
    pub ntfs_compressed: bool,
    /// Use large memory pages (`-mlp`; accepted)
    #[arg(global = true, long = "large-pages")]
    #[allow(dead_code)]
    pub large_pages: bool,
    /// Open shared files (`-dh`; accepted)
    #[arg(global = true, long = "shared-files")]
    #[allow(dead_code)]
    pub shared_files: bool,
    /// Charset for list files (`-sc<charset>l`; accepted)
    #[arg(
        global = true,
        long = "charset",
        value_name = "SET",
        overrides_with = "charset"
    )]
    #[allow(dead_code)]
    pub charset: Option<String>,
    /// Use CRC32 hash records (`-htc`; the default, accepted on every
    /// command like WinRAR)
    #[arg(global = true, long = "hash-crc")]
    #[allow(dead_code)]
    pub hash_crc: bool,
    /// Disable / enable `@listfile` processing (`-@` / `-@+`)
    #[arg(
        global = true,
        long = "list-files",
        value_name = "+",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "",
        overrides_with = "list_files"
    )]
    pub list_files: Option<String>,
    /// Allow potentially incompatible names (`-oni`; Windows-only)
    #[arg(global = true, long = "allow-names")]
    #[allow(dead_code)]
    pub allow_names: bool,
    /// Task priority and sleep (`-ri<P>[:<S>]`; Windows-only)
    #[arg(
        global = true,
        long = "priority",
        value_name = "P[:S]",
        overrides_with = "priority"
    )]
    #[allow(dead_code)]
    pub priority: Option<String>,
    /// Pause before each volume (`-vp`; no interactive prompts here)
    #[arg(global = true, long = "pause-volumes")]
    #[allow(dead_code)]
    pub pause_volumes: bool,
    /// Erase disk contents before creating volume (`-vd`; removable
    /// media only, never touched)
    #[arg(global = true, long = "erase-disk")]
    #[allow(dead_code)]
    pub erase_disk: bool,
    /// Save identical files as references (`-oi[0-4][:<minsize>]`; create
    /// side; accepted as a no-op on extraction)
    #[arg(global = true, long = "identical", value_name = "OPTS", num_args = 0..=1, default_missing_value = "", require_equals = true, overrides_with = "identical")]
    pub identical: Option<String>,
    /// Propagate Mark of the Web from the archive to extracted files
    /// (`-om[-|1][=ext;ext]`; Windows only)
    #[arg(global = true, long = "mark-web", value_name = "OPTS", num_args = 0..=1, default_missing_value = "", require_equals = true, overrides_with = "mark_web")]
    pub mark_web: Option<String>,
    /// Encryption parameters (`-me<par>`; accepted)
    #[arg(
        global = true,
        long = "me",
        value_name = "PAR",
        overrides_with = "me_params"
    )]
    #[allow(dead_code)]
    pub me_params: Option<String>,
    /// Skip symbolic links when archiving or extracting (`-ol-`)
    #[arg(global = true, long = "skip-links")]
    pub skip_links: bool,
    /// Extract links with dangerous targets as-is (`-ola`; disables the
    /// link safety checks)
    #[arg(global = true, long = "unsafe-links")]
    pub unsafe_links: bool,
    /// Freshen files (`-f`; `a -f` is equivalent to the `f` command)
    #[arg(global = true, long = "freshen")]
    pub freshen: bool,
    /// Update files (`-u`; `a -u` is equivalent to the `u` command)
    #[arg(global = true, long = "update-files")]
    pub update_files: bool,
    /// Lock the archive (`-k`)
    #[arg(global = true, long = "lock")]
    pub lock: bool,
    /// How the solid chain splits (`-sd`/`-sv`/`-se`, also `-s=d`/`-s=v`/
    /// `-s=e`): `continuous` keeps the statistics across the whole archive,
    /// `volume` resets them at each volume boundary and `extension` resets
    /// them when the member's file extension changes. Any of them enables
    /// solid mode; `off` is the unset default (`-s`/`-s=` are the only other
    /// enablers, so a bare `-sd` must not collapse into the default).
    /// Accepted on every command, like WinRAR's parser.
    #[arg(
        global = true,
        long = "solid-reset",
        value_name = "MODE",
        default_value = "off",
        value_parser = ["off", "continuous", "volume", "extension"]
    )]
    pub solid_reset: String,
    /// Read the comment from a file (`-z<file>`; a bare `-z` reads stdin).
    /// Used by the comment and create commands, accepted and ignored
    /// elsewhere
    #[arg(global = true, long = "comment-file", value_name = "FILE")]
    #[allow(dead_code)]
    pub comment_file: Option<String>,
    /// Write archive/file names to a log file (`-log[AFPU]*[=name]`; rar
    /// only — UnRAR rejects the switch like the official one)
    #[arg(global = true, long = "log", value_name = "SPEC", action = clap::ArgAction::Append)]
    pub log_specs: Vec<String>,
    /// Archive metadata save/restore (`-am[s,r]`; accepted)
    #[arg(
        global = true,
        long = "archive-meta",
        value_name = "SPEC",
        overrides_with = "archive_meta"
    )]
    #[allow(dead_code)]
    pub archive_meta: Option<String>,
    /// Log errors to a file (`-ilog[name]`; default `rar.log`)
    #[arg(global = true, long = "log-errors", num_args = 0..=1, default_missing_value = "", require_equals = true, overrides_with = "log_errors")]
    pub log_errors: Option<String>,
    /// File version control (`-ver[n]`; keep old versions on update)
    #[arg(global = true, long = "version-control", value_name = "N")]
    pub version_control: Option<String>,
    /// Save owner/group on Unix (`-ow`)
    #[arg(global = true, long = "owner")]
    pub owner: bool,
    /// Preserve the source files' access time when archiving (`-tsp`)
    #[arg(global = true, long = "ts-preserve")]
    pub ts_preserve: bool,
    /// Display the version and quit (`-iver`)
    #[arg(global = true, long = "version-info")]
    pub version_info: bool,
    /// Ignore the configuration file and RARINISWITCHES (`-cfg-`)
    #[arg(global = true, long = "no-config")]
    #[allow(dead_code)]
    pub no_config: bool,
    /// Send archive by email (`-ieml[.][addr]`; never performed)
    #[arg(global = true, long = "email", value_name = "ADDR")]
    #[allow(dead_code)]
    pub email: Option<String>,
    /// Turn the PC off after the operation (`-ioff[n]`; never performed)
    #[arg(global = true, long = "power-off", value_name = "N")]
    #[allow(dead_code)]
    pub power_off: Option<String>,
    /// Notification sounds (`-isnd[-]`; no sounds are played)
    #[arg(global = true, long = "sound", value_name = "FLAG")]
    #[allow(dead_code)]
    pub sound: Option<String>,
}
/// Normalize rar-style switches (`-htb`, `-ep1`, `-m3`, `-ap<path>`, ...)
/// into clap long options. clap short flags are single characters, so the
/// multi-character rar forms are mapped here; the single-character forms
/// (`-m`, `-p`, `-v`, `-s`) keep their rar spelling via `short`.
/// Used only by the `rar` binary.
#[allow(dead_code)]
pub fn normalize_switch(arg: &str) -> String {
    if let Some(rest) = arg.strip_prefix("-mt") {
        return format!("--threads={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-ms") {
        return format!("--store-types={rest}");
    }
    if arg == "-df" {
        return "--delete-after".into();
    }
    if arg == "-t" {
        return "--test-after".into();
    }
    if let Some(rest) = arg.strip_prefix("-ep4") {
        return format!("--exclude-prefix={rest}");
    }
    if arg == "-as" {
        return "--sync-archive".into();
    }
    if arg == "-ds" {
        return "--no-sort".into();
    }
    // WinRAR accepts the reset modes both bare (`-se`) and with an equals
    // sign (`-s=e`); other `-s=` values stay solid parameters.
    if let Some(rest) = arg.strip_prefix("-s=") {
        return match rest {
            "d" => "--solid-reset=continuous".into(),
            "v" => "--solid-reset=volume".into(),
            "e" => "--solid-reset=extension".into(),
            other => format!("--solid-params={other}"),
        };
    }
    // WinRAR `-s` modifiers that split the solid compression chain.
    if arg == "-se" {
        return "--solid-reset=extension".into();
    }
    if arg == "-sv" {
        return "--solid-reset=volume".into();
    }
    if arg == "-sd" {
        return "--solid-reset=continuous".into();
    }
    if arg == "-so" {
        return "--stdout".into();
    }
    if arg == "-htc" {
        return "--hash-crc".into();
    }
    if let Some(rest) = arg.strip_prefix("-mcl") {
        return format!("--long-match={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-mc") {
        return format!("--mc={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-me") {
        return format!("--me={rest}");
    }
    if arg == "-ao" {
        return "--archive-attr".into();
    }
    if arg == "-oc" {
        return "--ntfs-compressed".into();
    }
    if arg == "-mlp" {
        return "--large-pages".into();
    }
    if arg == "-dh" {
        return "--shared-files".into();
    }
    if arg == "-dr" {
        return "--recycle-bin".into();
    }
    if arg == "-dw" {
        return "--wipe".into();
    }
    if let Some(rest) = arg.strip_prefix("-mdx") {
        return format!("--dict-extract={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-ma") {
        return format!("--archive-format={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-md") {
        return format!("--dict-size={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-m") {
        return if rest.is_empty() {
            "--level".into()
        } else {
            format!("--level={rest}")
        };
    }
    if let Some(rest) = arg.strip_prefix("-p") {
        return if rest.is_empty() {
            // The caller rejects this form unless secure no-echo prompting is
            // available. Keep it distinct from `-p-` during normalization.
            "--password-prompt".into()
        } else if rest == "-" {
            // `-p-` explicitly disables password use.
            "--password=".into()
        } else {
            format!("--password={rest}")
        };
    }
    if let Some(rest) = arg.strip_prefix("-ver") {
        return format!("--version-control={rest}");
    }
    if arg == "-vp" {
        return "--pause-volumes".into();
    }
    if arg == "-vd" {
        return "--erase-disk".into();
    }
    if arg == "-v-" {
        // WinRAR cancels volume creation with `-v-`; volumes are off by
        // default here, so this only has to clear a requested size.
        return "--no-volumes".into();
    }
    if let Some(rest) = arg.strip_prefix("-v") {
        return if rest.is_empty() {
            "--volume-size".into()
        } else {
            format!("--volume-size={rest}")
        };
    }
    if arg == "-s" {
        return "--solid".into();
    }
    if arg == "-htb" {
        return "--blake2".into();
    }
    if let Some(rest) = arg.strip_prefix("-qo") {
        return match rest {
            "+" => "--quick-open".into(),
            "-" => "--no-quick-open".into(),
            _ => "--quick-open".into(),
        };
    }
    if let Some(rest) = arg.strip_prefix("-hp") {
        return if rest.is_empty() {
            "--header-encrypt".into()
        } else {
            format!("--header-encrypt={rest}")
        };
    }
    if let Some(rest) = arg.strip_prefix("-rr") {
        // `-rr` alone (and `-rr%`) means WinRAR's default 3 percent. `-rrN%`
        // is a percentage; a bare `-rrN` is a legacy RAR4 parity-sector count
        // (the record's native unit), which `create` turns into a percentage
        // for RAR5, whose record is sized by percent only. Both are measured
        // against 6.23: `-rr10` writes exactly 10 RAR4 parity sectors at any
        // size, and bare `-rr` matches `-rr3%` for both formats.
        match rest.strip_suffix('%') {
            Some("") => return "--recovery-percent=3".into(),
            Some(percent) => return format!("--recovery-percent={percent}"),
            None => {}
        }
        if rest.is_empty() {
            return "--recovery-percent=3".into();
        }
        return format!("--recovery-sectors={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-rv") {
        return format!("--recovery-volumes={rest}");
    }
    if arg == "-ep" {
        return "--basename-only".into();
    }
    if arg == "-ep1" {
        return "--exclude-base-dir".into();
    }
    if arg == "-ep2" {
        return "--full-paths".into();
    }
    if arg == "-ep3" {
        return "--full-paths-drive".into();
    }
    if arg == "-r" {
        return "--recurse".into();
    }
    if arg == "-r0" {
        return "--recurse-zero".into();
    }
    if arg == "-r-" {
        return "--no-recurse".into();
    }
    if arg == "-cl" {
        return "--lowercase".into();
    }
    if arg == "-cu" {
        return "--uppercase".into();
    }
    if let Some(rest) = arg.strip_prefix("-ap") {
        return format!("--archive-path={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-x@") {
        return format!("--exclude-list={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-n@") {
        return format!("--include-list={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-x") {
        return format!("--exclude={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-n") {
        return format!("--include={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-sfx") {
        return format!("--sfx-module={rest}");
    }
    if arg == "-y" {
        return "--yes".into();
    }
    if arg == "-idq" || arg == "-inul" {
        return "--quiet".into();
    }
    if arg == "-ierr" {
        return "--err".into();
    }
    if arg == "-iver" {
        return "--version-info".into();
    }
    if arg == "-cfg-" {
        return "--no-config".into();
    }
    if let Some(rest) = arg.strip_prefix("-ieml") {
        return format!("--email={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-ioff") {
        return format!("--power-off={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-isnd") {
        return format!("--sound={rest}");
    }
    if arg == "-idc" {
        return "--id=c".into();
    }
    if arg == "-idd" {
        return "--id=d".into();
    }
    if arg == "-idn" {
        return "--id=n".into();
    }
    if arg == "-idp" {
        return "--id=p".into();
    }
    if arg == "-@" {
        return "--list-files=".into();
    }
    if arg == "-@+" {
        return "--list-files=+".into();
    }
    if arg == "-ac" {
        return "--clear-attr".into();
    }
    if arg == "-ai" {
        return "--ignore-attr".into();
    }
    if arg == "-os" {
        return "--save-streams".into();
    }
    if let Some(rest) = arg.strip_prefix("-sc") {
        return format!("--charset={rest}");
    }
    if arg == "-oni" {
        return "--allow-names".into();
    }
    if let Some(rest) = arg.strip_prefix("-ri") {
        return format!("--priority={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-oi") {
        return format!("--identical={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-om") {
        return format!("--mark-web={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-log") {
        return format!("--log={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-am") {
        return format!("--archive-meta={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-ilog") {
        return format!("--log-errors={rest}");
    }
    if arg == "-ow" {
        return "--owner".into();
    }
    if let Some(rest) = arg.strip_prefix("-w") {
        return format!("--work-dir={rest}");
    }
    if arg == "-o+" {
        return "--overwrite=always".into();
    }
    if arg == "-o-" {
        return "--overwrite=never".into();
    }
    if arg == "-or" {
        return "--auto-rename".into();
    }
    if arg == "-kb" {
        return "--keep-broken".into();
    }
    if let Some(rest) = arg.strip_prefix("-op") {
        return format!("--output-path={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-ta") {
        return format!("--after={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-tb") {
        return format!("--before={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-tn") {
        return format!("--tn-filter={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-to") {
        return format!("--to-filter={rest}");
    }
    if arg == "-tk" {
        return "--keep-time".into();
    }
    if let Some(rest) = arg.strip_prefix("-tk") {
        // `-tk<date>` sets the archive time.
        return format!("--keep-time={rest}");
    }
    if arg == "-tl" {
        return "--set-latest-time".into();
    }
    if arg == "-tsp" {
        return "--ts-preserve".into();
    }
    if let Some(rest) = arg.strip_prefix("-ts") {
        return format!("--ts={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-ag") {
        return if rest.is_empty() {
            "--auto-name".into()
        } else {
            format!("--auto-name={rest}")
        };
    }
    if arg == "-ol-" {
        return "--skip-links".into();
    }
    if arg == "-ola" {
        return "--unsafe-links".into();
    }
    if arg == "-ol" {
        return "--links".into();
    }
    if arg == "-oh" {
        return "--hardlinks".into();
    }
    if arg == "-f" {
        return "--freshen".into();
    }
    if arg == "-u" {
        return "--update-files".into();
    }
    if arg == "-k" {
        return "--lock".into();
    }
    if let Some(rest) = arg.strip_prefix("-sl") {
        return format!("--size-less={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-sm") {
        return format!("--size-more={rest}");
    }
    if arg == "-ed" {
        return "--no-empty-dirs".into();
    }
    if let Some(rest) = arg.strip_prefix("-e") {
        return format!("--exclude-attrs={rest}");
    }

    if arg == "-c-" {
        return "--no-comment".into();
    }
    if arg == "-ad" {
        return "--append-dir".into();
    }
    if arg == "-ad1" {
        return "--append-dir=1".into();
    }
    if arg == "-ad2" {
        return "--append-dir=2".into();
    }
    if let Some(rest) = arg.strip_prefix("-si") {
        return format!("--stdin-name={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-z") {
        return format!("--comment-file={rest}");
    }
    arg.to_string()
}

/// Parse the normalized `--mark-web` value (`-om[-|1][=ext;ext]`), or `None`
/// when the switch is absent.
pub fn mark_web(spec: Option<&str>) -> Result<Option<rar_rs::MarkOfTheWeb>, String> {
    match spec {
        Some(spec) => parse_mark_web(spec),
        None => Ok(None),
    }
}

/// Parse the normalized `--mark-web` value (`-om[-|1][=ext;ext]`).
///
/// Returns `Ok(None)` for the off form (`-om-`); the extension list, when
/// present, is lowercased and stripped of leading dots.
pub fn parse_mark_web(spec: &str) -> Result<Option<rar_rs::MarkOfTheWeb>, String> {
    let (flags, list) = match spec.split_once('=') {
        Some((flags, list)) => (flags, Some(list)),
        None => (spec, None),
    };
    let all_fields = match flags {
        "" => false,
        "1" => true,
        "-" => return Ok(None),
        other => return Err(format!("Unknown option: om{other}")),
    };
    let extensions = list.and_then(|list| {
        let extensions: Vec<String> = list
            .split(';')
            .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|ext| !ext.is_empty())
            .collect();
        (!extensions.is_empty()).then_some(extensions)
    });
    Ok(Some(rar_rs::MarkOfTheWeb {
        all_fields,
        extensions,
    }))
}

/// Print an informational message unless quiet mode is on (errors and
/// requested output — listings, prints — always print). With `-ierr` the
/// message goes to stderr.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        if !$crate::output::QUIET.load(std::sync::atomic::Ordering::Relaxed) {
            if $crate::output::ERR.load(std::sync::atomic::Ordering::Relaxed) {
                eprintln!($($arg)*);
            } else {
                println!($($arg)*);
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::{
        command_name, merge_default_switches, normalize_switch, parse_mdx_size,
        switches_after_command, value_options,
    };
    use std::collections::HashSet;

    #[test]
    fn mcl_is_normalized_before_the_shorter_mc_prefix() {
        assert_eq!(normalize_switch("-mcl"), "--long-match=");
        assert_eq!(normalize_switch("-mcl123"), "--long-match=123");
        assert_eq!(normalize_switch("-mc123"), "--mc=123");
    }

    #[test]
    fn password_switch_forms_remain_distinct() {
        assert_eq!(normalize_switch("-p"), "--password-prompt");
        assert_eq!(normalize_switch("-p-"), "--password=");
        assert_eq!(normalize_switch("-psecret"), "--password=secret");
    }

    #[test]
    fn mdx_size_parsing_checks_multiplication_overflow() {
        assert_eq!(parse_mdx_size("2k"), Ok(2 * 1024));
        assert!(parse_mdx_size("18446744073709551615g").is_err());
    }

    #[test]
    fn attached_si_value_normalizes() {
        assert_eq!(normalize_switch("-sidata.bin"), "--stdin-name=data.bin");
        assert_eq!(normalize_switch("-si"), "--stdin-name=");
        assert_eq!(normalize_switch("-@"), "--list-files=");
        assert_eq!(normalize_switch("-@+"), "--list-files=+");
    }

    #[test]
    fn command_scan_skips_option_values() {
        let cmd = clap::Command::new("t")
            .arg(
                clap::Arg::new("work")
                    .long("work-dir")
                    .action(clap::ArgAction::Set),
            )
            .subcommand(
                clap::Command::new("a").arg(
                    clap::Arg::new("level")
                        .long("level")
                        .action(clap::ArgAction::Set),
                ),
            );
        let options = value_options(&cmd);
        let args: Vec<String> = ["--work-dir", ".", "--level", "5", "a", "arc.rar"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(command_name(&args, &options).as_deref(), Some("a"));
        // Root switches stay before the command; subcommand switches move
        // behind it with their split values attached.
        assert_eq!(
            switches_after_command(args, &cmd),
            ["--work-dir", ".", "a", "--level", "5", "arc.rar"]
        );
    }

    #[test]
    fn default_switches_dedupe_keeps_the_last_value() {
        let options = HashSet::new();
        let merged = merge_default_switches(
            vec!["--level=1".into(), "--level=5".into()],
            vec!["a".into(), "arc.rar".into()],
            &options,
        );
        assert_eq!(merged, ["a", "--level=5", "arc.rar"]);

        let options: HashSet<String> = ["--password".to_string()].into_iter().collect();
        let merged = merge_default_switches(
            vec![
                "--password".into(),
                "old".into(),
                "--password".into(),
                "new".into(),
            ],
            vec!["a".into(), "arc.rar".into()],
            &options,
        );
        assert_eq!(merged, ["a", "--password", "new", "arc.rar"]);
    }
}
