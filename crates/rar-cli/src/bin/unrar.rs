//! unrar — extract and inspect RAR4, RAR5, and RAR7 archives.

#[macro_use]
#[path = "../conout.rs"]
mod conout;
#[path = "../common.rs"]
mod common;
#[path = "../error.rs"]
mod error;
#[path = "../input.rs"]
mod input;
#[path = "../listfile.rs"]
mod listfile;
#[path = "../ops.rs"]
mod ops;
#[path = "../output.rs"]
mod output;
#[path = "../password.rs"]
mod password;
#[path = "../selector.rs"]
mod selector;
#[path = "../time.rs"]
#[allow(dead_code)] // shared with `rar`; unrar only needs the -ts parser
mod time;

use clap::{Args, CommandFactory, Parser, Subcommand};
use error::CliResult;
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
    /// Advanced compression parameters (like `-mc<par>`; accepted for CLI
    /// parity, unused on extraction)
    #[arg(long = "mc", value_name = "PAR", global = true)]
    #[allow(dead_code)]
    mc_params: Option<String>,
    /// Assume Yes on all queries (like `-y`; there are no interactive
    /// prompts, so this selects overwrite-on-extract)
    #[arg(long, global = true)]
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
    #[arg(long = "dest", value_name = "DEST")]
    dest: Option<String>,
    /// One or more member names to extract; when omitted, every file member
    /// is extracted (or, with `-so`, written to stdout). Member names match
    /// the full stored path, a `*`/`?` mask (masks also match basenames) or
    /// a directory prefix. A trailing argument ending with a path separator
    /// is treated as the destination directory.
    #[arg(value_name = "NAMES")]
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
    append_dir: Option<String>,
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
    /// Member names to list/test (empty = every member)
    #[arg(value_name = "NAMES")]
    names: Vec<String>,
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
    let raw: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    // Configuration sources (priority: command line > RARINISWITCHES >
    // rar.ini / .rarrc); normalized first so `-cfg-` and `--no-config` are
    // the same check.
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
    let surface = Cli::command();
    let value_options = common::value_options(&surface);
    let command = common::command_name(&cli_args, &value_options);
    let no_config = cli_args.iter().any(|a| a == "--no-config");
    let defaults: Vec<String> = common::default_switches(command.as_deref(), no_config)
        .iter()
        .map(|a| common::normalize_switch(a))
        .collect();
    // WinRAR accepts switches before the command; clap's subcommand-scoped
    // options do not, so move that block behind the command token.
    let cli_args = common::switches_after_command(cli_args, &surface);
    let args = common::merge_default_switches(defaults, cli_args, &value_options);
    if let Err(e) = password::reject_bare_password(&args) {
        eprintln!("unrar: {e}");
        process::exit(error::EXIT_BAD_COMMAND);
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
        // A silent outcome (exit code only) was already reported on stdout.
        if !e.message().is_empty() {
            eprintln!("unrar: {e}");
        }
        process::exit(e.exit_code());
    }
}

