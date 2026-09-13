//! Archive editing: delete, rename, move and parameter changes.

use crate::args::FilesArgs;
use crate::args::archive_version;
use crate::args::resolve_dict_switch;
use crate::args::{ChangeArgs, DeleteArgs, RenameArgs};
use crate::common;
use crate::error::CliResult;
use crate::filters::arg_to_name;
use crate::info;
use crate::time;
/// Open a [`rar_rs::ArchiveEditor`] for the CLI, honoring the password switch.
pub(crate) fn open_editor(
    path: impl AsRef<std::path::Path>,
    password: Option<&str>,
) -> Result<rar_rs::ArchiveEditor, String> {
    match password {
        Some(pw) if !pw.is_empty() => {
            rar_rs::ArchiveEditor::open_with_password(path, pw).map_err(|e| format!("open: {e}"))
        }
        _ => rar_rs::ArchiveEditor::open(path).map_err(|e| format!("open: {e}")),
    }
}

/// Resolve delete names onto an [`rar_rs::EditPlan`] with the legacy `rar d`
/// semantics: every name deletes the first matching member that is not
/// already selected, so repeated names delete successive duplicates, and a
/// missing name fails the whole plan before any rewrite starts.
pub(crate) fn editor_delete_plan(
    editor: &rar_rs::ArchiveEditor,
    names: &[&str],
) -> Result<rar_rs::EditPlan, rar_rs::RarError> {
    let mut plan = rar_rs::EditPlan::new();
    let mut chosen: Vec<rar_rs::EntryId> = Vec::new();
    for name in names {
        let id = editor
            .entries_named(name)
            .map(|entry| entry.id())
            .find(|id| !chosen.contains(id))
            .ok_or_else(|| rar_rs::RarError::MemberNotFound {
                name: (*name).to_string(),
            })?;
        chosen.push(id);
        plan = plan.delete(id);
    }
    Ok(plan)
}

/// Resolve rename pairs onto an [`rar_rs::EditPlan`]: each old name targets
/// the first member with that stored name (trailing `/` ignored) that is
/// not already renamed in the plan; directory expansion to descendants is
/// handled by the rewrite core.
pub(crate) fn editor_rename_plan(
    editor: &rar_rs::ArchiveEditor,
    pairs: &[(&str, &str)],
) -> Result<rar_rs::EditPlan, rar_rs::RarError> {
    let mut plan = rar_rs::EditPlan::new();
    let mut chosen: Vec<rar_rs::EntryId> = Vec::new();
    for (old, new) in pairs {
        let old_norm = old.trim_end_matches('/');
        let id = editor
            .entries()
            .find(|entry| {
                entry.name().trim_end_matches('/') == old_norm && !chosen.contains(&entry.id())
            })
            .map(|entry| entry.id())
            .ok_or_else(|| rar_rs::RarError::MemberNotFound {
                name: (*old).to_string(),
            })?;
        chosen.push(id);
        plan = plan.rename(id, (*new).to_string());
    }
    Ok(plan)
}

/// Resolve chained rename pairs onto an [`rar_rs::EditPlan`] mirroring the
/// legacy name-based `rename` resolution exactly: each old name targets the
/// first member whose stored name — or already-planned rename in this call —
/// equals it, so version chains (`a.txt -> a.txt;1 -> a.txt;2`) and repeated
/// old names resolve like the legacy sequential rewrite while staying
/// index-addressed.
pub(crate) fn editor_chained_rename_plan(
    editor: &rar_rs::ArchiveEditor,
    pairs: &[(&str, &str)],
) -> Result<rar_rs::EditPlan, rar_rs::RarError> {
    let mut plan = rar_rs::EditPlan::new();
    let mut planned: std::collections::HashMap<rar_rs::EntryId, String> =
        std::collections::HashMap::new();
    for (old, new) in pairs {
        let old_norm = old.trim_end_matches('/');
        let id = editor
            .entries()
            .find(|entry| {
                planned
                    .get(&entry.id())
                    .map(String::as_str)
                    .unwrap_or_else(|| entry.name())
                    .trim_end_matches('/')
                    == old_norm
            })
            .map(|entry| entry.id())
            .ok_or_else(|| rar_rs::RarError::MemberNotFound {
                name: (*old).to_string(),
            })?;
        planned.insert(id, (*new).to_string());
        plan = plan.rename(id, (*new).to_string());
    }
    Ok(plan)
}

