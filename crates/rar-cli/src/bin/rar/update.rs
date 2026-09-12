//! `rar u` / `rar f` — transactional update and freshen.

use crate::args::archive_version;
use crate::args::resolve_dict_switch;
use crate::args::{FilesArgs, collect_inputs};
use crate::common;
use crate::edit::editor_chained_rename_plan;
use crate::edit::editor_delete_plan;
use crate::edit::open_editor;
use crate::error::CliResult;
use crate::info;
use crate::ops;
use crate::staging::update_archive_transactionally;
use crate::time;
/// Update an archive: add files not present, replace files whose source
/// is newer (like `rar u`).
pub(crate) fn cmd_update(args: &FilesArgs, misc: &common::MiscSwitches) -> CliResult<()> {
    cmd_update_freshen(args, false, "Updated", misc)
}

/// Freshen the archive (like `rar f`): update members that already exist
/// when the source is newer; never add new members.
pub(crate) fn cmd_freshen(args: &FilesArgs, misc: &common::MiscSwitches) -> CliResult<()> {
    cmd_update_freshen(args, true, "Freshened", misc)
}

/// Shared transactional update/freshen implementation. Source arguments are
/// expanded with the create command's collector. Every mutation is applied to
/// a same-directory copy and the original is atomically replaced only after
/// the complete delete/rename/append/close sequence succeeds.
fn cmd_update_freshen(
    args: &FilesArgs,
    freshen: bool,
    verb: &str,
    misc: &common::MiscSwitches,
) -> CliResult<()> {
    let archive_path = std::path::Path::new(&args.archive);
    let password = &args.password.password;
    if !archive_path.exists() {
        return Err(format!("archive not found: {}", archive_path.display()).into());
    }
    if rar_rs::discover_volumes(archive_path).len() > 1 {
        return Err("transactional update of multi-volume archives is not supported".into());
    }

    // Validate all operation options before allocating the staged copy.
    let (dict_size_log, dict_size_bytes) = match args.dict_size.as_deref() {
        Some(spec) => resolve_dict_switch(spec, args.archive_format.as_deref())?,
        None => (None, None),
    };
    let (version, v70_dict_bytes) = archive_version(
        args.archive_format.as_deref(),
        dict_size_log,
        dict_size_bytes,
    )?;
    // The version table is the single write knob: `-ma7` selects v70
    // (every member v70); `-ma5`/default keep v50 with the legacy auto
    // v50/v70 semantics for > 4 GiB `-md` requests; `-ma4` selects v29.
    // The legacy v29 pipeline never takes a dictionary.
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
    let ts = time::parse_ts_specs(&args.ts_specs)?;
    let original_mtime = if args.keep_time {
        Some(
            std::fs::metadata(archive_path)
                .and_then(|metadata| metadata.modified())
                .map_err(|error| format!("read archive modification time: {error}"))?,
        )
    } else {
        None
    };

    let collected = collect_inputs(
        &crate::name_policy::NamePolicy::default(),
        &args.files,
        3,
        &args.archive,
    )?;
    let archive = ops::open_reader(archive_path, password.as_deref())
        .map_err(|error| format!("open: {error}"))?;
    let mut to_delete = Vec::new();
    let mut to_add = Vec::new();
    for item in &collected {
        let source_mtime = std::fs::metadata(&item.path)
            .and_then(|metadata| metadata.modified())
            .map_err(|error| format!("read source metadata {}: {error}", item.path.display()))?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let source_mtime = u32::try_from(source_mtime).unwrap_or(u32::MAX);
        if let Some(entry) = archive.entries_named(&item.name).next() {
            if source_mtime > entry.mtime() {
                to_delete.push(item.name.clone());
                to_add.push(item.clone());
            }
        } else if !freshen {
            to_add.push(item.clone());
        }
    }
    drop(archive);

    // -ol / -oh: symlinks and hard links become redirect members (RAR4 has
    // no redirect records, and WinRAR's `-ma4 -oh` stores full files).
    let (to_add, redirects) = crate::links::split_link_redirects(
        to_add,
        args.store_links,
        args.store_hardlinks && !version.is_legacy(),
    );

    if to_delete.is_empty() && to_add.is_empty() && redirects.is_empty() {
        info!("{}: no files to {verb}", archive_path.display());
        return Ok(());
    }

    let updated_count = to_add.len();
    update_archive_transactionally(archive_path, |staged_path| {
        if !to_delete.is_empty() {
            if let Some(version_spec) = &misc.version_control {
                // Version control chains renames inside one call with
                // map-aware resolution (a.txt -> a.txt;1 -> a.txt;2), then
                // drops versions above the cap. Both run through the editor
                // role in two atomic rewrites — renames re-emit only the
                // headers (never recompressing), while the drop may
                // recompress the solid chain, so they stay split exactly
                // like the legacy sequential calls.
                let mut editor = open_editor(staged_path, password.as_deref())
                    .map_err(|error| format!("open staged archive: {error}"))?;
                let max_versions = if version_spec.is_empty() {
                    None
                } else {
                    version_spec.parse::<u32>().ok().filter(|count| *count > 0)
                };
                let mut renames = Vec::new();
                let mut to_drop = Vec::new();
                for name in &to_delete {
                    let mut versions: Vec<(u32, String)> = editor
                        .entries()
                        .filter_map(|entry| {
                            let member = entry.name();
                            if member == *name {
                                Some((0, member.to_string()))
                            } else if let Some(suffix) = member.strip_prefix(&format!("{name};"))
                                && let Ok(version) = suffix.parse::<u32>()
                            {
                                Some((version, member.to_string()))
                            } else {
                                None
                            }
                        })
                        .collect();
                    versions.sort_by_key(|(version, _)| *version);
                    for (version, member) in versions.iter().rev() {
                        let new_suffix = version
                            .checked_add(1)
                            .ok_or_else(|| format!("version number overflow for {member}"))?;
                        if max_versions.is_some_and(|limit| new_suffix > limit) {
                            to_drop.push(member.clone());
                        } else {
                            let new_name = if *version == 0 {
                                format!("{name};1")
                            } else {
                                format!("{name};{new_suffix}")
                            };
                            renames.push((member.clone(), new_name));
                        }
                    }
                }
                if !renames.is_empty() {
                    let pairs: Vec<(&str, &str)> = renames
                        .iter()
                        .map(|(old, new)| (old.as_str(), new.as_str()))
                        .collect();
                    let plan = editor_chained_rename_plan(&editor, &pairs)
                        .map_err(|error| format!("rename staged members: {error}"))?;
                    editor
                        .apply(plan)
                        .map_err(|error| format!("rename staged members: {error}"))?;
                }
                if !to_drop.is_empty() {
                    let names: Vec<&str> = to_drop.iter().map(String::as_str).collect();
                    let plan = editor_delete_plan(&editor, &names)
                        .map_err(|error| format!("delete staged versions: {error}"))?;
                    editor
                        .apply(plan)
                        .map_err(|error| format!("delete staged versions: {error}"))?;
                }
            } else {
                // Plain replacement delete (no version control) runs through
                // the editor role in one atomic rewrite.
                let mut editor = open_editor(staged_path, password.as_deref())
                    .map_err(|error| format!("open staged archive: {error}"))?;
                let names: Vec<&str> = to_delete.iter().map(String::as_str).collect();
                let plan = editor_delete_plan(&editor, &names)
                    .map_err(|error| format!("delete staged members: {error}"))?;
                editor
                    .apply(plan)
                    .map_err(|error| format!("delete staged members: {error}"))?;
            }
        }

        let mut staged = if staged_path.exists() {
            let mut append_opts = rar_rs::AppendOptions::new();
            if let Some(value) = password {
                append_opts = append_opts.password(value.clone());
            }
            if let Some(size) = dictionary {
                append_opts = append_opts.dictionary_size(size);
            }
            rar_rs::ArchiveWriter::append_with(staged_path, append_opts)
                .map_err(|error| format!("open staged archive for append: {error}"))?
        } else {
            let mut writer_opts = rar_rs::WriterOptions::new()
                .compression(version)
                .save_ctime(ts.save_ctime)
                .save_atime(ts.save_atime)
                .save_mtime(ts.save_mtime)
                .save_owner(misc.owner)
                .save_streams(misc.save_streams)
                .time_precision_seconds(ts.precision_seconds);
            if let Some(value) = password {
                writer_opts = writer_opts.password(value.clone());
            }
            if let Some(size) = dictionary {
                writer_opts = writer_opts.dictionary_size(size);
            }
            rar_rs::ArchiveWriter::create_with(staged_path, writer_opts)
                .map_err(|error| format!("recreate staged archive: {error}"))?
        };
        let mut write_entries: Vec<rar_rs::WriteEntry<'_>> = Vec::with_capacity(to_add.len());
        for item in &to_add {
            let options = rar_rs::EntryWriteOptions::new().compression_level(
                rar_rs::CompressionLevel::try_from(item.level)
                    .map_err(|error| format!("level: {error}"))?,
            );
            if item.is_dir {
                write_entries.push(rar_rs::WriteEntry::Directory {
                    path: &item.path,
                    name: Some(&item.name),
                });
            } else {
                write_entries.push(rar_rs::WriteEntry::File {
                    path: &item.path,
                    name: Some(&item.name),
                    options,
                });
            }
        }
        staged
            .add_batch(&write_entries)
            .map_err(|error| format!("append staged members: {error}"))?;
        for (name, redir_type, target) in &redirects {
            staged
                .add_redirect(name, *redir_type, target)
                .map_err(|error| format!("link {name}: {error}"))?;
        }
        staged
            .finish()
            .map_err(|error| format!("close staged archive: {error}"))?;

        if let Some(modified) = original_mtime {
            std::fs::File::options()
                .write(true)
                .open(staged_path)
                .and_then(|file| file.set_times(std::fs::FileTimes::new().set_modified(modified)))
                .map_err(|error| format!("restore staged archive time: {error}"))?;
        }
        Ok(())
    })?;

    info!(
        "{verb} {} ({updated_count} file(s))",
        archive_path.display()
    );
    Ok(())
}