fn run(cli: Cli) -> CliResult<()> {
    let log_errors = cli.misc.log_errors.clone();
    let result = run_inner(cli);
    if let Err(e) = &result
        && !e.message().is_empty()
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

fn run_inner(cli: Cli) -> CliResult<()> {
    if cli.misc.erase_disk {
        return Err("-vd/--erase-disk is not supported; no disk was erased".into());
    }
    // UnRAR rejects `-log` (it is a console RAR switch), like the official
    // binary: "Unknown option: log..." with exit code 7.
    if let Some(spec) = cli.misc.log_specs.first() {
        return Err(error::CliError::with_code(
            format!("Unknown option: log{spec}"),
            error::EXIT_BAD_COMMAND,
        ));
    }
    let password = cli.password.password.as_deref();
    let ts = time::parse_ts_specs(&cli.ts_specs)?;
    let max_dict_size = cli
        .dict_extract
        .as_deref()
        .map(common::parse_mdx_size)
        .transpose()?;
    let motw = common::mark_web(cli.misc.mark_web.as_deref())?;
    match cli.command {
        Command::Extract(args) => {
            cmd_extract(&args, password, ts, max_dict_size, motw, &cli.misc, cli.yes)
        }
        Command::ExtractFlat(args) => {
            cmd_extract_flat(&args, password, ts, max_dict_size, motw, &cli.misc, cli.yes)
        }
        Command::List(args) => cmd_list(&args, password, &cli.misc),
        Command::ListBare(args) => cmd_list_bare(&args, password, &cli.misc),
        Command::ListTechnical(args) => cmd_list_technical(&args, password, &cli.misc),
        Command::VerboseList(args) => {
            let names = listfile::expand(&args.names, cli.misc.list_files.as_deref())
                .map_err(error::CliError::from)?;
            let rar = ops::open_reader(&args.archive, password)?;
            ops::list_entries(&rar, &args.archive, &names, true);
            Ok(())
        }
        Command::VerboseListBare(args) => cmd_list_bare(&args, password, &cli.misc),
        Command::VerboseListTechnical(args) => cmd_list_technical(&args, password, &cli.misc),
        Command::Test(args) => cmd_test(&args, password, &cli.misc),
        Command::Print(args) => cmd_print(&args, password, max_dict_size),
        Command::External(ext) => {
            // A bare archive path is not a command: official UnRAR prints
            // usage and exits 7. Listing used to happen silently and dropped
            // any member names or switches after the path.
            let name = ext.first().cloned().unwrap_or_default();
            Err(error::CliError::with_code(
                format!("unknown command: {name}"),
                error::EXIT_BAD_COMMAND,
            ))
        }
    }
}

/// Bare list (`lb` / `vb`): member names only.
fn cmd_list_bare(
    args: &ArchiveArgs,
    password: Option<&str>,
    misc: &common::MiscSwitches,
) -> CliResult<()> {
    let names =
        listfile::expand(&args.names, misc.list_files.as_deref()).map_err(error::CliError::from)?;
    let rar = ops::open_reader(&args.archive, password)?;
    ops::list_bare(&rar, &args.archive, &names);
    Ok(())
}

/// Technical list (`lt` / `vt`): mtime, sizes, ratio, CRC and method per
/// member, in the spirit of UnRAR's `lt`.
fn cmd_list_technical(
    args: &ArchiveArgs,
    password: Option<&str>,
    misc: &common::MiscSwitches,
) -> CliResult<()> {
    let names =
        listfile::expand(&args.names, misc.list_files.as_deref()).map_err(error::CliError::from)?;
    let rar = ops::open_reader(&args.archive, password)?;
    ops::list_technical(&rar, &args.archive, &names);
    Ok(())
}

/// Expand `@listfiles` and resolve the extraction target through the shared
/// `ops` helper.
fn resolve_target(
    args: &ExtractArgs,
    misc: &common::MiscSwitches,
) -> Result<(Vec<String>, std::path::PathBuf), String> {
    ops::extract_names_and_dest(
        &args.names,
        misc.list_files.as_deref(),
        args.dest.as_deref(),
        args.output_path.as_deref(),
        args.append_dir.as_deref(),
        &args.archive,
    )
}

fn cmd_extract(
    args: &ExtractArgs,
    password: Option<&str>,
    ts: time::TsSettings,
    max_dict_size: Option<u64>,
    motw: Option<rar_rs::MarkOfTheWeb>,
    misc: &common::MiscSwitches,
    assume_yes: bool,
) -> CliResult<()> {
    let (names, dest) = resolve_target(args, misc)?;
    let request = ops::ExtractRequest {
        names,
        dest,
        flat: args.flat,
        stdout: args.stdout,
        threads: args.threads,
        max_dict_size,
        mark_web: motw,
        overwrite: args.overwrite.clone(),
        assume_yes,
        auto_rename: args.auto_rename,
        freshen: misc.freshen,
        update: misc.update_files,
        keep_broken: args.keep_broken,
        set_creation_time: ts.save_ctime,
        set_access_time: ts.save_atime,
        skip_links: misc.skip_links,
        allow_unsafe_links: misc.unsafe_links,
    };
    let mut rar = ops::open_reader(&args.archive, password)?;
    if let Some(report) = ops::extract(&mut rar, &request)? {
        if report.written_count() == 0 && report.skipped_count() > 0 {
            // Like official UnRAR: every member was skipped -> "No files to
            // extract", exit 10 (the message is the whole report).
            info!("No files to extract");
            return Err(error::CliError::silent(error::EXIT_NO_FILES));
        }
        info!(
            "Extracted {} entries to {}",
            report.written_count(),
            request.dest.display()
        );
    }
    Ok(())
}

fn cmd_extract_flat(
    args: &ExtractArgs,
    password: Option<&str>,
    ts: time::TsSettings,
    max_dict_size: Option<u64>,
    motw: Option<rar_rs::MarkOfTheWeb>,
    misc: &common::MiscSwitches,
    assume_yes: bool,
) -> CliResult<()> {
    let (names, dest) = resolve_target(args, misc)?;
    let request = ops::ExtractRequest {
        names,
        dest,
        flat: true,
        stdout: args.stdout,
        threads: args.threads,
        max_dict_size,
        mark_web: motw,
        overwrite: args.overwrite.clone(),
        assume_yes,
        auto_rename: args.auto_rename,
        freshen: misc.freshen,
        update: misc.update_files,
        keep_broken: args.keep_broken,
        set_creation_time: ts.save_ctime,
        set_access_time: ts.save_atime,
        skip_links: misc.skip_links,
        allow_unsafe_links: misc.unsafe_links,
    };
    let mut rar = ops::open_reader(&args.archive, password)?;
    if let Some(report) = ops::extract(&mut rar, &request)? {
        if report.written_count() == 0 && report.skipped_count() > 0 {
            info!("No files to extract");
            return Err(error::CliError::silent(error::EXIT_NO_FILES));
        }
        info!(
            "Extracted {} entries to {}",
            report.written_count(),
            request.dest.display()
        );
    }
    Ok(())
}

fn cmd_list(
    args: &ArchiveArgs,
    password: Option<&str>,
    misc: &common::MiscSwitches,
) -> CliResult<()> {
    let names =
        listfile::expand(&args.names, misc.list_files.as_deref()).map_err(error::CliError::from)?;
    let rar = ops::open_reader(&args.archive, password)?;
    ops::list_entries(&rar, &args.archive, &names, false);
    Ok(())
}

fn cmd_test(
    args: &ArchiveArgs,
    password: Option<&str>,
    misc: &common::MiscSwitches,
) -> CliResult<()> {
    let names =
        listfile::expand(&args.names, misc.list_files.as_deref()).map_err(error::CliError::from)?;
    let mut rar = ops::open_reader(&args.archive, password)?;
    let report = ops::verify_members(&mut rar, &names)?;
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
        // Surface the first failure's category so scripts see CRC failures
        // (exit 3) or wrong passwords (exit 11) instead of a generic error.
        let code = report
            .failures()
            .first()
            .map_or(error::EXIT_FATAL, |failure| {
                error::exit_code_for(failure.error().code())
            });
        Err(error::CliError::with_code(
            format!(
                "{} file(s) failed, {} checked",
                report.failed(),
                report.checked()
            ),
            code,
        ))
    }
}

fn cmd_print(
    args: &PrintArgs,
    password: Option<&str>,
    max_dict_size: Option<u64>,
) -> CliResult<()> {
    let mut rar = ops::open_reader(&args.archive, password)?;
    ops::print_members(&mut rar, args.file.as_deref(), max_dict_size)
}
