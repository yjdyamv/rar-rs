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
        } else {
            format!("--password={rest}")
        };
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
    if let Some(rest) = arg.strip_prefix("-x") {
        return format!("--exclude={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-n") {
        return format!("--include={rest}");
    }
    if let Some(rest) = arg.strip_prefix("-sfx") {
        return format!("--sfx-module={rest}");
    }
    arg.to_string()
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
