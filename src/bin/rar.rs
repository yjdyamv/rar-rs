//! rar — create and modify RAR5 archives.

use std::env;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        usage();
        process::exit(1);
    }

    let result = match args[1].as_str() {
        "a" | "create" => cmd_create(&args[2..]),
        "l" | "list" => cmd_list(&args[2..]),
        "i" | "info" => cmd_info(&args[2..]),
        "-h" | "--help" | "help" => {
            usage();
            Ok(())
        }
        _ => {
            eprintln!("unknown command: {}", args[1]);
            usage();
            process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("rar: {e}");
        process::exit(1);
    }
}

fn usage() {
    eprintln!("rar-rs — create RAR5 archives");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  rar a [-m0..-m5] [-p<password>] [-v<size>] <archive.rar> <files...>");
    eprintln!("  rar l [-p<password>] <archive.rar>              List archive contents");
    eprintln!("  rar i [-p<password>] <archive.rar>              Show archive info");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -v<size>    Create multi-volume archive (e.g. -v1m, -v100k, -v50000)");
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('k').or_else(|| s.strip_suffix('K')) {
        num.parse::<u64>()
            .map(|n| n * 1024)
            .map_err(|_| format!("invalid size: {s}"))
    } else if let Some(num) = s.strip_suffix('m').or_else(|| s.strip_suffix('M')) {
        num.parse::<u64>()
            .map(|n| n * 1024 * 1024)
            .map_err(|_| format!("invalid size: {s}"))
    } else {
        s.parse::<u64>().map_err(|_| format!("invalid size: {s}"))
    }
}

fn cmd_create(args: &[String]) -> Result<(), String> {
    let mut level: u8 = 3;
    let mut password: Option<String> = None;
    let mut volume_size: Option<u64> = None;
    let mut positional = Vec::new();

    for arg in args {
        if let Some(rest) = arg.strip_prefix("-m") {
            level = rest
                .parse::<u8>()
                .map_err(|_| format!("invalid compression level: {arg}"))?;
            if level > 5 {
                return Err(format!("compression level must be 0-5, got {level}"));
            }
        } else if let Some(pw) = arg.strip_prefix("-p") {
            password = Some(pw.to_string());
        } else if let Some(sz) = arg.strip_prefix("-v") {
            volume_size = Some(parse_size(sz)?);
        } else {
            positional.push(arg.as_str());
        }
    }

    if positional.len() < 2 {
        return Err(
            "usage: rar a [-m0..-m5] [-p<password>] [-v<size>] <archive.rar> <files...>".into(),
        );
    }
    let archive_path = positional[0];
    let files = &positional[1..];

    let mut rar = if let Some(vol_size) = volume_size {
        let mut ar = rar5::RarArchive::create_multivolume(archive_path, vol_size)
            .map_err(|e| format!("create: {e}"))?;
        if let Some(ref pw) = password {
            ar.set_password(pw);
        }
        ar
    } else if let Some(ref pw) = password {
        rar5::RarArchive::create_with_password(archive_path, pw)
            .map_err(|e| format!("create: {e}"))?
    } else {
        rar5::RarArchive::create(archive_path).map_err(|e| format!("create: {e}"))?
    };

    for file in files {
        rar.add(file, level)
            .map_err(|e| format!("add {file}: {e}"))?;
    }

    rar.close().map_err(|e| format!("close: {e}"))?;
    if volume_size.is_some() {
        let vols = rar5::discover_volumes(std::path::Path::new(archive_path));
        println!(
            "Created {} volume(s) ({} file(s), level {level})",
            vols.len(),
            files.len()
        );
    } else {
        println!(
            "Created {archive_path} ({} file(s), level {level})",
            files.len()
        );
    }
    Ok(())
}

fn cmd_list(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: rar l <archive.rar>".into());
    }
    let rar = rar5::RarArchive::open(&args[0]).map_err(|e| format!("{e}"))?;

    println!(
        "{:>10}  {:>10}  {:>6}  {:<8}  Name",
        "Size", "Packed", "Ratio", "Method"
    );
    println!("{}", "-".repeat(60));

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

        println!(
            "{:>10}  {:>10}  {:>6}  {:<8}  {}",
            entry.size(),
            entry.compressed_size(),
            ratio,
            entry.method_name(),
            entry.name()
        );

        total_size += entry.size();
        total_packed += entry.compressed_size();
    }

    println!("{}", "-".repeat(60));
    let overall = if total_size > 0 {
        format!("{:.1}%", total_packed as f64 / total_size as f64 * 100.0)
    } else {
        " 0.0%".to_string()
    };
    println!(
        "{total_size:>10}  {total_packed:>10}  {overall:>6}  {:<8}  {} file(s)",
        "",
        rar.list().len()
    );

    Ok(())
}

fn cmd_info(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: rar i <archive.rar>".into());
    }
    let rar = rar5::RarArchive::open(&args[0]).map_err(|e| format!("{e}"))?;

    let files: Vec<_> = rar.list().iter().filter(|e| !e.is_dir()).collect();
    let dirs: Vec<_> = rar.list().iter().filter(|e| e.is_dir()).collect();
    let total_size: u64 = files.iter().map(|e| e.size()).sum();
    let total_packed: u64 = files.iter().map(|e| e.compressed_size()).sum();

    println!("Archive: {}", args[0]);
    println!("Files:   {}", files.len());
    println!("Dirs:    {}", dirs.len());
    println!("Size:    {} bytes", total_size);
    println!("Packed:  {} bytes", total_packed);
    if total_size > 0 {
        println!(
            "Ratio:   {:.1}%",
            total_packed as f64 / total_size as f64 * 100.0
        );
    }

    Ok(())
}
