//! `rar-sfx` — the console self-extractor prepended to SFX archives.
//!
//! The RAR payload is appended to this executable; when run it extracts every
//! member of that payload. `rar a -sfx` / `rar s` embed this binary as the SFX
//! module (`find_sfx_module`), so creating an SFX needs no WinRAR install. The
//! GUI stub is out of scope.
//!
//! Its command line follows WinRAR's console SFX module (`WinCon.SFX`): the
//! commands `-x` (extract, the default), `-t` (test), `-v` (list) and `-?`
//! (help), an archive password prompt (no echo, a wrong entry is re-asked) and
//! an interactive overwrite prompt. The destination is the current directory
//! unless one is named. `-p<pwd>`, `-y`/`-o+`/`-o-`/`-or` are honoured beyond
//! the official module, which reads the password only from the prompt.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use rar_rs::{ArchiveReader, ErrorCode, ExtractOptions, OpenOptions, OverwriteChoice};

#[path = "../error.rs"]
mod error;

/// The action to take, mirroring WinRAR's SFX commands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Extract,
    Test,
    List,
    Help,
}

/// How an existing destination is handled.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Overwrite {
    /// Ask about each existing file (the default).
    Ask,
    /// `-y` / `-o+`: overwrite everything.
    All,
    /// `-o-`: leave existing files untouched.
    Skip,
    /// `-or`: write to `name(N).ext` instead.
    Rename,
}

struct Args {
    command: Command,
    password: Option<String>,
    /// `-p-`: never prompt.
    no_password: bool,
    overwrite: Overwrite,
    destination: PathBuf,
}

fn main() -> ExitCode {
    ExitCode::from(run())
}

fn run() -> u8 {
    let args = match parse(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("rar-sfx: {message}");
            return 1;
        }
    };
    if args.command == Command::Help {
        print_help();
        return 0;
    }

    let self_path = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("rar-sfx: cannot locate my own executable: {error}");
            return 1;
        }
    };

    println!("RAR SFX archive");
    match args.command {
        Command::Extract => println!("Extracting from {}", self_path.display()),
        Command::Test => println!("Testing archive {}", self_path.display()),
        Command::List => println!("Archive: {}", self_path.display()),
        Command::Help => unreachable!("handled above"),
    }

    let (mut password, mut reader) = {
        let initial = if args.no_password {
            None
        } else {
            args.password.clone()
        };
        match open(&self_path, initial, args.no_password) {
            Some(pair) => pair,
            None => return aborted(),
        }
    };
    let first = first_name(&reader);

    let code = match args.command {
        Command::List => {
            list(&reader);
            0
        }
        Command::Test => loop {
            match reader.verify() {
                Ok(report) if report.is_ok() => break 0,
                Ok(report) => {
                    eprintln!(
                        "rar-sfx: {} of {} member(s) failed verification",
                        report.failed(),
                        report.checked()
                    );
                    break error::EXIT_CRC as u8;
                }
                Err(error) if needs_password(error.code(), args.no_password) => {
                    if resolve_password(&error, &first, &mut password).is_none() {
                        return aborted();
                    }
                    reader = match reopen(&self_path, &mut password, args.no_password) {
                        Some(reader) => reader,
                        None => return aborted(),
                    };
                }
                Err(error) => break report(&error),
            }
        },
        Command::Extract => {
            let mut options = ExtractOptions::default();
            match args.overwrite {
                Overwrite::All => {}
                Overwrite::Skip => options.skip_existing = true,
                Overwrite::Rename => options.auto_rename = true,
                Overwrite::Ask => options.prompt_overwrite = true,
            }
            if args.overwrite == Overwrite::Ask {
                reader.set_overwrite_prompt(Some(overwrite_prompt()));
            }
            loop {
                match reader.extract_all_with_options(&args.destination, options.clone()) {
                    Ok(_) => break 0,
                    Err(error) if needs_password(error.code(), args.no_password) => {
                        if resolve_password(&error, &first, &mut password).is_none() {
                            return aborted();
                        }
                        reader = match reopen(&self_path, &mut password, args.no_password) {
                            Some(reader) => reader,
                            None => return aborted(),
                        };
                        if args.overwrite == Overwrite::Ask {
                            reader.set_overwrite_prompt(Some(overwrite_prompt()));
                        }
                    }
                    // Answering `Q` (or closing stdin) at the overwrite prompt
                    // aborts, like the official module.
                    Err(error) if error.code() == ErrorCode::Cancelled => return aborted(),
                    Err(error) => break report(&error),
                }
            }
        }
        Command::Help => 0,
    };

    if code == 0 && matches!(args.command, Command::Extract | Command::Test) {
        println!();
        println!("All OK");
    }
    code
}

