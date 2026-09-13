//! `rar a` — create or append with the full switch surface.

use crate::args::RecoveryVolumes;
use crate::args::archive_version;
use crate::args::resolve_dict_switch;
use crate::args::{CreateArgs, collect_inputs, store_type_matches};
use crate::common;
use crate::edit::editor_delete_plan;
use crate::edit::open_editor;
use crate::error;
use crate::error::CliResult;
use crate::filters::TimeKind;
use crate::filters::file_time;
use crate::filters::parse_period_filter;
use crate::filters::parse_rar_date;
use crate::filters::read_mask_file;
use crate::info;
use crate::input;
use crate::list::apply_rarfiles_order;
use crate::ops;
use crate::time;
use std::process;
pub(crate) fn cmd_create(args: &CreateArgs, misc: &common::MiscSwitches) -> CliResult<()> {
    if let Some(threads) = args.threads {
        rar_rs::set_compression_threads(threads);
        rar_rs::set_extraction_threads(threads);
    }
    if args.wipe {
        return Err("-dw/--wipe is not supported; no source files were deleted".into());
    }
    if args.recycle_bin {
        return Err("-dr/--recycle-bin is not supported; no source files were deleted".into());
    }
    let mut password = args.password.password.clone();
    // `-p-` normalizes to an empty value and explicitly disables password
    // use. Bare `-p` was rejected before clap parsing.
    if password.as_deref() == Some("") {
        password = None;
    }
    let mut recovery_volumes_percent = None;
    let mut recovery_volume_count = None;
    if let Some(rv) = args.recovery_volumes {
        match rv {
            RecoveryVolumes::Count(n) => recovery_volume_count = Some(n),
            RecoveryVolumes::Percent(p) => recovery_volumes_percent = Some(p),
        }
    }
    let case = match (args.lowercase, args.uppercase) {
        (true, false) => Some(crate::name_policy::CaseKind::Lower),
        (false, true) => Some(crate::name_policy::CaseKind::Upper),
        _ => None,
    };
    let header_encrypt = args.header_encrypt.is_some();
    if let Some(pw) = &args.header_encrypt
        && !pw.is_empty()
    {
        password = Some(pw.clone());
    }
    let ts = time::parse_ts_specs(&args.ts_specs)?;

    let mut archive_path = args.archive.clone();
    // -ag: generate the archive name from the current date (default format
    // YYYYMMDDHHMMSS, like WinRAR): `*` in the name is replaced, otherwise
    // the stamp is inserted before the extension.
    if let Some(fmt) = &args.auto_name {
        // WinRAR stamps the archive name with the current *local* time.
        let (y, mo, d, hour, minute, second) = time::local_civil_now();
        let stamp = time::format_auto_name(fmt, y, mo, d, hour, minute, second);
        archive_path = if archive_path.contains('*') {
            archive_path.replace('*', &stamp)
        } else if let Some(dot) = archive_path.rfind('.') {
            archive_path.insert_str(dot, &stamp);
            archive_path
        } else {
            archive_path.push_str(&stamp);
            archive_path
        };
    }
    // WinRAR appends `.rar` when the archive name carries no extension.
    if std::path::Path::new(&archive_path).extension().is_none() {
        archive_path.push_str(".rar");
    }
    let archive_path = &archive_path;
    let mut files = crate::listfile::expand(&args.files, misc.list_files.as_deref())
        .map_err(error::CliError::from)?;
    // WinRAR implies `*.*` when neither files nor listfiles are given
    // (`-si` supplies its own member instead).
    if args.files.is_empty() && args.stdin_name.is_none() {
        files = vec!["*.*".to_string()];
    }
    let files = &files;

    let (dict_size_log, dict_size_bytes) = match args.dict_size.as_deref() {
        Some(s) => resolve_dict_switch(s, args.archive_format.as_deref())?,
        None => (None, None),
    };
    let (version, v70_dict_bytes) = archive_version(
        args.archive_format.as_deref(),
        dict_size_log,
        dict_size_bytes,
    )?;
    // The RAR5 editor cannot rewrite headers of a header-encrypted archive
    // (`-hp`): renaming and comment changes would corrupt it, so refuse the
    // operations up front instead of committing an archive without them.
    // The RAR4 editor supports `-hp` comments; RAR 1.3/1.4 rejects `-hp`.
    if header_encrypt && !version.is_legacy() && !version.is_rar13() {
        if misc.comment_file.is_some() {
            return Err(
                "-z/--comment-file is not supported for header-encrypted (RAR5 -hp) archives"
                    .into(),
            );
        }
        if misc.lock {
            return Err(
                "-k/--lock is not supported for header-encrypted (RAR5 -hp) archives".into(),
            );
        }
    }
    // The version table is the single write knob: `-ma4` selects v29 (the
    // legacy RAR4 pipeline), `-ma7` v70 (every member v70, 32 MiB default
    // dictionary), and `-ma5`/default v50 — a > 4 GiB `-md` keeps the
    // legacy auto mode (v70 members only when a member's effective
    // dictionary exceeds 4 GiB — exactly the bytes the writer produced
    // before).
    // The `-md` dictionary in bytes. The legacy v29 pipeline never
    // receives one: its writer picks the per-member window internally,
    // and the legacy CLI silently ignored `-md` there.
    let dictionary = if version.is_legacy() || version.is_rar13() {
        None
    } else {
        v70_dict_bytes
            .or(dict_size_bytes)
            .or_else(|| dict_size_log.map(|log| (128u64 * 1024) << log))
            .map(|bytes| {
                rar_rs::DictionarySize::try_from(bytes).map_err(|e| format!("dictionary: {e}"))
            })
            .transpose()?
    };
    // `--solid-reset` implies solid mode unless `-s-`-style off (the CLI
    // has no explicit off switch, matching WinRAR: `-sd`/`-sv`/`-se` all
    // enable solid creation).
    let solid_mode = match (
        args.solid_reset.as_str(),
        args.solid || args.solid_params.is_some() || args.solid_reset != "continuous",
    ) {
        (_, false) => rar_rs::SolidMode::Disabled,
        ("volume", _) => rar_rs::SolidMode::PerVolume,
        ("extension", _) => rar_rs::SolidMode::PerExtension,
        _ => rar_rs::SolidMode::Continuous,
    };
    let opts = rar_rs::WriterOptions::new()
        .compression(version)
        .solid_mode(solid_mode)
        .quick_open(args.quick_open && !args.no_quick_open)
        .blake2(args.blake2)
        .encrypt_headers(header_encrypt)
        .filters(match args.mc_params.as_deref() {
            Some(spec) => crate::args::parse_mc_params(spec),
            None => Default::default(),
        });
    let opts = if let Some(pw) = &password {
        opts.password(pw.clone())
    } else {
        opts
    };
    let opts = if let Some(percent) = args.recovery_percent {
        opts.recovery_percent(percent)
    } else {
        opts
    };
    let opts = if let Some(percent) = recovery_volumes_percent {
        opts.recovery_volumes_percent(percent)
    } else {
        opts
    };
    let opts = if let Some(count) = recovery_volume_count {
        opts.recovery_volume_count(count)
    } else {
        opts
    };
    let opts = if let Some(size) = args.volume_size {
        opts.volume_size(size)
    } else {
        opts
    };
    let opts = if let Some(size) = dictionary {
        opts.dictionary_size(size)
    } else {
        opts
    };
    let opts = if let Some(threads) = args.threads {
        opts.thread_count(
            rar_rs::ThreadCount::try_from(threads).map_err(|e| format!("threads: {e}"))?,
        )
    } else {
        opts
    };
    let opts = opts
        .save_ctime(ts.save_ctime)
        .save_atime(ts.save_atime)
        .save_mtime(ts.save_mtime)
        .save_owner(misc.owner)
        .save_streams(misc.save_streams)
        .time_precision_seconds(ts.precision_seconds);

    let existing = std::path::Path::new(archive_path).exists();
    // -tk: keep the archive's original modification time on update, or set
    // the archive time to the given local date.
    let keep_original_time = args.keep_time.as_deref() == Some("");
    let tk_date = match args.keep_time.as_deref() {
        Some(spec) if !spec.is_empty() => {
            Some(time::parse_tk_date(spec).map_err(|e| format!("-tk: {e}"))?)
        }
        _ => None,
    };
    let orig_mtime = if keep_original_time && existing {
        std::fs::metadata(archive_path)
            .and_then(|m| m.modified())
            .ok()
    } else {
        None
    };
    // The writer is opened lazily, after the candidate list is final: for an
    // existing archive the same-named members are replaced (deleted) first,
    // so the append handle is only opened after that rewrite; a new archive
    // is created just before the first add. This also keeps `-oi3`/`-oi4`
    // (identical-file listings) from creating an archive at all.

    let mut include_masks = args.include_masks.clone();
    let mut exclude_masks = args.exclude_masks.clone();
    for file in &args.include_list_files {
        include_masks.extend(read_mask_file(file)?);
    }
    for file in &args.exclude_list_files {
        exclude_masks.extend(read_mask_file(file)?);
    }
    let policy = crate::name_policy::NamePolicy {
        path_prefix: args.path_prefix.clone(),
        exclude_prefix: args.exclude_prefix.clone(),
        basename_only: args.basename_only,
        strip_base: args.strip_base,
        full_paths: args.full_paths,
        full_paths_drive: args.full_paths_drive,
        no_recurse: args.no_recurse,
        wildcard_top_only: args.recurse_zero,
        case: case.map(|c| match c {
            crate::name_policy::CaseKind::Lower => crate::name_policy::CaseKind::Lower,
            crate::name_policy::CaseKind::Upper => crate::name_policy::CaseKind::Upper,
        }),
        include_masks,
        exclude_masks,
    };
    let mut collected = collect_inputs(&policy, files, args.level, archive_path)?;
    // -ms<list>: files matching one of the listed types (extensions or
    // wildcard masks, semicolon-separated, repeatable) are stored without
    // compression (level 0), like WinRAR.
    if !args.store_types.is_empty() {
        let masks: Vec<String> = args
            .store_types
            .iter()
            .flat_map(|l| l.split(';'))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        for c in collected.iter_mut() {
            if !c.is_dir && masks.iter().any(|m| store_type_matches(m, &c.name)) {
                c.level = 0;
            }
        }
    }

    // -sl / -sm / -ed: size filters and skip-empty-directories.
    if args.size_less.is_some() || args.size_more.is_some() || args.no_empty_dirs {
        collected.retain(|c| {
            if c.is_dir {
                if args.no_empty_dirs {
                    return std::fs::read_dir(&c.path)
                        .map(|mut it| it.next().is_some())
                        .unwrap_or(true);
                }
                return true;
            }
            let size = std::fs::metadata(&c.path).map(|m| m.len()).unwrap_or(0);
            let less_ok = args.size_less.is_none_or(|s| size < s);
            let more_ok = args.size_more.is_none_or(|s| size > s);
            less_ok && more_ok
        });
    }
    // Time filters (-ta / -tb absolute dates, -tn / -to relative periods):
    // only members whose time falls in the window are added (directories
    // always pass). `-tn<period>` keeps files with time >= now - period
    // (exact match included), `-to<period>` keeps time < now - period
    // (exact match excluded); WinRAR treats an unparsable/empty period as 0
    // seconds. Multiple -tn/-to switches combine with AND logic.
    if args.after.is_some()
        || args.before.is_some()
        || !args.tn_filters.is_empty()
        || !args.to_filters.is_empty()
    {
        let after = args.after.as_deref().map(parse_rar_date).transpose()?;
        let before = args.before.as_deref().map(parse_rar_date).transpose()?;
        // Compare in nanosecond precision (like WinRAR): whole-second
        // truncation would wrongly drop files created within the same
        // second as the run, e.g. `-to` with an empty period.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let filters: Vec<(TimeKind, u64, bool)> = args
            .tn_filters
            .iter()
            .map(|s| {
                let (k, p) = parse_period_filter(s);
                (k, p, true)
            })
            .chain(args.to_filters.iter().map(|s| {
                let (k, p) = parse_period_filter(s);
                (k, p, false)
            }))
            .collect();
        collected.retain(|c| {
            if c.is_dir {
                return true;
            }
            let meta = match std::fs::metadata(&c.path) {
                Ok(m) => m,
                Err(_) => return false,
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let after_ok = after.is_none_or(|a| mtime > u128::from(a) * 1_000_000_000);
            let before_ok = before.is_none_or(|b| mtime < u128::from(b) * 1_000_000_000);
            let period_ok = filters.iter().all(|&(kind, period, is_tn)| {
                let t = file_time(&meta, kind);
                let bound = now.saturating_sub(u128::from(period) * 1_000_000_000);
                if is_tn { t >= bound } else { t < bound }
            });
            after_ok && before_ok && period_ok
        });
    }
    // -ol / -oh: symbolic links and hard links are stored as redirect
    // records instead of their data. The data member of a hard-link group
    // (first occurrence) is archived normally; the rest reference it.
    // RAR4 has no redirect records, and WinRAR's `-ma4 -oh` likewise stores
    // the files in full, so `-oh` is a no-op for the legacy pipeline.
    let (mut collected, mut redirects) = crate::links::split_link_redirects(
        collected,
        args.store_links,
        args.store_hardlinks && !version.is_legacy(),
        misc.skip_links,
    );
    // -oi: identical-file references. The listing modes (-oi3/-oi4) print
    // the groups and create no archive at all; the dedup modes store the
    // first file and redirect the rest (RAR4 has no redirect records, and
    // WinRAR's `-ma4 -oi` likewise stores the files in full).
    let identical = crate::links::parse_identical(misc.identical.as_deref())?;
    if let Some(spec) = identical.as_ref() {
        if matches!(
            spec.mode,
            crate::links::IdenticalMode::List | crate::links::IdenticalMode::ListBare
        ) {
            crate::links::print_identical_groups(&collected, spec);
            return Ok(());
        }
        if !version.is_legacy() {
            let (kept, mut copies) = crate::links::apply_identical_redirects(collected, spec);
            collected = kept;
            redirects.append(&mut copies);
        }
    }
    // WinRAR aborts with "WARNING: No files" (exit code 10) and leaves the
    // archive untouched when every candidate was filtered out; since the
    // writer opens lazily, nothing has been created at this point.
    if collected.is_empty() && args.stdin_name.is_none() && redirects.is_empty() {
        info!("WARNING: No files");
        process::exit(10);
    }
    // -log: capture the member names before the candidate list is reordered
    // for writing.
    let logs = crate::log::specs_from(misc)?;
    let log_files: Vec<String> = if logs.is_empty() {
        Vec::new()
    } else {
        collected
            .iter()
            .map(|c| c.name.clone())
            .chain(redirects.iter().map(|(name, _, _)| name.clone()))
            .chain(args.stdin_name.iter().cloned())
            .collect()
    };
    let created: Option<rar_rs::ArchiveWriter> = if existing {
        None
    } else {
        Some(
            rar_rs::ArchiveWriter::create_with(archive_path, opts.clone())
                .map_err(|e| format!("create: {e}"))?,
        )
    };
    // Directory entries always come after the files, like WinRAR.
    let (file_entries, dir_entries): (Vec<_>, Vec<_>) =
        collected.into_iter().partition(|c| !c.is_dir);
    let mut collected: Vec<_> = file_entries;
    // `-se` (reset the solid chain on a file-extension change) is handled
    // per-member inside the writer via `maybe_reset_solid_for_extension`, so
    // WinRAR's input order is preserved (we do NOT reorder by extension here
    // — WinRAR keeps order and resets only when the extension changes).
    // rarfiles.lst: user-defined add order for solid archives (mask list
    // with optional `$default`); matched files are grouped by the
    // highest-priority mask, where a mask whose matches are a subset of
    // another mask's wins regardless of position (WinRAR semantics).
    // `-ds` disables the sorting (like WinRAR).
    if (args.solid || args.solid_params.is_some()) && !args.no_sort {
        let masks = input::read_rarfiles_lst();
        if !masks.is_empty() {
            apply_rarfiles_order(&mut collected, &masks);
        }
    }
    collected.extend(dir_entries);
    // -tsp: snapshot source access times before reading the files.
    #[cfg(unix)]
    let ts_preserve_atimes: Vec<(std::path::PathBuf, std::time::SystemTime)> = if misc.ts_preserve {
        use std::os::unix::fs::MetadataExt;
        collected
            .iter()
            .filter(|c| !c.is_dir)
            .filter_map(|c| {
                let m = std::fs::metadata(&c.path).ok()?;
                Some((
                    c.path.clone(),
                    std::time::UNIX_EPOCH
                        + std::time::Duration::from_secs(m.atime() as u64)
                        + std::time::Duration::from_nanos(m.atime_nsec() as u64),
                ))
            })
            .collect()
    } else {
        Vec::new()
    };
    #[cfg(not(unix))]
    let ts_preserve_atimes: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    let mut write_entries: Vec<rar_rs::WriteEntry<'_>> = Vec::with_capacity(collected.len());
    for c in &collected {
        let options = rar_rs::EntryWriteOptions::new().compression_level(
            rar_rs::CompressionLevel::try_from(c.level).map_err(|e| format!("level: {e}"))?,
        );
        if c.is_dir {
            write_entries.push(rar_rs::WriteEntry::Directory {
                path: &c.path,
                name: Some(&c.name),
            });
        } else {
            write_entries.push(rar_rs::WriteEntry::File {
                path: &c.path,
                name: Some(&c.name),
                options,
            });
        }
    }
    // WinRAR `rar a` semantics on an existing archive: members with the
    // same name as an incoming file are replaced — deleted first through
    // the editor role, then re-added through the typed append below; every
    // other member is preserved verbatim. The append handle is opened only
    // after the rewrite.
    let mut writer = if existing {
        use std::collections::HashSet;
        if args.volume_size.is_some() {
            return Err("appending to multi-volume archives is not supported".into());
        }
        let incoming: HashSet<String> = collected
            .iter()
            .map(|c| c.name.clone())
            .chain(args.stdin_name.iter().cloned())
            .collect();
        let mut editor = open_editor(archive_path, password.as_deref())?;
        let to_drop: Vec<String> = editor
            .entries()
            .map(|entry| entry.name().to_string())
            .filter(|n| incoming.contains(n))
            .collect();
        if !to_drop.is_empty() {
            let refs: Vec<&str> = to_drop.iter().map(|s| s.as_str()).collect();
            let plan = editor_delete_plan(&editor, &refs).map_err(|e| format!("replace: {e}"))?;
            editor.apply(plan).map_err(|e| format!("replace: {e}"))?;
        }
        // Deleting every member erases the archive file (like `rar d`);
        // when the replacement removed the only members, recreate it
        // instead of appending to a file that no longer exists.
        if std::path::Path::new(archive_path).exists() {
            let mut append_opts = rar_rs::AppendOptions::new();
            if let Some(pw) = &password {
                append_opts = append_opts.password(pw.clone());
            }
            if let Some(size) = dictionary {
                append_opts = append_opts.dictionary_size(size);
            }
            rar_rs::ArchiveWriter::append_with(archive_path, append_opts)
                .map_err(|e| format!("open: {e}"))?
        } else {
            rar_rs::ArchiveWriter::create_with(archive_path, opts.clone())
                .map_err(|e| format!("create: {e}"))?
        }
    } else {
        created.expect("new archive opened above")
    };
    // RAR 1.3/1.4 have no editor path: the DOS main header must precede every
    // member, so `-z` queues the comment before the first member; RAR5/RAR4
    // attach it after creation through the editor below.
    if version.is_rar13()
        && let Some(comment_file) = &misc.comment_file
    {
        let data = std::fs::read(comment_file).map_err(|e| format!("comment: {e}"))?;
        writer
            .set_archive_comment(Some(data))
            .map_err(|e| format!("comment: {e}"))?;
    }
    writer
        .add_batch(&write_entries)
        .map_err(|e| format!("add: {e}"))?;
    // Link redirects are recorded after their data members (the reference
    // target name is what matters, not the order).
    for (name, redir_type, target) in &redirects {
        writer
            .add_redirect(name, *redir_type, target)
            .map_err(|e| format!("link {name}: {e}"))?;
    }

    // -si<name>: one member read from stdin.
    if let Some(name) = &args.stdin_name {
        use std::io::Read;
        let mut data = Vec::new();
        std::io::stdin()
            .read_to_end(&mut data)
            .map_err(|e| format!("stdin: {e}"))?;
        let name = name.replace('\\', "/");
        let stdin_options = rar_rs::EntryWriteOptions::new().compression_level(
            rar_rs::CompressionLevel::try_from(args.level).map_err(|e| format!("level: {e}"))?,
        );
        writer
            .add_bytes(&name, &data, stdin_options)
            .map_err(|e| format!("add stdin: {e}"))?;
    }

    let was_existing = existing;
    let write_report = writer.finish().map_err(|e| format!("close: {e}"))?;
    // -sfx[name]: prepend the SFX module to the (first) archive volume.
    if let Some(module) = &args.sfx_module {
        crate::sfx::prepend_module_in_place(write_report.primary_path(), Some(module.as_str()))?;
    }
    crate::log::write_logs(&logs, write_report.volume_paths(), &log_files)?;
    // -tsp: restore the source files' access times that were recorded
    // before archiving (reading the files may have refreshed them).
    if misc.ts_preserve {
        #[cfg(unix)]
        for (path, atime) in &ts_preserve_atimes {
            let _ = std::fs::File::options()
                .write(true)
                .open(path)
                .and_then(|f| f.set_times(std::fs::FileTimes::new().set_accessed(*atime)));
        }
        #[cfg(not(unix))]
        {
            let _ = misc;
            let _ = &ts_preserve_atimes;
        }
    }
    // -tk: restore the archive's original modification time.
    if let Some(t) = orig_mtime {
        let _ = std::fs::File::options()
            .write(true)
            .open(archive_path)
            .and_then(|f| f.set_times(std::fs::FileTimes::new().set_modified(t)));
    }
    // -tl: set the archive's modification time to the newest member.
    if args.latest_time && !collected.is_empty() {
        let latest = collected
            .iter()
            .filter(|c| !c.is_dir)
            .filter_map(|c| std::fs::metadata(&c.path).ok()?.modified().ok())
            .max();
        if let Some(t) = latest {
            let _ = std::fs::File::options()
                .write(true)
                .open(archive_path)
                .and_then(|f| f.set_times(std::fs::FileTimes::new().set_modified(t)));
        }
    }
    // -tk<date>: assign the requested archive modification time.
    if let Some(t) = tk_date {
        let _ = time::set_file_mtime(std::path::Path::new(archive_path), t);
    }
    // -df: delete the source files after archiving (the archive keeps
    // them; directories are left in place, like WinRAR).
    if args.delete_after {
        for c in &collected {
            if !c.is_dir {
                let _ = std::fs::remove_file(&c.path);
            }
        }
    }
    // -t: test the archive right after creating it without materializing
    // member contents or extracting to a temporary directory.
    if args.test_after {
        let mut ar =
            ops::open_reader(archive_path, password.as_deref()).map_err(|e| e.context("open"))?;
        let report: rar_rs::VerificationReport = ar
            .verify()
            .map_err(|e| error::CliError::from(e).context("test failed"))?;
        if report.failed() != 0 {
            return Err(format!("test failed: {} member(s) failed", report.failed()).into());
        }
    }
    // -as: synchronize the archive contents — drop members that are not
    // part of the file list (only meaningful when appending to an
    // existing archive; a freshly created archive only holds the list).
    if args.sync_archive && was_existing {
        use std::collections::HashSet;
        let keep: HashSet<String> = collected
            .iter()
            .map(|c| c.name.clone())
            .chain(args.stdin_name.iter().cloned())
            .collect();
        let mut editor = open_editor(archive_path, password.as_deref())?;
        let stale: Vec<String> = editor
            .entries()
            .map(|entry| entry.name().to_string())
            .filter(|n| !keep.contains(n))
            .collect();
        if !stale.is_empty() {
            let refs: Vec<&str> = stale.iter().map(|s| s.as_str()).collect();
            let plan = editor_delete_plan(&editor, &refs).map_err(|e| format!("sync: {e}"))?;
            editor.apply(plan).map_err(|e| format!("sync: {e}"))?;
        }
    }
    if args.volume_size.is_some() {
        info!(
            "Created {} volume(s) ({} file(s), level {})",
            write_report.volume_paths().len(),
            files.len(),
            args.level
        );
    } else if was_existing {
        info!(
            "Updated {archive_path} ({} file(s), level {})",
            files.len(),
            args.level
        );
    } else {
        info!(
            "Created {archive_path} ({} file(s), level {})",
            files.len(),
            args.level
        );
    }
    // -z<file>: attach an archive comment through the editor role (the RAR
    // 1.3/1.4 comment was already queued before the first member above).
    // The resolved password includes a `-hp<password>` value, unlike the raw
    // `args.password` (an `-hp`-encrypted archive needs it here).
    if !version.is_rar13() && misc.comment_file.is_some() {
        crate::comment::cmd_comment_set(
            &crate::args::CommentArgs {
                password: crate::password::PasswordArgs {
                    password: password.clone(),
                },
                archive: archive_path.clone(),
            },
            misc,
        )?;
    }
    // -k: lock the archive after a successful create.
    if misc.lock {
        crate::recovery::cmd_lock(&crate::args::ArchiveArgs {
            password: crate::password::PasswordArgs {
                password: password.clone(),
            },
            archive: archive_path.clone(),
        })?;
    }
    Ok(())
}
