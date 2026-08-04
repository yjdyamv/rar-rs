//! unrar — extract and inspect RAR5 archives.

use std::env;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        usage();
        process::exit(1);
    }

    // Extract -p<password> from args
    let mut password: Option<String> = None;
    let mut filtered_args: Vec<String> = Vec::new();
    for arg in &args[1..] {
        if let Some(pw) = arg.strip_prefix("-p") {
            password = Some(pw.to_string());
        } else {
            filtered_args.push(arg.clone());
        }
    }

    if filtered_args.is_empty() {
        usage();
        process::exit(1);
    }

    let result = match filtered_args[0].as_str() {
        "x" | "extract" => cmd_extract(&filtered_args[1..], password.as_deref()),
        "e" => cmd_extract_flat(&filtered_args[1..], password.as_deref()),
        "l" | "list" => cmd_list(&filtered_args[1..], password.as_deref()),
        "t" | "test" => cmd_test(&filtered_args[1..], password.as_deref()),
        "p" | "print" => cmd_print(&filtered_args[1..], password.as_deref()),
        "-h" | "--help" | "help" => {
            usage();
            Ok(())
        }
        other if other.ends_with(".rar") || other.ends_with(".cbr") => {
            cmd_list(&filtered_args, password.as_deref())
        }
        _ => {
            eprintln!("unknown command: {}", filtered_args[0]);
            usage();
            process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("unrar: {e}");
        process::exit(1);
    }
}

fn usage() {
    eprintln!("unrar-rs — extract RAR5 archives");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  unrar x <archive.rar> [dest/]    Extract with full paths");
    eprintln!("  unrar e <archive.rar> [dest/]    Extract flat (no paths)");
    eprintln!("  unrar l <archive.rar>            List contents");
    eprintln!("  unrar t <archive.rar>            Test integrity");
    eprintln!("  unrar p <archive.rar> [file]     Print file to stdout");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -p<password>                     Set password for encrypted archives");
}

fn open_archive(path: &str, password: Option<&str>) -> Result<rar5::RarArchive, String> {
    if let Some(pw) = password {
        rar5::RarArchive::open_with_password(path, pw).map_err(|e| format!("{e}"))
    } else {
        rar5::RarArchive::open(path).map_err(|e| format!("{e}"))
    }
}

fn cmd_extract(args: &[String], password: Option<&str>) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: unrar x <archive.rar> [dest/]".into());
    }
    let dest = if args.len() > 1 { &args[1] } else { "." };

    let mut rar = open_archive(&args[0], password)?;

    let count = rar.list().len();
    rar.extract_all(dest).map_err(|e| format!("{e}"))?;
    println!("Extracted {count} entries to {dest}");
    Ok(())
}

fn cmd_extract_flat(args: &[String], password: Option<&str>) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: unrar e <archive.rar> [dest/]".into());
    }
    let dest = if args.len() > 1 { &args[1] } else { "." };

    let mut rar = open_archive(&args[0], password)?;

    let names: Vec<String> = rar.list().iter().map(|e| e.name().to_string()).collect();
    for name in &names {
        let entry = rar.get_entry(name).unwrap();
        if entry.is_dir() {
            continue;
        }
        let data = rar.read(name).map_err(|e| format!("{name}: {e}"))?;
        let file_name = std::path::Path::new(name)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let out_path = std::path::Path::new(dest).join(file_name.as_ref());
        std::fs::write(&out_path, &data).map_err(|e| format!("{}: {e}", out_path.display()))?;
    }

    Ok(())
}

fn cmd_list(args: &[String], password: Option<&str>) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: unrar l <archive.rar>".into());
    }
    let rar = open_archive(&args[0], password)?;

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

fn cmd_test(args: &[String], password: Option<&str>) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: unrar t <archive.rar>".into());
    }
    let mut rar = open_archive(&args[0], password)?;

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

fn cmd_print(args: &[String], password: Option<&str>) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: unrar p <archive.rar> [file]".into());
    }
    let mut rar = open_archive(&args[0], password)?;

    if args.len() > 1 {
        let data = rar.read(&args[1]).map_err(|e| format!("{e}"))?;
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