/// Delete members from an archive without rebuilding it (mirrors `rar d`).
pub(crate) fn cmd_delete(args: &DeleteArgs, misc: &common::MiscSwitches) -> CliResult<()> {
    let archive_path = &args.archive;
    let logs = crate::log::specs_from(misc)?;
    let expanded = crate::listfile::expand(&args.names, misc.list_files.as_deref())?;
    if expanded.is_empty() {
        // WinRAR treats `d` without members as a successful no-op.
        return Ok(());
    }
    let names: Vec<&str> = expanded.iter().map(|s| s.as_str()).collect();
    let mut editor = open_editor(archive_path, args.password.password.as_deref())?;
    let plan = editor_delete_plan(&editor, &names)
        .map_err(|e| crate::error::CliError::from(e).context("delete"))?;
    let deleted = editor
        .apply(plan)
        .map_err(|e| crate::error::CliError::from(e).context("delete"))?
        .deleted();
    info!("Deleted {deleted} file(s) from {archive_path}");
    if !logs.is_empty() {
        crate::log::write_logs(&logs, &[std::path::PathBuf::from(archive_path)], &expanded)?;
    }
    Ok(())
}

/// Rename archived members (like `rar rn`): pairs of old/new names.
pub(crate) fn cmd_rename(args: &RenameArgs) -> CliResult<()> {
    if !args.pairs.len().is_multiple_of(2) {
        return Err("usage: rar rn <archive.rar> <old1> <new1> [<old2> <new2> ...]".into());
    }
    let archive_path = &args.archive;
    let pairs: Vec<(&str, &str)> = args
        .pairs
        .chunks(2)
        .map(|c| (c[0].as_str(), c[1].as_str()))
        .collect();
    // A `-hp` archive needs its password: the rename rewrites the encrypted
    // FILE_HEAD.
    let mut editor = open_editor(archive_path, args.password.password.as_deref())?;
    let plan = editor_rename_plan(&editor, &pairs)
        .map_err(|e| crate::error::CliError::from(e).context("rename"))?;
    let renamed = editor
        .apply(plan)
        .map_err(|e| crate::error::CliError::from(e).context("rename"))?
        .renamed();
    info!("Renamed {renamed} file(s) in {archive_path}");
    Ok(())
}

