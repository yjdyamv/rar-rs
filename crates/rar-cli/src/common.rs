//! Shared helpers for the `rar` and `unrar` binaries.

use clap::Args;
use std::collections::HashSet;

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
/// priority: command line > RARINISWITCHES > configuration file).
/// The defaults are inserted right after the subcommand token (clap
/// subcommand-scoped options must not appear before the subcommand).
pub fn merge_default_switches(defaults: Vec<String>, cli_args: Vec<String>) -> Vec<String> {
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
    if defaults.is_empty() {
        return cli_args;
    }
    let pos = cli_args
        .iter()
        .position(|a| !a.starts_with('-'))
        .map(|p| p + 1)
        .unwrap_or(0);
    let mut merged = Vec::with_capacity(defaults.len() + cli_args.len());
    merged.extend(cli_args[..pos].iter().cloned());
    merged.extend(defaults);
    merged.extend(cli_args[pos..].iter().cloned());
    merged
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

/// The subcommand name of a raw argument list (the first token that does
/// not start with `-`), used to select `switches_<command>` entries.
pub fn command_name(raw: &[String]) -> Option<String> {
    raw.iter().skip(1).find(|a| !a.starts_with('-')).cloned()
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
    #[arg(global = true, long = "identical", value_name = "OPTS", num_args = 0..=1, default_missing_value = "", overrides_with = "identical")]
    pub identical: Option<String>,
    /// Propagate Mark of the Web from the archive to extracted files
    /// (`-om[-|1][=ext;ext]`; Windows only)
    #[arg(global = true, long = "mark-web", value_name = "OPTS", num_args = 0..=1, default_missing_value = "", overrides_with = "mark_web")]
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
    #[arg(global = true, long = "log-errors", num_args = 0..=1, default_missing_value = "", overrides_with = "log_errors")]
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
    if let Some(rest) = arg.strip_prefix("-s=") {
        return format!("--solid-params={rest}");
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
        // WinRAR: `-rr` alone means 10%, `-rrN` / `-rrN%` a percentage.
        let rest = rest.strip_suffix('%').unwrap_or(rest);
        let rest = if rest.is_empty() { "10" } else { rest };
        return format!("--recovery-percent={rest}");
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
    use super::{normalize_switch, parse_mdx_size};

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
}
