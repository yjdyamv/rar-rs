//! `rar-sfx` — the console self-extractor prepended to SFX archives.
//!
//! The RAR payload is appended to this executable; when run it extracts every
//! member of that payload. `rar a -sfx` / `rar s` embed this binary as the SFX
//! module (`find_sfx_module`), so creating an SFX needs no WinRAR install. The
//! GUI stub is out of scope.

use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rar-sfx: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut destination = std::path::PathBuf::from(".");
    let mut overwrite = false;
    for argument in std::env::args_os().skip(1) {
        let text = argument.to_string_lossy();
        match text.as_ref() {
            "-y" | "--yes" | "-o+" => overwrite = true,
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {other}"));
            }
            _ => destination = argument.into(),
        }
    }

    let self_path = std::env::current_exe()
        .map_err(|error| format!("cannot locate my own executable: {error}"))?;
    let mut reader = rar_rs::ArchiveReader::open(&self_path)
        .map_err(|error| format!("{} is not an SFX archive: {error}", self_path.display()))?;
    // Skipping is the safe default for a self-extractor; `-y` overwrites.
    let options = rar_rs::ExtractOptions {
        skip_existing: !overwrite,
        ..rar_rs::ExtractOptions::default()
    };
    let report = reader
        .extract_all_with_options(&destination, options)
        .map_err(|error| format!("extraction failed: {error}"))?;
    println!(
        "Extracted {} file(s) to {}",
        report.written_count(),
        destination.display()
    );
    Ok(())
}

fn print_help() {
    let exe = std::env::args().next().unwrap_or_else(|| "rar-sfx".into());
    println!("rar-sfx — console self-extractor for rar-rs SFX archives");
    println!();
    println!("Usage: {exe} [-y|--yes] [DESTINATION]");
    println!();
    println!("Extracts the RAR payload appended to this executable.");
    println!("  DESTINATION   output directory (default: current directory)");
    println!("  -y, --yes     overwrite existing files (default: skip)");
}
