//! `rar x` / `rar e` / `rar p` — extraction and printing.

use crate::args::{ExtractArgs, PrintArgs};
use crate::common;
use crate::error;
use crate::error::CliResult;
use crate::info;
use crate::ops;
/// Print a member to stdout (like `rar p`).
pub(crate) fn cmd_print(args: &PrintArgs) -> CliResult<()> {
    let max_dict_size = dict_cap(args.dict_extract.as_deref())?;
    let mut rar = ops::open_reader(&args.archive, args.password.password.as_deref())?;
    ops::print_members(&mut rar, args.file.as_deref(), max_dict_size)
}

/// `-mdx`: extraction dictionary cap; no unit means GiB (WinRAR).
fn dict_cap(spec: Option<&str>) -> Result<Option<u64>, String> {
    spec.map(common::parse_mdx_size).transpose()
}

/// Extract with full paths (like `rar x`).
pub(crate) fn cmd_extract(
    args: &ExtractArgs,
    misc: &common::MiscSwitches,
    assume_yes: bool,
) -> CliResult<()> {
    let max_dict_size = dict_cap(args.dict_extract.as_deref())?;
    let ts = crate::time::parse_ts_specs(&args.ts_specs)?;
    let (names, dest) = resolve_target(args, misc)?;
    let request = ops::ExtractRequest {
        names,
        dest,
        stdout: args.stdout,
        threads: args.threads,
        max_dict_size,
        mark_web: common::mark_web(misc.mark_web.as_deref())?,
        overwrite: args.overwrite.clone(),
        assume_yes,
        auto_rename: args.auto_rename,
        freshen: misc.freshen,
        update: misc.update_files,
        keep_broken: args.keep_broken,
        skip_links: misc.skip_links,
        allow_unsafe_links: misc.unsafe_links,
        set_creation_time: ts.save_ctime,
        set_access_time: ts.save_atime,
        ..ops::ExtractRequest::default()
    };
    let mut rar = ops::open_reader(&args.archive, args.password.password.as_deref())?;
    if let Some(report) = ops::extract(&mut rar, &request)? {
        write_extract_logs(misc, &rar, args, &request.names)?;
        info!("{}", extract_summary(report.written_count(), &request.dest));
        if report.written_count() == 0 && report.skipped_count() > 0 {
            // Like WinRAR/UnRAR: nothing was extracted because every selected
            // member was left untouched -> "No files to extract", exit 10.
            return Err(error::CliError::silent(error::EXIT_NO_FILES));
        }
    }
    Ok(())
}

/// Final extraction line; when every file was left alone (all existing and
/// `-o-`, or an empty archive) WinRAR prints "No files to extract".
fn extract_summary(count: usize, dest: &std::path::Path) -> String {
    if count == 0 {
        "No files to extract".to_string()
    } else {
        format!("Extracted {count} file(s) to {}", dest.display())
    }
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

/// Extract without archived paths (like `rar e`).
pub(crate) fn cmd_extract_flat(
    args: &ExtractArgs,
    misc: &common::MiscSwitches,
    assume_yes: bool,
) -> CliResult<()> {
    let max_dict_size = dict_cap(args.dict_extract.as_deref())?;
    let ts = crate::time::parse_ts_specs(&args.ts_specs)?;
    let (names, dest) = resolve_target(args, misc)?;
    let request = ops::ExtractRequest {
        names,
        dest,
        flat: true,
        stdout: args.stdout,
        threads: args.threads,
        max_dict_size,
        mark_web: common::mark_web(misc.mark_web.as_deref())?,
        overwrite: args.overwrite.clone(),
        assume_yes,
        auto_rename: args.auto_rename,
        freshen: misc.freshen,
        update: misc.update_files,
        keep_broken: args.keep_broken,
        skip_links: misc.skip_links,
        allow_unsafe_links: misc.unsafe_links,
        set_creation_time: ts.save_ctime,
        set_access_time: ts.save_atime,
    };
    let mut rar = ops::open_reader(&args.archive, args.password.password.as_deref())?;
    if let Some(report) = ops::extract(&mut rar, &request)? {
        write_extract_logs(misc, &rar, args, &request.names)?;
        info!("{}", extract_summary(report.written_count(), &request.dest));
        if report.written_count() == 0 && report.skipped_count() > 0 {
            return Err(error::CliError::silent(error::EXIT_NO_FILES));
        }
    }
    Ok(())
}
