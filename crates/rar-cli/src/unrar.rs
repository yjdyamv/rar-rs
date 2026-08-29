//! unrar — extract and inspect RAR5 archives.

mod common;
mod input;
mod output;
mod password;
mod time;

use clap::{Args, Parser, Subcommand};
use std::process;

#[derive(Parser)]
#[command(
    name = "unrar",
    version,
    about = "unrar-rs — extract and inspect RAR5 archives"
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
    /// Dictionary size (like `-md<size>`; accepted for CLI parity, no
    /// effect on extraction — RAR5 dictionaries up to 4 GiB are always
    /// supported)
    #[arg(long = "dict-size", value_name = "SIZE", global = true)]
    #[allow(dead_code)]
    dict_size: Option<String>,
    /// Extraction dictionary cap (like `-mdx<size>`; accepted, no effect)
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
    /// Misc switches (-ilog and accepted no-ops)
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
    #[arg(value_name = "DEST")]
    dest: Option<String>,
    /// One or more member names to extract; when omitted, every file member
    /// is extracted (or, with `-so`, written to stdout). WinRAR matches the
    /// full stored path, so a basename also selects the member. Member names
    /// follow the optional destination directory, like `x archive dest name`.
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
    for entry in rar.list() {
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
        let modified = format_unix_time(entry.header.mtime);
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

fn open_archive(path: &str, password: Option<&str>) -> Result<rar5::RarArchive, String> {
    if let Some(pw) = password {
        rar5::RarArchive::open_with_password(path, pw).map_err(|e| format!("{e}"))
    } else {
        rar5::RarArchive::open(path).map_err(|e| format!("{e}"))
    }
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
        return extract_to_stdout(&mut rar, &args.names);
    }

    let count = rar.list().len();
    rar.extract_all_with_options(
        &dest,
        rar5::ExtractOptions {
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
        },
    )
    .map_err(|e| format!("{e}"))?;
    info!("Extracted {count} entries to {}", dest.display());
    Ok(())
}

/// Extract every file member of an archive to stdout, concatenated, like
/// `rar/unrar x -so`. Informational messages are suppressed so the stream
/// stays clean.
fn extract_to_stdout(rar: &mut rar5::RarArchive, names: &[String]) -> Result<(), String> {
    use std::io::Write;
    let members: Vec<String> = rar
        .list()
        .iter()
        .filter(|e| !e.is_dir())
        .map(|e| e.name().to_string())
        .collect();
    let mut wanted: Vec<String> = Vec::new();
    if names.is_empty() {
        wanted = members;
    } else {
        for m in &members {
            if names
                .iter()
                .any(|n| m == n || m.ends_with(&format!("/{n}")) || n.ends_with(m))
            {
                wanted.push(m.clone());
            }
        }
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for name in &wanted {
        let data = rar
            .read_with_options(
                name,
                rar5::ExtractOptions {
                    max_unpacked_bytes: None,
                    max_total_unpacked_bytes: None,
                    ..Default::default()
                },
            )
            .map_err(|e| format!("read {name}: {e}"))?;
        out.write_all(&data).map_err(|e| format!("stdout: {e}"))?;
    }
    out.flush().map_err(|e| format!("stdout: {e}"))?;
    Ok(())
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
        return extract_to_stdout(&mut rar, &args.names);
    }
    rar.extract_all_with_options(
        &dest,
        rar5::ExtractOptions {
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
        },
    )
    .map_err(|e| format!("{e}"))?;
    Ok(())
}

fn cmd_list(archive: &str, password: Option<&str>) -> Result<(), String> {
    let rar = open_archive(archive, password)?;

    println!(
        "{:>10}  {:>10}  {:>6}  {:<8}  Name",
        "Size", "Packed", "Ratio", "Method"
    );
    println!("{}", "-".repeat(60));

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
    }

    Ok(())
}

fn cmd_test(archive: &str, password: Option<&str>) -> Result<(), String> {
    let mut rar = open_archive(archive, password)?;

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
                info!("  FAIL  {name}: {e}");
                fail += 1;
            }
        }
    }

    info!();
    if fail == 0 {
        info!("All {ok} files OK");
        Ok(())
    } else {
        Err(format!("{fail} file(s) failed, {ok} OK"))
    }
}

fn cmd_print(args: &PrintArgs, password: Option<&str>) -> Result<(), String> {
    let mut rar = open_archive(&args.archive, password)?;

    if let Some(file) = &args.file {
        let data = rar.read(file).map_err(|e| format!("{e}"))?;
        use std::io::Write;
        std::io::stdout()
            .write_all(&data)
            .map_err(|e| format!("{e}"))?;
    } else {
        let names: Vec<String> = rar.list().iter().map(|e| e.name().to_string()).collect();
        for name in &names {
            let entry = rar.get_entry(name).unwrap();
            if entry.is_dir() {
                continue;
            }
            let data = rar.read(name).map_err(|e| format!("{e}"))?;
            use std::io::Write;
            std::io::stdout()
                .write_all(&data)
                .map_err(|e| format!("{e}"))?;
        }
    }

    Ok(())
}
