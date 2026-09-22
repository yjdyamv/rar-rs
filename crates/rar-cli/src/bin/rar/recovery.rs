//! Lock, recovery records, recovery volumes, repair and rebuild.

use crate::args::{ArchiveArgs, RecoveryArgs, RecoveryVolumesArgs};
use crate::edit::open_editor;
use crate::error;
use crate::error::CliResult;
use crate::info;
use crate::list::is_rar4_file;
/// Lock the archive (like `rar k`).
pub(crate) fn cmd_lock(args: &ArchiveArgs) -> CliResult<()> {
    let mut editor = open_editor(&args.archive, args.password.password.as_deref())
        .map_err(|e| format!("open: {e}"))?;
    editor.lock().map_err(|e| format!("lock: {e}"))?;
    info!("Locked {archive}", archive = args.archive);
    Ok(())
}

/// Add an inline recovery record (like `rar rr`).
pub(crate) fn cmd_rr(args: &RecoveryArgs) -> CliResult<()> {
    let mut editor = open_editor(&args.archive, args.password.password.as_deref())?;
    editor
        .apply(rar_rs::EditPlan::new().set_recovery(args.percent))
        .map_err(|e| format!("rr: {e}"))?;
    info!(
        "Recovery record {}% added to {archive}",
        args.percent,
        archive = args.archive
    );
    Ok(())
}

/// Create recovery volumes for an existing multi-volume set (like
/// `rar rv[N]`): `N` is the number of `.rev` files, or `N%` the percent
/// of data volumes (default 10%). The count is capped at 10x the data
/// volume count and the `.rev` files are named with the set's padding,
/// matching WinRAR. Only the raw volume bytes are read, so encrypted
/// sets need no password.
pub(crate) fn cmd_recovery_volumes(args: &RecoveryVolumesArgs) -> CliResult<()> {
    let first = std::path::Path::new(&args.archive);
    let volumes = rar_rs::discover_volumes(first);
    let nd = volumes.len();
    if nd <= 1 {
        return Err(format!("rv: {} is not part of a multi-volume set", first.display()).into());
    }
    let spec = args.count_spec.trim();
    let rec_count = if let Some(pct) = spec.strip_suffix('%') {
        let pct: u64 = pct
            .parse()
            .map_err(|_| error::CliError::from(format!("invalid recovery percent: {spec}")))?;
        if pct > 1000 {
            return Err(format!("invalid recovery percent: {spec}").into());
        }
        rar_rs::plan_recovery_volume_count(nd, pct)
            .map_err(|e| error::CliError::from(e).context("rv"))?
    } else {
        spec.parse::<usize>()
            .map_err(|_| error::CliError::from(format!("invalid recovery volume count: {spec}")))?
    };

    let written = rar_rs::build_recovery_volumes_for_set(&volumes, rec_count)
        .map_err(|e| error::CliError::from(e).context("rv"))?;
    for path in &written {
        info!("Creating {}", path.display());
    }
    info!("{} recovery volume(s) created", written.len());
    Ok(())
}

/// Render a repair failure.
///
/// `Unsupported` from the repair entry points means "this archive has no
/// recovery record" (the RAR5 and legacy scanners both report it that way),
/// so it is printed as `repair: <message>` instead of `unsupported: …`;
/// everything else gets the operation as context. Neither branch repeats the
/// `repair: ` prefix the library used to add as well, which produced
/// `repair: RAR format error: repair: …`.
fn repair_failure(error: rar_rs::RarError) -> error::CliError {
    match error {
        rar_rs::RarError::Unsupported(message) => format!("repair: {message}").into(),
        other => error::CliError::from(other).context("repair"),
    }
}

/// Repair an archive with its inline recovery record (like `rar r`); when the
/// archive carries no record, reconstruct a fresh archive from the members
/// that still decode, like WinRAR. Writes `fixed.<name>` when damage was
/// repaired and `rebuilt.<name>` when there was no record to repair with.
pub(crate) fn cmd_repair(args: &ArchiveArgs) -> CliResult<()> {
    let archive_path = &args.archive;
    // RAR 1.3/1.4 has no recovery records and its fixed-width headers are
    // not RAR5 blocks; refuse before the RAR5 repair path misparses them.
    if crate::list::is_rar13_file(std::path::Path::new(archive_path)) {
        return Err("repair is not supported for RAR 1.3/1.4 archives".into());
    }
    let name = std::path::Path::new(archive_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive.rar".to_string());
    // A `-hp` archive needs the password to read its headers; `-p-` (empty)
    // means "no password".
    let password = args
        .password
        .password
        .as_deref()
        .filter(|pw| !pw.is_empty());
    let fixed_path = format!("fixed.{name}");
    // Streaming repair: bounded memory regardless of archive size; the
    // repaired archive is staged and renamed atomically by the library.
    let repaired = if is_rar4_file(std::path::Path::new(archive_path)) {
        // `-hp`: the recovery record's header is encrypted, so locating it
        // needs the archive password (the protected bytes are not).
        rar_rs::repair_legacy_archive_path_with_password(
            std::path::Path::new(archive_path),
            std::path::Path::new(&fixed_path),
            password,
        )
    } else {
        rar_rs::repair_archive_path(
            std::path::Path::new(archive_path),
            std::path::Path::new(&fixed_path),
        )
    };
    match repaired {
        Ok(true) => {
            // The official tool refuses an obviously truncated archive with a
            // clear error; validate the repaired bytes with our own reader. A
            // `-hp` archive must be opened with its password, or the open
            // fails before we even reach the members.
            let mut options = rar_rs::OpenOptions::new();
            if let Some(pw) = password {
                options = options.password(pw);
            }
            if let Err(e) = rar_rs::ArchiveReader::open_with(&fixed_path, options) {
                let _ = std::fs::remove_file(&fixed_path);
                return Err(
                    error::CliError::from(e).context("repair produced an unreadable archive")
                );
            }
            info!("Repaired {archive_path} -> {fixed_path}");
            Ok(())
        }
        Ok(false) => {
            info!("All OK");
            Ok(())
        }
        // Nothing to repair *with*: rebuild from the members that still
        // decode, the way WinRAR does.
        Err(rar_rs::RarError::Unsupported(_)) => {
            reconstruct(std::path::Path::new(archive_path), &name, password)
        }
        Err(other) => Err(repair_failure(other)),
    }
}

/// `rar r`'s fallback when no recovery record is present: decode every member,
/// keep only the ones that verify, and write them into `rebuilt.<name>`, like
/// WinRAR.
fn reconstruct(archive: &std::path::Path, name: &str, password: Option<&str>) -> CliResult<()> {
    let rebuilt = format!("rebuilt.{name}");
    info!("Data recovery record not found");
    info!("Reconstructing {}", archive.display());
    info!("Building {rebuilt}");
    let report =
        rar_rs::reconstruct_archive_path(archive, std::path::Path::new(&rebuilt), password)
            .map_err(|e| error::CliError::from(e).context("reconstruct"))?;
    for member in report.recovered() {
        info!("Found  {member}");
    }
    info!("Done");
    Ok(())
}

/// Rebuild missing volumes from the `.rev` recovery volumes (like `rar rc`).
pub(crate) fn cmd_rebuild_volumes(args: &ArchiveArgs) -> CliResult<()> {
    let first = &args.archive;
    let rebuilt = rar_rs::rebuild_missing_volumes(std::path::Path::new(first))
        .map_err(|e| format!("rc: {e}"))?;
    if rebuilt.is_empty() {
        info!("All volumes present");
    } else {
        for path in &rebuilt {
            info!("Rebuilt {}", path.display());
        }
    }
    Ok(())
}
