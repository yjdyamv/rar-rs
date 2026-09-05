//! unrar — extract and inspect RAR4, RAR5, and RAR7 archives.

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
    name = "unrar",
    version,
    about = "unrar-rs — extract and inspect RAR archives"
)]
struct Cli {
    #[command(flatten)]
    password: password::PasswordArgs,
    /// Quiet mode: suppress informational messages (like `-idq` / `-inul`)
    #[arg(long, global = true)]
    quiet: bool,
    /// Send informational messages to stderr (like `-ierr`)
    #[arg(long, global = true)]
    err: bool,
    /// Dictionary size (like `-md<size>`; accepted for CLI parity; the
    /// decoder uses the dictionary declared by each member)
    #[arg(long = "dict-size", value_name = "SIZE", global = true)]
    #[allow(dead_code)]
    dict_size: Option<String>,
    /// Extraction dictionary cap (like `-mdx<size>`)
    #[arg(long = "dict-extract", value_name = "SIZE", global = true)]
    #[allow(dead_code)]
    dict_extract: Option<String>,
    /// Assume Yes on all queries (like `-y`; accepted, no prompts exist)
    #[arg(long, global = true)]
    #[allow(dead_code)]
    yes: bool,
    /// Save/restore file times (like `-ts[m,c,a][+,-,1]`; repeatable —
    /// on extraction, sets creation/access times in addition to mtime)
    #[arg(long = "ts", value_name = "SPEC", global = true, action = clap::ArgAction::Append)]
    #[allow(dead_code)]
    ts_specs: Vec<String>,
    /// Misc switches (-ilog and compatibility switches)
    #[command(flatten)]
    misc: common::MiscSwitches,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Extract with full paths
    #[command(visible_alias = "x")]
    Extract(ExtractArgs),
    /// Extract flat (no paths)
    #[command(visible_alias = "e")]
    ExtractFlat(ExtractArgs),
    /// List contents
    #[command(visible_alias = "l")]
    List(ArchiveArgs),
    /// List bare (names only, like `lb`)
    #[command(visible_alias = "lb")]
    ListBare(ArchiveArgs),
    /// List technical (like `lt`)
    #[command(visible_alias = "lt")]
    ListTechnical(ArchiveArgs),
    /// Verbosely list contents
    #[command(visible_alias = "v")]
    VerboseList(ArchiveArgs),
    /// Verbosely list bare (like `vb`)
    #[command(visible_alias = "vb")]
    VerboseListBare(ArchiveArgs),
    /// Verbosely list technical (like `vt`)
    #[command(visible_alias = "vt")]
    VerboseListTechnical(ArchiveArgs),
    /// Test integrity
    #[command(visible_alias = "t")]
    Test(ArchiveArgs),
    /// Print file to stdout
    #[command(visible_alias = "p")]
    Print(PrintArgs),
    /// A bare archive path is treated as a list command
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// Archive path plus an optional destination directory.
#[derive(Args)]
struct ExtractArgs {
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(long = "dest", default_value = ".", value_name = "DEST")]
    dest: Option<String>,
    /// One or more member names to extract; when omitted, every file member
    /// is extracted (or, with `-so`, written to stdout). Member names match
    /// the full stored path or its basename. They are never treated as a
    /// destination directory — set the destination with `--dest` instead.
    #[arg(value_name = "NAMES", trailing_var_arg = true)]
    names: Vec<String>,
    /// Output path for extracted files (like `-op<path>`; overrides
    /// the DEST argument when both are given)
    #[arg(long = "output-path", value_name = "PATH")]
    output_path: Option<String>,
    /// Extract without stored paths (like `-ep`; same as the `e` command)
    #[arg(long)]
    flat: bool,
    /// Rename existing destination files automatically (like `-or`):
    /// `name.ext` becomes `name(1).ext`
    #[arg(long = "auto-rename")]
    auto_rename: bool,
    /// Keep broken extracted files (like `-kb`)
    #[arg(long = "keep-broken")]
    keep_broken: bool,
    /// Extraction threads (like `rar -mt<N>`)
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

fn parse_threads(s: &str) -> Result<usize, String> {
    let n: usize = s
        .parse()
        .map_err(|_| format!("invalid thread count: {s}"))?;
    if (1..=64).contains(&n) {
        Ok(n)
    } else {
        Err("thread count must be between 1 and 64".to_string())
    }
}

/// Archive path.
#[derive(Args)]
struct ArchiveArgs {
    #[arg(value_name = "ARCHIVE")]
    archive: String,
}

/// Archive path plus an optional member to print.
#[derive(Args)]
struct PrintArgs {
    #[arg(value_name = "ARCHIVE")]
    archive: String,
    #[arg(value_name = "FILE")]
    file: Option<String>,
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    // Configuration sources (priority: command line > RARINISWITCHES >
    // rar.ini / .rarrc); `-cfg-` disables both.
    let no_config = raw.iter().skip(1).any(|a| a == "-cfg-");
    let command = common::command_name(&raw);
    let defaults: Vec<String> = common::default_switches(command.as_deref(), no_config)
        .iter()
        .map(|a| common::normalize_switch(a))
        .collect();
    let cli_args: Vec<String> = raw
        .iter()
        .skip(1)
        .map(|a| {
            // `-ep` means "exclude paths" here (like the `e` command);
            // the shared normalize maps it to the rar-side `-ep`
            // (basename-only add), which does not exist in unrar.
            if a == "-ep" {
                "--flat".to_string()
            } else {
                common::normalize_switch(a)
            }
        })
        .collect();
    let args = common::merge_default_switches(defaults, cli_args);
    if let Err(e) = password::reject_bare_password(&args) {
        eprintln!("unrar: {e}");
        process::exit(1);
    }
    // `unrar -iver` prints the version and exits (no subcommand needed).
    if args.iter().any(|a| a == "--version-info") {
        println!(
            "UNRAR 7.23 CLI parity (unrar-rs {})",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    let cli = Cli::parse_from(std::iter::once("unrar".to_string()).chain(args));
    output::QUIET.store(cli.quiet, std::sync::atomic::Ordering::Relaxed);
    output::ERR.store(cli.err, std::sync::atomic::Ordering::Relaxed);
    if let Err(e) = run(cli) {
        eprintln!("unrar: {e}");
        process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let log_errors = cli.misc.log_errors.clone();
    let result = run_inner(cli);
    if let Err(e) = &result
        && let Some(log) = &log_errors
    {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(format!("unrar: {e}\n").as_bytes())
            });
    }
    result
}

fn run_inner(cli: Cli) -> Result<(), String> {
    if cli.misc.erase_disk {
        return Err("-vd/--erase-disk is not supported; no disk was erased".into());
    }
    let password = cli.password.password.as_deref();
    let ts = time::parse_ts_specs(&cli.ts_specs)?;
    let max_dict_size = cli
        .dict_extract
        .as_deref()
        .map(common::parse_mdx_size)
        .transpose()?;
    match cli.command {
        Command::Extract(args) => cmd_extract(&args, password, ts, max_dict_size),
        Command::ExtractFlat(args) => cmd_extract_flat(&args, password, ts, max_dict_size),
        Command::List(args) => cmd_list(&args.archive, password),
        Command::ListBare(args) => cmd_list_bare(&args.archive, password),
        Command::ListTechnical(args) => cmd_list_technical(&args.archive, password),
        Command::VerboseList(args) => {
            output::print_verbose_list(&open_archive(&args.archive, password)?)
        }
        Command::VerboseListBare(args) => cmd_list_bare(&args.archive, password),
        Command::VerboseListTechnical(args) => cmd_list_technical(&args.archive, password),
        Command::Test(args) => cmd_test(&args.archive, password),
        Command::Print(args) => cmd_print(&args, password),
        Command::External(ext) => {
            let name = ext.first().cloned().unwrap_or_default();
            if name.ends_with(".rar") || name.ends_with(".cbr") {
                cmd_list(&name, password)
            } else {
                Err(format!("unknown command: {name}"))
            }
        }
    }
}

/// Bare list (`lb` / `vb`): member names only.
fn cmd_list_bare(archive: &str, password: Option<&str>) -> Result<(), String> {
    let rar = open_archive(archive, password)?;
    for entry in rar.entries() {
        println!("{}", entry.name());
    }
    Ok(())
}

/// Technical list (`lt` / `vt`): mtime, attributes, sizes, ratio, CRC and
/// method per member, in the spirit of UnRAR's `lt`.
fn cmd_list_technical(archive: &str, password: Option<&str>) -> Result<(), String> {
    let rar = open_archive(archive, password)?;
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
        let modified = format_unix_time(entry.mtime());
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

fn format_unix_time(secs: u32) -> String {
    // Simple UTC rendering of a unix timestamp (no chrono dependency).
    let days = secs as i64 / 86400;
    let secs_of_day = secs % 86400;
    let (y, m, d) = time::civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

fn open_archive(path: &str, password: Option<&str>) -> Result<rar5::ArchiveReader, String> {
    let mut options = rar5::OpenOptions::new();
    if let Some(password) = password {
        options = options.password(password);
    }
    rar5::ArchiveReader::open_with(path, options).map_err(|e| format!("{e}"))
}

fn cmd_extract(
    args: &ExtractArgs,
    password: Option<&str>,
    ts: time::TsSettings,
    max_dict_size: Option<u64>,
) -> Result<(), String> {
    if let Some(threads) = args.threads {
        rar5::set_extraction_threads(threads);
    }
    let base = args
        .output_path
        .clone()
        .or_else(|| args.dest.clone())
        .unwrap_or_else(|| ".".to_string());
    let dest = output::extract_dest(&base, &args.archive, args.append_dir);
    let mut rar = open_archive(&args.archive, password)?;

    // `-so`: write the extracted members to stdout (one stream) instead of
    // to disk — handy for piping. Directories carry no data.
    if args.stdout {
        return extract_to_stdout(&mut rar, &args.names, max_dict_size);
    }

    let opts = rar5::ExtractOptions {
        // Extraction is fully streaming: no per-member or total
        // size caps (WinRAR's UnRAR extracts any size).
        max_unpacked_bytes: None,
        max_total_unpacked_bytes: None,
        flat_paths: args.flat,
        skip_existing: args.overwrite.as_deref() == Some("never"),
        auto_rename: args.auto_rename,
        keep_broken: args.keep_broken,
        set_creation_time: ts.save_ctime,
        set_access_time: ts.save_atime,
        // WinRAR refuses dictionaries above 4 GiB unless -mdx raises
        // the cap; None here means "use the default cap".
        max_dict_size: max_dict_size.or(Some(4 * 1024 * 1024 * 1024)),
        ..Default::default()
    };
    let count = if args.names.is_empty() {
        rar.extract_all_with_options(&dest, opts)
            .map_err(|e| format!("{e}"))?;
        rar.entries().len()
    } else {
        extract_selected(&mut rar, &dest, &args.names, &opts)?
    };
    info!("Extracted {count} entries to {}", dest.display());
    Ok(())
}

/// Extract only the members whose name matches one of `names` (full stored
/// path or basename). Errors clearly when no member matches, so a mistyped
/// name is never silently swallowed or treated as a destination directory.
fn extract_selected(
    rar: &mut rar5::ArchiveReader,
    dest: &std::path::Path,
    names: &[String],
    opts: &rar5::ExtractOptions,
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
        rar.extract_entry_with_options(id, dest, *opts)
            .map_err(|e| format!("extract {member}: {e}"))?;
    }
    Ok(wanted.len())
}

/// Extract every file member of an archive to stdout, concatenated, like
/// `rar/unrar x -so`. Informational messages are suppressed so the stream
/// stays clean.
fn extract_to_stdout(
    rar: &mut rar5::ArchiveReader,
    names: &[String],
    max_dict_size: Option<u64>,
) -> Result<(), String> {
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
    let options = rar5::ExtractOptions {
        max_unpacked_bytes: None,
        max_total_unpacked_bytes: None,
        max_dict_size: max_dict_size.or(Some(4 * 1024 * 1024 * 1024)),
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

fn cmd_extract_flat(
    args: &ExtractArgs,
    password: Option<&str>,
    ts: time::TsSettings,
    max_dict_size: Option<u64>,
) -> Result<(), String> {
    let base = args
        .output_path
        .clone()
        .or_else(|| args.dest.clone())
        .unwrap_or_else(|| ".".to_string());
    let dest = output::extract_dest(&base, &args.archive, args.append_dir);
    let mut rar = open_archive(&args.archive, password)?;
    if args.stdout {
        return extract_to_stdout(&mut rar, &args.names, max_dict_size);
    }
    let opts = rar5::ExtractOptions {
        flat_paths: true,
        max_unpacked_bytes: None,
        max_total_unpacked_bytes: None,
        skip_existing: args.overwrite.as_deref() == Some("never"),
        auto_rename: args.auto_rename,
        keep_broken: args.keep_broken,
        set_creation_time: ts.save_ctime,
        set_access_time: ts.save_atime,
        max_dict_size: max_dict_size.or(Some(4 * 1024 * 1024 * 1024)),
        ..Default::default()
    };
    let count = if args.names.is_empty() {
        rar.extract_all_with_options(&dest, opts)
            .map_err(|e| format!("{e}"))?;
        rar.entries().len()
    } else {
        extract_selected(&mut rar, &dest, &args.names, &opts)?
    };
    info!("Extracted {count} entries to {}", dest.display());
    Ok(())
}

fn cmd_list(archive: &str, password: Option<&str>) -> Result<(), String> {
    let rar = open_archive(archive, password)?;

    println!(
        "{:>10}  {:>10}  {:>6}  {:<8}  Name",
        "Size", "Packed", "Ratio", "Method"
    );
    println!("{}", "-".repeat(60));

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
    }

    Ok(())
}

fn cmd_test(archive: &str, password: Option<&str>) -> Result<(), String> {
    let mut rar = open_archive(archive, password)?;

    let report: rar5::VerificationReport = rar.verify().map_err(|e| format!("test: {e}"))?;
    info!();
    if report.failed() == 0 {
        info!("All {} files OK", report.checked());
        Ok(())
    } else {
        for failure in report.failures() {
            let name = rar
                .entry(failure.entry_id())
                .map(|entry| entry.name().to_string())
                .unwrap_or_else(|_| "<unknown>".to_string());
            info!("{name}: {}", failure.error());
        }
        Err(format!(
            "{} file(s) failed, {} checked",
            report.failed(),
            report.checked()
        ))
    }
}

fn cmd_print(args: &PrintArgs, password: Option<&str>) -> Result<(), String> {
    use std::io::Write;
    let mut rar = open_archive(&args.archive, password)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let options = rar5::ExtractOptions {
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
        rar.copy_entry_to_with_options(id, &mut out, options)
            .map_err(|e| format!("{e}"))?;
    }

    out.flush().map_err(|e| format!("stdout: {e}"))
}
