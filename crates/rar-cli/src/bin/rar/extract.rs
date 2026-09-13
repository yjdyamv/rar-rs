//! `rar x` / `rar e` / `rar p` — extraction and printing.

use crate::args::{ExtractArgs, PrintArgs};
use crate::common;
use crate::error::CliResult;
use crate::info;
use crate::ops;
use crate::output;
/// Print a member to stdout (like `rar p`).
pub(crate) fn cmd_print(args: &PrintArgs) -> CliResult<()> {
    let mut rar = ops::open_reader(&args.archive, args.password.password.as_deref())?;
    ops::print_members(&mut rar, args.file.as_deref())
}

/// Apply `-om` (Mark of the Web propagation) to the reader before
/// extraction; a no-op unless the switch is present.
fn apply_mark_web(
    rar: &mut rar_rs::ArchiveReader,
    misc: &common::MiscSwitches,
) -> Result<(), String> {
    if let Some(spec) = misc.mark_web.as_deref() {
        rar.set_mark_of_the_web(common::parse_mark_web(spec)?);
    }
    Ok(())
}

/// Extract with full paths (like `rar x`).
pub(crate) fn cmd_extract(args: &ExtractArgs, misc: &common::MiscSwitches) -> CliResult<()> {
    if let Some(threads) = args.threads {
        rar_rs::set_extraction_threads(threads);
    }
    let dest = extract_dest(args)?;
    let mut rar = ops::open_reader(&args.archive, args.password.password.as_deref())?;
    // `-so`: write the extracted members to stdout (one stream) instead of
    // to disk — handy for piping. Directories carry no data.
    if args.stdout {
        return ops::extract_to_stdout(&mut rar, &args.names, None);
    }
    apply_mark_web(&mut rar, misc)?;
    let skip = args.overwrite.as_deref() == Some("never");
    let options = rar_rs::ExtractOptions {
        skip_existing: skip,
        ..Default::default()
    };
    let count = ops::extract_members(&mut rar, &dest, &args.names, options)?;
    write_extract_logs(misc, &rar, args)?;
    info!("Extracted {count} file(s) to {}", dest.display());
    Ok(())
}

/// `-log` for extraction: archive name plus every extracted member (the
/// requested names, or all members when the selection is empty).
fn write_extract_logs(
    misc: &common::MiscSwitches,
    rar: &rar_rs::ArchiveReader,
    args: &ExtractArgs,
) -> CliResult<()> {
    let logs = crate::log::specs_from(misc)?;
    if logs.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = if args.names.is_empty() {
        rar.entries()
            .map(|entry| entry.name().to_string())
            .collect()
    } else {
        args.names.clone()
    };
    crate::log::write_logs(&logs, &[std::path::PathBuf::from(&args.archive)], &names)?;
    Ok(())
}

/// Destination directory, honoring `-ad` (append the archive base name).
pub(crate) fn extract_dest(args: &ExtractArgs) -> Result<std::path::PathBuf, String> {
    Ok(output::extract_dest(
        &args.dest,
        &args.archive,
        args.append_dir,
    ))
}

/// Extract without archived paths (like `rar e`).
pub(crate) fn cmd_extract_flat(args: &ExtractArgs, misc: &common::MiscSwitches) -> CliResult<()> {
    if let Some(threads) = args.threads {
        rar_rs::set_extraction_threads(threads);
    }
    let dest = extract_dest(args)?;
    let mut rar = ops::open_reader(&args.archive, args.password.password.as_deref())?;
    if args.stdout {
        return ops::extract_to_stdout(&mut rar, &args.names, None);
    }
    apply_mark_web(&mut rar, misc)?;
    let skip = args.overwrite.as_deref() == Some("never");
    let options = rar_rs::ExtractOptions {
        flat_paths: true,
        skip_existing: skip,
        ..Default::default()
    };
    let count = ops::extract_members(&mut rar, &dest, &args.names, options)?;
    write_extract_logs(misc, &rar, args)?;
    info!("Extracted {count} file(s) to {}", dest.display());
    Ok(())
}
