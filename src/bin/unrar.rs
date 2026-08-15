//! unrar — extract and inspect RAR5 archives.

mod common;

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
    password: common::PasswordArgs,
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
    /// Verbosely list contents
    #[command(visible_alias = "v")]
    VerboseList(ArchiveArgs),
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
    /// Extraction threads (like `rar -mt<N>`)
    #[arg(long = "threads", value_name = "N", value_parser = parse_threads)]
    threads: Option<usize>,
}

fn parse_threads(s: &str) -> Result<usize, String> {
    let n: usize = s
        .parse()
        .map_err(|_| format!("invalid thread count: {s}"))?;
    if (1..=64).contains(&n) {
        Ok(n)
    } else {
        Err(format!("thread count must be between 1 and 64"))
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
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("unrar: {e}");
        process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let password = cli.password.password.as_deref();
    match cli.command {
        Command::Extract(args) => cmd_extract(&args, password),
        Command::ExtractFlat(args) => cmd_extract_flat(&args, password),
        Command::List(args) => cmd_list(&args.archive, password),
        Command::VerboseList(args) => {
            common::print_verbose_list(&open_archive(&args.archive, password)?)
        }
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

fn open_archive(path: &str, password: Option<&str>) -> Result<rar5::RarArchive, String> {
    if let Some(pw) = password {
        rar5::RarArchive::open_with_password(path, pw).map_err(|e| format!("{e}"))
    } else {
        rar5::RarArchive::open(path).map_err(|e| format!("{e}"))
    }
}

fn cmd_extract(args: &ExtractArgs, password: Option<&str>) -> Result<(), String> {
    if let Some(threads) = args.threads {
        rar5::set_extraction_threads(threads);
    }
    let dest = args.dest.as_deref().unwrap_or(".");
    let mut rar = open_archive(&args.archive, password)?;

    let count = rar.list().len();
    rar.extract_all(dest).map_err(|e| format!("{e}"))?;
    println!("Extracted {count} entries to {dest}");
    Ok(())
}

fn cmd_extract_flat(args: &ExtractArgs, password: Option<&str>) -> Result<(), String> {
    let dest = args.dest.as_deref().unwrap_or(".");
    let mut rar = open_archive(&args.archive, password)?;
    rar.extract_all_with_options(
        dest,
        rar5::ExtractOptions {
            flat_paths: true,
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
                println!("  OK  {name}");
                ok += 1;
            }
            Err(e) => {
                println!("  FAIL  {name}: {e}");
                fail += 1;
            }
        }
    }

    println!();
    if fail == 0 {
        println!("All {ok} files OK");
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