/// Open the payload, prompting for a password when one is needed.
///
/// Returns the reader together with the password actually used, or `None` when
/// the user aborted the prompt. A fatal (non-password) error exits directly.
fn open(
    path: &Path,
    mut password: Option<String>,
    no_password: bool,
) -> Option<(Option<String>, ArchiveReader)> {
    loop {
        match try_open(path, password.as_deref()) {
            Ok(reader) => return Some((password, reader)),
            Err(error) if needs_password(error.code(), no_password) => {
                resolve_password(&error, "archive", &mut password)?;
            }
            Err(error) => {
                eprintln!("rar-sfx: {error}");
                std::process::exit(error::exit_code_for(error.code()));
            }
        }
    }
}

fn reopen(path: &Path, password: &mut Option<String>, no_password: bool) -> Option<ArchiveReader> {
    loop {
        match try_open(path, password.as_deref()) {
            Ok(reader) => return Some(reader),
            Err(error) if needs_password(error.code(), no_password) => {
                resolve_password(&error, "archive", password)?;
            }
            Err(error) => {
                eprintln!("rar-sfx: {error}");
                std::process::exit(error::exit_code_for(error.code()));
            }
        }
    }
}

fn try_open(path: &Path, password: Option<&str>) -> Result<ArchiveReader, rar_rs::RarError> {
    let mut options = OpenOptions::new();
    if let Some(password) = password {
        options = options.password(password);
    }
    ArchiveReader::open_with(path, options)
}

/// Whether an error means "a (different) password is needed".
fn needs_password(code: ErrorCode, no_password: bool) -> bool {
    !no_password && matches!(code, ErrorCode::Encrypted | ErrorCode::WrongPassword)
}

/// Ask for a password and store it in `password`. `None` means the user
/// aborted (stdin was closed).
fn resolve_password(
    error: &rar_rs::RarError,
    name: &str,
    password: &mut Option<String>,
) -> Option<()> {
    if error.code() == ErrorCode::WrongPassword {
        println!("The specified password is incorrect.");
    }
    match prompt_password(name) {
        Some(entered) => {
            *password = Some(entered);
            Some(())
        }
        None => None,
    }
}

fn first_name(reader: &ArchiveReader) -> String {
    reader
        .entries()
        .next()
        .map(|entry| entry.name().to_string())
        .unwrap_or_else(|| "archive".to_string())
}

/// The friendly family name WinRAR prints after `Details:`.
fn version_label(version: rar_rs::ArchiveVersion) -> &'static str {
    if version.is_rar13() {
        "RAR 1.4"
    } else if version.is_legacy() {
        "RAR 4"
    } else if version == rar_rs::ArchiveVersion::V70 {
        "RAR 7"
    } else {
        "RAR 5"
    }
}

fn list(reader: &ArchiveReader) {
    let version = reader
        .entries()
        .next()
        .map(|entry| version_label(entry.version()))
        .unwrap_or("RAR 5");
    println!("Details: {version}, SFX");
    println!();
    println!("{:>12}  Name", "Size");
    let mut total = 0u64;
    let mut count = 0usize;
    for entry in reader.entries() {
        println!("{:>12}  {}", entry.size(), entry.name());
        total += entry.size();
        count += 1;
    }
    println!("{:>12}  {} file(s)", total, count);
}

/// The interactive overwrite prompt (WinRAR's `[Y]es, [N]o, [A]ll, n[E]ver,
/// [R]ename, [Q]uit`). `A` and `E` are remembered for the rest of the run.
fn overwrite_prompt() -> Arc<rar_rs::OverwritePrompt> {
    // 0 = ask, 1 = all, 2 = never.
    let state = AtomicU8::new(0);
    Arc::new(move |path: &Path| -> OverwriteChoice {
        match state.load(Ordering::Relaxed) {
            1 => return OverwriteChoice::OverwriteAll,
            2 => return OverwriteChoice::Skip,
            _ => {}
        }
        loop {
            println!(
                "Would you like to replace the existing file {}",
                path.display()
            );
            print!("\n[Y]es, [N]o, [A]ll, n[E]ver, [R]ename, [Q]uit ");
            let _ = io::stdout().flush();
            let mut line = String::new();
            match io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => return OverwriteChoice::Quit,
                Ok(_) => {}
            }
            match line
                .trim_start()
                .chars()
                .next()
                .map(|c| c.to_ascii_lowercase())
            {
                Some('y') => return OverwriteChoice::Overwrite,
                Some('n') => return OverwriteChoice::Skip,
                Some('a') => {
                    state.store(1, Ordering::Relaxed);
                    return OverwriteChoice::OverwriteAll;
                }
                Some('e') => {
                    state.store(2, Ordering::Relaxed);
                    return OverwriteChoice::Skip;
                }
                Some('r') => return OverwriteChoice::Rename,
                Some('q') => return OverwriteChoice::Quit,
                _ => continue,
            }
        }
    })
}

