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
pub(crate) fn cmd_extract(
    args: &ExtractArgs,
    misc: &common::MiscSwitches,
    assume_yes: bool,
) -> CliResult<()> {
    if let Some(threads) = args.threads {
        rar_rs::set_extraction_threads(threads);
    }
    let (names, dest) = extract_names_and_dest(args, misc)?;
    let mut rar = ops::open_reader(&args.archive, args.password.password.as_deref())?;
    // `-so`: write the extracted members to stdout (one stream) instead of
    // to disk — handy for piping. Directories carry no data.
    if args.stdout {
        return ops::extract_to_stdout(&mut rar, &names, None);
    }
    apply_mark_web(&mut rar, misc)?;
    let options = rar_rs::ExtractOptions {
        skip_existing: output::skip_existing(
            args.overwrite.as_deref(),
            assume_yes,
            args.auto_rename,
        ),
        auto_rename: args.auto_rename,
        keep_broken: args.keep_broken,
        skip_links: misc.skip_links,
        allow_unsafe_links: misc.unsafe_links,
        ..Default::default()
    };
    let count = ops::extract_members(&mut rar, &dest, &names, options)?;
    write_extract_logs(misc, &rar, args, &names)?;
    info!("Extracted {count} file(s) to {}", dest.display());
    Ok(())
}

/// `-log` for extraction: archive name plus every extracted member (the
/// requested names, or all members when the selection is empty).
fn write_extract_logs(
    misc: &common::MiscSwitches,
    rar: &rar_rs::ArchiveReader,
    args: &ExtractArgs,
    names: &[String],
) -> CliResult<()> {
    let logs = crate::log::specs_from(misc)?;
    if logs.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = if names.is_empty() {
        rar.entries()
            .map(|entry| entry.name().to_string())
            .collect()
    } else {
        names.to_vec()
    };
    crate::log::write_logs(&logs, &[std::path::PathBuf::from(&args.archive)], &names)?;
    Ok(())
}

/// Expand `@listfiles` and split off a trailing positional destination
/// (WinRAR: the last argument is the destination when it ends with a path
/// separator and `--dest` was not given).
fn extract_names_and_dest(
    args: &ExtractArgs,
    misc: &common::MiscSwitches,
) -> Result<(Vec<String>, std::path::PathBuf), String> {
    let mut names = crate::listfile::expand(&args.names, misc.list_files.as_deref())?;
    let mut dest = args.dest.clone().unwrap_or_else(|| ".".to_string());
    if args.dest.is_none()
        && let Some(last) = names.last()
        && (last.ends_with('/') || last.ends_with('\\'))
    {
        dest = names.pop().expect("checked above");
    }
    let base = args.output_path.as_deref().unwrap_or(&dest);
    let mode = output::parse_append_dir(args.append_dir.as_deref())?;
    Ok((names, output::extract_dest(base, &args.archive, mode)))
}

/// Extract without archived paths (like `rar e`).
pub(crate) fn cmd_extract_flat(
    args: &ExtractArgs,
    misc: &common::MiscSwitches,
    assume_yes: bool,
) -> CliResult<()> {
    if let Some(threads) = args.threads {
        rar_rs::set_extraction_threads(threads);
    }
    let (names, dest) = extract_names_and_dest(args, misc)?;
    let mut rar = ops::open_reader(&args.archive, args.password.password.as_deref())?;
    if args.stdout {
        return ops::extract_to_stdout(&mut rar, &names, None);
    }
    apply_mark_web(&mut rar, misc)?;
    let options = rar_rs::ExtractOptions {
        flat_paths: true,
        skip_existing: output::skip_existing(
            args.overwrite.as_deref(),
            assume_yes,
            args.auto_rename,
        ),
        auto_rename: args.auto_rename,
        keep_broken: args.keep_broken,
        skip_links: misc.skip_links,
        allow_unsafe_links: misc.unsafe_links,
        ..Default::default()
    };
    let count = ops::extract_members(&mut rar, &dest, &names, options)?;
    write_extract_logs(misc, &rar, args, &names)?;
    info!("Extracted {count} file(s) to {}", dest.display());
    Ok(())
}
