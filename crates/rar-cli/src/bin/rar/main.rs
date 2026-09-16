//! rar — create, modify, and inspect RAR4, RAR5, and RAR7 archives.

#[macro_use]
#[path = "../../conout.rs"]
mod conout;
#[path = "../../common.rs"]
mod common;
#[path = "../../error.rs"]
mod error;
#[path = "../../input.rs"]
mod input;
#[path = "../../listfile.rs"]
mod listfile;
#[path = "../../name_policy.rs"]
mod name_policy;
#[path = "../../ops.rs"]
mod ops;
#[path = "../../output.rs"]
mod output;
#[path = "../../password.rs"]
mod password;
#[path = "../../selector.rs"]
mod selector;
#[path = "../../time.rs"]
mod time;

mod args;
mod comment;
mod create;
mod edit;
mod extract;
mod filters;
mod links;
mod list;
mod log;
mod recovery;
mod sfx;
mod transaction;
mod update;

#[cfg(test)]
mod tests;

use clap::{CommandFactory, Parser};
use std::process;

use args::{Cli, Command, RecoveryVolumesArgs};
use error::CliResult;

fn main() {
    let raw: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    // Configuration sources (priority: command line > RARINISWITCHES >
    // rar.ini); normalized first so `-cfg-` and `--no-config` are the same
    // check.
    let cli_args: Vec<String> = raw
        .iter()
        .skip(1)
        .map(|a| common::normalize_switch(a))
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
    // options do not, so move that block behind the command token. External
    // commands (`i<string>`, `rv<N>`) get the same treatment.
    let cli_args = common::switches_after_command(cli_args, &surface);
    let cli_args = common::switches_after_external_command(cli_args);
    let args = common::merge_default_switches(defaults, cli_args, &value_options);
    if let Err(e) = password::reject_bare_password(&args) {
        eprintln!("rar: {e}");
        process::exit(error::EXIT_BAD_COMMAND);
    }
    // `rar -iver` prints the version and exits (no subcommand needed).
    if args.iter().any(|a| a == "--version-info") {
        println!("RAR 7.23 CLI parity (rar-rs {})", env!("CARGO_PKG_VERSION"));
        return;
    }
    let cli = Cli::parse_from(std::iter::once("rar".to_string()).chain(args));
    output::QUIET.store(cli.quiet, std::sync::atomic::Ordering::Relaxed);
    output::ERR.store(cli.err, std::sync::atomic::Ordering::Relaxed);
    if let Some(dir) = &cli.work_dir {
        // WinRAR's `-w<p>` only selects the directory used for temporary
        // files (and requires it to exist); it never changes the working
        // directory, so neither do we. Our staging files stay next to
        // their target so an interrupted commit never crosses volumes.
        if !std::path::Path::new(dir).is_dir() {
            eprintln!("rar: cannot set work directory {dir}: no such directory");
            process::exit(error::EXIT_BAD_COMMAND);
        }
    }
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
        process::exit(e.exit_code());
    }
}