/// Prompt for a password, reading it without echo. `None` means stdin was
/// closed (the caller aborts).
fn prompt_password(name: &str) -> Option<String> {
    print!("Enter password (will not be echoed) for {name}: ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let read = read_secret(&mut line).ok()?;
    let _ = io::stdout().write_all(b"\n");
    if read == 0 {
        return None;
    }
    Some(line.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(unix)]
fn read_secret(line: &mut String) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;

    let fd = io::stdin().as_raw_fd();
    // SAFETY: `fd` is stdin's valid descriptor and `termios` is plain data.
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    let is_tty = unsafe { libc::isatty(fd) } == 1;
    let restore = if is_tty && unsafe { libc::tcgetattr(fd, &mut term) } == 0 {
        let saved = term;
        let mut hidden = term;
        hidden.c_lflag &= !libc::ECHO;
        // SAFETY: `hidden` is a valid termios for this terminal.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) };
        Some(saved)
    } else {
        None
    };
    let read = io::stdin().read_line(line);
    if let Some(saved) = restore {
        // SAFETY: restore the mode captured above.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    }
    read
}

#[cfg(windows)]
fn read_secret(line: &mut String) -> io::Result<usize> {
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
    };

    // SAFETY: console-mode calls on stdin; a non-console stdin simply fails
    // `GetConsoleMode`, and the password is then read with echo as usual.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut mode = 0u32;
    let has_mode = unsafe { GetConsoleMode(handle, &mut mode) } != 0;
    if has_mode {
        unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) };
    }
    let read = io::stdin().read_line(line);
    if has_mode {
        unsafe { SetConsoleMode(handle, mode) };
    }
    read
}

fn aborted() -> u8 {
    println!();
    println!("Program aborted");
    error::EXIT_PASSWORD_REQUIRED as u8
}

fn report(error: &rar_rs::RarError) -> u8 {
    eprintln!("rar-sfx: {error}");
    error::exit_code_for(error.code()) as u8
}

fn parse<I: Iterator<Item = OsString>>(mut arguments: I) -> Result<Args, String> {
    let mut command = Command::Extract;
    let mut command_seen = false;
    let mut args = Args {
        command: Command::Extract,
        password: None,
        no_password: false,
        overwrite: Overwrite::Ask,
        destination: PathBuf::from("."),
    };
    let mut destination_seen = false;
    while let Some(argument) = arguments.next() {
        let text = argument.to_string_lossy();
        match text.as_ref() {
            "-x" | "--extract" => set_command(&mut command, &mut command_seen, Command::Extract)?,
            "-t" | "--test" => set_command(&mut command, &mut command_seen, Command::Test)?,
            "-v" | "--list" => set_command(&mut command, &mut command_seen, Command::List)?,
            "-?" | "-h" | "--help" => set_command(&mut command, &mut command_seen, Command::Help)?,
            "-y" | "-o+" => args.overwrite = Overwrite::All,
            "-o-" => args.overwrite = Overwrite::Skip,
            "-or" => args.overwrite = Overwrite::Rename,
            // Accepted for compatibility; the console stub has no GUI to quiet.
            "-s" => {}
            "-p-" => {
                args.no_password = true;
                args.password = None;
            }
            "-p" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "-p requires a password value".to_string())?;
                args.password = Some(value.to_string_lossy().into_owned());
            }
            other if other.starts_with("-p") => {
                args.password = Some(other[2..].to_string());
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unknown option: {other}"));
            }
            _ => {
                if destination_seen {
                    return Err(format!("unexpected argument: {text}"));
                }
                args.destination = PathBuf::from(argument);
                destination_seen = true;
            }
        }
    }
    args.command = command;
    Ok(args)
}

fn set_command(command: &mut Command, seen: &mut bool, value: Command) -> Result<(), String> {
    if *seen {
        return Err("only one command may be given".to_string());
    }
    *command = value;
    *seen = true;
    Ok(())
}

fn print_help() {
    let exe = std::env::args().next().unwrap_or_else(|| "rar-sfx".into());
    println!("RAR SFX archive");
    println!();
    println!("Usage: {exe} [-x|-t|-v|-?] [-p<password>] [-y|-o+|-o-|-or] [destination]");
    println!();
    println!("<Commands>");
    println!("  -x      Extract from archive (default)");
    println!("  -t      Test archive files");
    println!("  -v      Verbosely list contents of archive");
    println!("  -?      Display this help");
    println!();
    println!("<Switches>");
    println!("  -p<pwd>   Set the password (-p- disables the prompt)");
    println!("  -y, -o+   Overwrite existing files (-y is an alias)");
    println!("  -o-       Skip existing files");
    println!("  -or       Rename existing files automatically");
    println!();
    println!("Without a destination the archive extracts into the current directory.");
}
