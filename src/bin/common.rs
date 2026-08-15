//! Shared helpers for the `rar` and `unrar` binaries.

use clap::Args;

/// Common `-p<password>` argument shared by every command.
#[derive(Args)]
pub struct PasswordArgs {
    /// Archive password (empty with bare `-p`)
    #[arg(
        short = 'p',
        long,
        global = true,
        value_name = "PASSWORD",
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub password: Option<String>,
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
    #[arg(global = true, long = "exclude-attrs", value_name = "MASK")]
    #[allow(dead_code)]
    pub exclude_attrs: Option<String>,
    /// Save NTFS streams (`-os`; not supported)
    #[arg(global = true, long = "save-streams")]
    #[allow(dead_code)]
    pub save_streams: bool,
    /// Charset for list files (`-sc<charset>l`; accepted)
    #[arg(global = true, long = "charset", value_name = "SET")]
    #[allow(dead_code)]
    pub charset: Option<String>,
    /// Allow potentially incompatible names (`-oni`; Windows-only)
    #[arg(global = true, long = "allow-names")]
    #[allow(dead_code)]
    pub allow_names: bool,
    /// Task priority and sleep (`-ri<P>[:<S>]`; Windows-only)
    #[arg(global = true, long = "priority", value_name = "P[:S]")]
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
    /// Stream options (`-oi[1|-]`; not supported)
    #[arg(global = true, long = "stream-options", value_name = "OPTS")]
    #[allow(dead_code)]
    pub stream_options: Option<String>,
    /// Archive metadata save/restore (`-am[s,r]`; accepted)
    #[arg(global = true, long = "archive-meta", value_name = "SPEC")]
    #[allow(dead_code)]
    pub archive_meta: Option<String>,
    /// Log errors to a file (`-ilog[name]`; default `rar.log`)
    #[arg(global = true, long = "log-errors", num_args = 0..=1, default_missing_value = "")]
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
    /// Ignore configuration file and RARINISWITCHES (`-cfg-`; no config
    /// files are read anyway)
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
    if let Some(rest) = arg.strip_prefix("-mdx") {
        return format!("--dict-extract={rest}");
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
            "--password".into()
        } else if rest == "-" {
            // `-p-` means "no password" in WinRAR (unlike bare `-p`, which
            // encrypts with an empty/prompted password).
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
    if arg == "-qo" {
        return "--quick-open".into();
    }
    if let Some(rest) = arg.strip_prefix("-hp") {
        return if rest.is_empty() {
            "--header-encrypt".into()
        } else {
            format!("--header-encrypt={rest}")
        };
    }
    if let Some(rest) = arg.strip_prefix("-rr") {
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
        return format!("--stream-options={rest}");
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
    if arg == "-ol" {
        return "--links".into();
    }
    if arg == "-oh" {
        return "--hardlinks".into();
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
    if arg == "-p-" {
        return "--password=".into();
    }
    if arg == "-c-" {
        return "--no-comment".into();
    }
    if arg == "-ad" {
        return "--append-dir".into();
    }
    if let Some(rest) = arg.strip_prefix("-si") {
        return format!("--stdin-name={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-z") {
        return format!("--comment-file={rest}");
    }
    arg.to_string()
}

/// Suppresses informational messages when `-idq` / `-inul` is given.
pub static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Sends informational messages to stderr instead of stdout when `-ierr`
/// is given.
pub static ERR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Print an informational message unless quiet mode is on (errors and
/// requested output — listings, prints — always print). With `-ierr` the
/// message goes to stderr.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        if !$crate::common::QUIET.load(std::sync::atomic::Ordering::Relaxed) {
            if $crate::common::ERR.load(std::sync::atomic::Ordering::Relaxed) {
                eprintln!($($arg)*);
            } else {
                println!($($arg)*);
            }
        }
    };
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Destination directory, honoring `-ad` (append the archive base name as
/// a subdirectory; `.partN` volume suffixes are stripped).
pub fn extract_dest(dest: &str, archive: &str, append_dir: bool) -> std::path::PathBuf {
    let dest = std::path::PathBuf::from(dest);
    if !append_dir {
        return dest;
    }
    let mut base = std::path::Path::new(archive)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    if let Some(idx) = base.to_lowercase().find(".part")
        && base[idx + 5..].chars().all(|c| c.is_ascii_digit())
    {
        base.truncate(idx);
    }
    dest.join(base)
}

/// Print a verbose listing (like `rar v` / `unrar v`).
pub fn print_verbose_list(rar: &rar5::RarArchive) -> Result<(), String> {
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

/// Parsed `-ts` settings: which times to save and at what precision.
#[derive(Clone, Copy, Default)]
pub struct TsSettings {
    pub save_mtime: bool,
    pub save_ctime: bool,
    pub save_atime: bool,
    pub precision_seconds: bool,
}

/// Parse repeatable `-ts[m,c,a][+,-,1]` specs with WinRAR semantics:
/// a bare `-ts` (or no kinds) selects all three times; `-` omits a time,
/// `1` selects 1-second precision, `+` high precision (default). All
/// times of a member share one precision (`+` wins over `1`).
pub fn parse_ts_specs(specs: &[String]) -> Result<TsSettings, String> {
    let mut settings = TsSettings {
        save_mtime: true,
        ..Default::default()
    };
    if specs.is_empty() {
        return Ok(settings);
    }
    let mut save = [false, false, false]; // m, c, a
    let mut saw_plus = false;
    let mut saw_one = false;
    for spec in specs {
        let mut kinds = 0u8; // bit 0 = m, 1 = c, 2 = a
        let mut mode: Option<char> = None;
        for ch in spec.chars() {
            match ch {
                'm' => kinds |= 1,
                'c' => kinds |= 2,
                'a' => kinds |= 4,
                '+' | '1' | '-' => {
                    if mode.is_some() {
                        return Err(format!("invalid -ts spec: {spec}"));
                    }
                    mode = Some(ch);
                }
                _ => return Err(format!("invalid -ts spec: {spec}")),
            }
        }
        if kinds == 0 {
            kinds = 7; // bare -ts: all three
        }
        match mode {
            Some('-') => {
                // Omit the selected times entirely.
                if kinds & 1 != 0 {
                    save[0] = false;
                }
                if kinds & 2 != 0 {
                    save[1] = false;
                }
                if kinds & 4 != 0 {
                    save[2] = false;
                }
            }
            Some('1') => {
                saw_one = true;
                for (i, bit) in [1u8, 2, 4].iter().enumerate() {
                    if kinds & bit != 0 {
                        save[i] = true;
                    }
                }
            }
            _ => {
                // '+' or implicit: high precision.
                if mode == Some('+') {
                    saw_plus = true;
                }
                for (i, bit) in [1u8, 2, 4].iter().enumerate() {
                    if kinds & bit != 0 {
                        save[i] = true;
                    }
                }
            }
        }
    }
    settings.save_mtime = save[0];
    settings.save_ctime = save[1];
    settings.save_atime = save[2];
    settings.precision_seconds = saw_one && !saw_plus;
    Ok(settings)
}