fn run(cli: Cli) -> CliResult<()> {
    let misc = &cli.misc;
    if misc.erase_disk {
        return Err("-vd/--erase-disk is not supported; no disk was erased".into());
    }
    match cli.command {
        Command::Create(args) => {
            // `-f` / `-u` turn `a` into the freshen/update commands, like
            // WinRAR's "a -f is equivalent to f".
            if misc.freshen {
                update::cmd_freshen(&crate::args::as_files_args(&args), misc)
            } else if misc.update_files {
                update::cmd_update(&crate::args::as_files_args(&args), misc)
            } else {
                create::cmd_create(&args, misc)
            }
        }
        Command::Update(args) => update::cmd_update(&args, misc),
        Command::Freshen(args) => update::cmd_freshen(&args, misc),
        Command::Move(args) => edit::cmd_move(&args, misc, false),
        Command::MoveFiles(args) => edit::cmd_move(&args, misc, true),
        Command::Delete(args) => edit::cmd_delete(&args, misc),
        Command::Rename(args) => edit::cmd_rename(&args),
        Command::Change(args) => edit::cmd_change(&args),
        Command::Lock(args) => recovery::cmd_lock(&args),
        Command::Recovery(args) => recovery::cmd_rr(&args),
        Command::RecoveryVolumes(args) => recovery::cmd_recovery_volumes(&args),
        Command::Repair(args) => recovery::cmd_repair(&args),
        Command::RebuildVolumes(args) => recovery::cmd_rebuild_volumes(&args),
        Command::Sfx(args) => sfx::cmd_sfx(&args),
        Command::SfxStrip(args) => sfx::cmd_sfx_strip(&args),
        Command::CommentSet(args) => comment::cmd_comment_set(&args, misc),
        Command::CommentWrite(args) => comment::cmd_comment_write(&args),
        Command::CommentFileSet(args) => comment::cmd_file_comment_set(&args, misc),
        Command::Print(args) => extract::cmd_print(&args),
        Command::Extract(args) => extract::cmd_extract(&args, misc, cli.yes),
        Command::ExtractFlat(args) => extract::cmd_extract_flat(&args, misc, cli.yes),
        Command::Test(args) => list::cmd_test(&args, misc),
        Command::VerboseList(args) => list::cmd_verbose_list(&args, misc),
        Command::List(args) => list::cmd_list(&args, misc),
        Command::ListBare(args) => list::cmd_list_bare(&args, misc),
        Command::ListTechnical(args) => list::cmd_list_technical(&args, misc),
        Command::VerboseListBare(args) => list::cmd_list_bare(&args, misc),
        Command::VerboseListTechnical(args) => list::cmd_list_technical(&args, misc),
        Command::Info(args) => list::cmd_info(&args),
        Command::External(ext) => {
            let name = ext.first().cloned().unwrap_or_default();
            // The external commands (`i<string>`, `rv[N]`) have no clap
            // surface of their own, so their trailing arguments arrive raw:
            // split out a password and the positionals, ignoring the other
            // normalized switches.
            let (password, positionals) = split_external_args(&ext[1..]);
            // `i<string>` (and `ic`/`ih` variants) find strings in members.
            if name.len() > 1 && name.starts_with('i') {
                list::cmd_find(&name, &positionals, password.as_deref())
            // WinRAR's canonical `rv[N]` embeds the count in the command
            // token (`rar rv3 data.part01.rar`); route those here.
            } else if name.len() > 2
                && name.starts_with("rv")
                && name[2..].chars().all(|c| c.is_ascii_digit() || c == '%')
            {
                let spec = name[2..].to_string();
                recovery::cmd_recovery_volumes(&RecoveryVolumesArgs {
                    password: password::PasswordArgs { password },
                    archive: positionals.first().cloned().unwrap_or_default(),
                    count_spec: if spec.is_empty() { "10%".into() } else { spec },
                })
            } else {
                Err(format!("unknown command: {name}").into())
            }
        }
    }
}

/// Split an external command's trailing arguments into an optional password
/// and the positionals. Normalized switches arrive as their long forms
/// (`-m0` became `--level=0`), so anything starting with `--` is a switch;
/// `--password` accepts both the attached and the spaced value forms.
fn split_external_args(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut password = None;
    let mut positionals = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--password=") {
            password = Some(value.to_string());
        } else if arg == "--password" {
            if let Some(value) = iter.next() {
                password = Some(value.clone());
            }
        } else if arg.starts_with("--") {
            // Another switch: accepted and ignored for these commands.
        } else {
            positionals.push(arg.clone());
        }
    }
    (password, positionals)
}

#[cfg(test)]
mod conout_shadowing {
    use crate::conout::test_capture::{self, serial};

    #[test]
    fn println_outside_conout_routes_through_it() {
        let _serial = serial();
        test_capture::start();
        println!("é");
        let captured = test_capture::take().expect("capture was on");
        assert_eq!(String::from_utf8(captured).unwrap(), "é\n");
    }
}