/// Move files into the archive (like `rar m`): add them through the typed
/// writer, then erase the sources after a successful commit. `files_only`
/// (`rar mf`) skips directory entries and removes only files.
pub(crate) fn cmd_move(
    args: &FilesArgs,
    misc: &common::MiscSwitches,
    files_only: bool,
) -> CliResult<()> {
    let archive_path = &args.archive;
    let files = &args.files;
    let password = &args.password.password;
    for file in files {
        let path = std::path::Path::new(file);
        if !path.exists() {
            return Err(format!("path not found: {file}").into());
        }
    }
    let (dict_size_log, dict_size_bytes) = match args.dict_size.as_deref() {
        Some(s) => resolve_dict_switch(s, args.archive_format.as_deref())?,
        None => (None, None),
    };
    let (version, v70_dict_bytes) = archive_version(
        args.archive_format.as_deref(),
        dict_size_log,
        dict_size_bytes,
    )?;
    let dictionary = if version.is_legacy() {
        None
    } else {
        v70_dict_bytes
            .or(dict_size_bytes)
            .or_else(|| dict_size_log.map(|log| (128u64 * 1024) << log))
            .map(|bytes| {
                rar_rs::DictionarySize::try_from(bytes)
                    .map_err(|error| format!("dictionary: {error}"))
            })
            .transpose()?
    };
    let mut writer = if std::path::Path::new(archive_path).exists() {
        let mut append_opts = rar_rs::AppendOptions::new();
        if let Some(pw) = password {
            append_opts = append_opts.password(pw.clone());
        }
        if let Some(size) = dictionary {
            append_opts = append_opts.dictionary_size(size);
        }
        rar_rs::ArchiveWriter::append_with(archive_path, append_opts)
            .map_err(|e| format!("open: {e}"))?
    } else {
        let ts = time::parse_ts_specs(&args.ts_specs)?;
        let mut writer_opts = rar_rs::WriterOptions::new()
            .compression(version)
            .save_ctime(ts.save_ctime)
            .save_atime(ts.save_atime)
            .save_mtime(ts.save_mtime)
            .save_owner(misc.owner)
            .save_streams(misc.save_streams)
            .filters(match args.mc_params.as_deref() {
                Some(spec) => crate::args::parse_mc_params(spec),
                None => Default::default(),
            })
            .time_precision_seconds(ts.precision_seconds);
        if let Some(pw) = password {
            writer_opts = writer_opts.password(pw.clone());
        }
        if let Some(size) = dictionary {
            writer_opts = writer_opts.dictionary_size(size);
        }
        rar_rs::ArchiveWriter::create_with(archive_path, writer_opts)
            .map_err(|e| format!("create: {e}"))?
    };
    let options = rar_rs::EntryWriteOptions::new().compression_level(
        rar_rs::CompressionLevel::try_from(3).map_err(|e| format!("level: {e}"))?,
    );
    // Both `m` and `mf` archive the full tree (directory entries included);
    // they differ only in what is removed from disk afterwards.
    let mut moved = 0usize;
    for file in files {
        let name = arg_to_name(file);
        writer
            .add_path_as(file, &name, options)
            .map_err(|e| format!("add {file}: {e}"))?;
        moved += 1;
    }
    writer.finish().map_err(|e| format!("close: {e}"))?;
    for file in files {
        let path = std::path::Path::new(file);
        if path.is_dir() {
            if files_only {
                remove_files_leaving_dirs(path);
            } else {
                let _ = std::fs::remove_dir_all(path);
            }
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
    info!("Moved {moved} file(s) to {archive_path}");
    Ok(())
}

/// Remove every file under `dir` (recursively) but leave the directory tree
/// itself in place — `rar mf` archives the tree like `m` but only moves the
/// files.
fn remove_files_leaving_dirs(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            remove_files_leaving_dirs(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Change archive parameters (like `rar ch`): member name case conversion
/// with `-cl` / `-cu`.
pub(crate) fn cmd_change(args: &ChangeArgs) -> CliResult<()> {
    let kind = match (args.lowercase, args.uppercase) {
        (true, false) => crate::name_policy::CaseKind::Lower,
        (false, true) => crate::name_policy::CaseKind::Upper,
        _ => return Err("usage: rar ch [-cl | -cu] <archive.rar>".into()),
    };
    let mut editor = match &args.password.password {
        Some(pw) if !pw.is_empty() => rar_rs::ArchiveEditor::open_with_password(&args.archive, pw)
            .map_err(|e| format!("open: {e}"))?,
        _ => rar_rs::ArchiveEditor::open(&args.archive).map_err(|e| format!("open: {e}"))?,
    };
    let names: Vec<String> = editor
        .entries()
        .map(|entry| entry.name().to_string())
        .collect();
    let mut pairs = Vec::new();
    for name in names {
        let converted = match kind {
            crate::name_policy::CaseKind::Lower => name.to_lowercase(),
            crate::name_policy::CaseKind::Upper => name.to_uppercase(),
        };
        if converted != name {
            pairs.push((name, converted));
        }
    }
    if pairs.is_empty() {
        info!("{archive}: no names to convert", archive = args.archive);
        return Ok(());
    }
    let pairs_ref: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let plan = editor_rename_plan(&editor, &pairs_ref).map_err(|e| format!("ch: {e}"))?;
    let renamed = editor
        .apply(plan)
        .map_err(|e| format!("ch: {e}"))?
        .renamed();
    info!(
        "Converted {renamed} name(s) in {archive}",
        archive = args.archive
    );
    Ok(())
}
