//! Format-neutral write operations shared by the RAR13, RAR4 and RAR5
//! pipelines: the member-addition dispatchers, batch progress/sequential
//! fallback and solid-chain state resets.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::engine::{BatchEntry, Engine};
use crate::error::{RarError, RarResult};

/// Whether a *whole-member* legacy RAR 1.5–4.x payload should be stored
/// instead of encoded.
///
/// Those encoders build an `O(input)` token vector before emitting anything, so
/// a random-data member (media, archives, encrypted files) would allocate
/// hundreds of MiB for a guaranteed-losing encode. The shared stride probe —
/// the same gate the RAR5 path uses — routes such members straight to STORE.
/// Level 0 already stores, and a member with distant byte-identical copies is
/// still compressible (the probe's repeat escape hatch keeps it).
pub(crate) fn whole_member_is_incompressible(data: &[u8], level: u8) -> bool {
    level != 0 && crate::codec::common::incompressible::sample_is_incompressible(data, level)
}

/// Derive an archive member name from a filesystem path when the caller did
/// not supply one. Root paths (`/`, `C:\`) have no final component; that is
/// a caller error rather than an internal invariant, so it maps to
/// `InvalidOption` instead of panicking on `file_name().unwrap()`.
pub(crate) fn archive_name_from_path(path: &Path) -> RarResult<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| {
            RarError::InvalidOption(format!(
                "cannot derive an archive name from {}; pass an explicit name",
                path.display()
            ))
        })
}

/// Add a file from the filesystem to the archive.
pub(crate) fn add(
    cx: &mut dyn Engine,
    path: impl AsRef<Path>,
    compression_level: u8,
) -> RarResult<()> {
    cx.check_cancel()?;
    let path = path.as_ref();
    if !path.exists() {
        return Err(RarError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("path not found: {}", path.display()),
        )));
    }

    if path.is_dir() {
        add_directory(cx, path, None, true, compression_level)
    } else {
        add_file(cx, path, None, compression_level)
    }
}

/// Add a file or directory to the archive under a custom archive name.
///
/// `arcname` overrides the entry name in the archive. For directories the
/// children keep the same relative layout beneath `arcname`.
pub(crate) fn add_as(
    cx: &mut dyn Engine,
    path: impl AsRef<Path>,
    arcname: &str,
    compression_level: u8,
) -> RarResult<()> {
    cx.check_cancel()?;
    let path = path.as_ref();
    if !path.exists() {
        return Err(RarError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("path not found: {}", path.display()),
        )));
    }

    let arcname = arcname.replace('\\', "/");
    let arcname = arcname.trim_start_matches('/').to_string();

    if path.is_dir() {
        add_directory(cx, path, Some(&arcname), true, compression_level)
    } else {
        add_file(cx, path, Some(&arcname), compression_level)
    }
}

/// The container-neutral file dispatcher: RAR 1.3/1.4 uses its own
/// DOS-era pipeline, RAR4 the legacy one, RAR5 the modern one.
pub(crate) fn add_file(
    cx: &mut dyn Engine,
    path: &Path,
    arcname: Option<&str>,
    level: u8,
) -> RarResult<()> {
    if cx.is_rar13() {
        crate::format::rar13::write::add_file_rar13(cx, path, arcname, level)
    } else if cx.is_rar4() {
        crate::format::rar4::write::member::add_file_rar4(cx, path, arcname, level)
    } else {
        crate::format::rar5::write::add::add_file_rar5(cx, path, arcname, level)
    }
}

/// Add raw bytes as a named file in the archive.
pub(crate) fn add_bytes(
    cx: &mut dyn Engine,
    arcname: &str,
    data: &[u8],
    compression_level: u8,
) -> RarResult<()> {
    cx.check_cancel()?;
    if cx.is_rar13() {
        let name = arcname.replace('\\', "/");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        return crate::format::rar13::write::add_rar13_data(
            cx,
            name,
            data.to_vec(),
            compression_level,
            now.as_secs() as u32,
            now.subsec_nanos(),
            None,
        );
    }
    if cx.is_rar4() {
        // RAR4 members are encoded through the same pipeline as
        // `add_file_rar4` (CRC, LZ/PPMd/filter/STORE candidates,
        // per-member encryption, volume splitting) with the current
        // time as the timestamp.
        let name = arcname.replace('\\', "/");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        return crate::format::rar4::write::member::add_rar4_data(
            cx,
            name,
            data.to_vec(),
            compression_level,
            now.as_secs() as u32,
            now.subsec_nanos(),
            None,
            None,
        );
    }
    crate::format::rar5::write::add::add_bytes_rar5(cx, arcname, data, compression_level)
}

/// Add a directory entry only (no recursion).
///
/// Writes the directory header without traversing children. Callers that
/// enumerate files themselves (e.g. with exclusion filtering) use this to
/// keep empty directories and the directory structure in the archive.
pub(crate) fn add_directory_only(
    cx: &mut dyn Engine,
    path: impl AsRef<Path>,
    arcname: &str,
) -> RarResult<()> {
    cx.check_cancel()?;
    let path = path.as_ref();
    if !solid_chain_is_position_derived(cx) {
        reset_solid_chain(cx);
    }
    let name = arcname.replace('\\', "/").trim_end_matches('/').to_string();

    let meta = fs::metadata(path)?;
    let mtime = meta
        .modified()
        .unwrap_or(SystemTime::now())
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as u32;
    let mtime_ns = meta
        .modified()
        .unwrap_or(SystemTime::now())
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();

    if cx.is_rar13() {
        return crate::format::rar13::write::write_rar13_dir_entry(cx, &name, mtime, mtime_ns);
    }
    if cx.is_rar4() {
        return crate::format::rar4::write::member::write_rar4_dir_entry(
            cx, &name, &meta, mtime, mtime_ns,
        );
    }
    crate::format::rar5::write::add::write_rar5_dir_entry(cx, &name, &meta, mtime)
}

/// Add a directory, optionally recursing into its children.
fn add_directory(
    cx: &mut dyn Engine,
    path: &Path,
    arcname: Option<&str>,
    recursive: bool,
    level: u8,
) -> RarResult<()> {
    let mut ancestors = HashSet::new();
    add_directory_inner(cx, path, arcname, recursive, level, &mut ancestors)
}

/// [`add_directory`] carrying the canonical identities of the
/// directory chain currently being walked. A symlink or junction loop
/// (`root/loop -> root`) resolved by `is_dir()` would otherwise recurse
/// until the path-length limit and fail mid-add; the loop edge is
/// skipped instead. Only the ancestor chain is recorded: `canonicalize`
/// maps a junction/symlink to its target, so an identity is on the set
/// exactly while one of its aliases is being walked and two sibling
/// links to the same directory are both traversed. Keeping every
/// identity in one whole-tree set instead dropped the second alias (and
/// its whole subtree) as if it were a cycle.
///
/// An identity is removed when its subtree finishes; an error aborts the
/// whole add (every caller propagates), so no stale entry can survive
/// into a sibling.
fn add_directory_inner(
    cx: &mut dyn Engine,
    path: &Path,
    arcname: Option<&str>,
    recursive: bool,
    level: u8,
    ancestors: &mut HashSet<PathBuf>,
) -> RarResult<()> {
    let identity = canonical_directory_id(path);
    if ancestors.contains(&identity) {
        return Ok(());
    }
    ancestors.insert(identity.clone());
    if !solid_chain_is_position_derived(cx) {
        reset_solid_chain(cx);
    }
    let name = match arcname {
        Some(s) => s.to_string(),
        None => archive_name_from_path(path)?,
    };
    let name = name.replace('\\', "/").trim_end_matches('/').to_string();

    let meta = fs::metadata(path)?;
    let mtime = meta
        .modified()
        .unwrap_or(SystemTime::now())
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as u32;

    if cx.is_rar13() {
        let mtime_ns = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        crate::format::rar13::write::write_rar13_dir_entry(cx, &name, mtime, mtime_ns)?;
    } else if cx.is_rar4() {
        let mtime_ns = meta
            .modified()
            .unwrap_or(SystemTime::now())
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        crate::format::rar4::write::member::write_rar4_dir_entry(
            cx, &name, &meta, mtime, mtime_ns,
        )?;
    } else {
        crate::format::rar5::write::add::write_rar5_dir_entry(cx, &name, &meta, mtime)?;
    }

    if recursive {
        let mut children: Vec<_> = fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
        children.sort_by_key(|e| e.file_name());

        for child in children {
            cx.check_cancel()?;
            let child_path = child.path();
            let child_name = if name.is_empty() {
                child.file_name().to_string_lossy().into_owned()
            } else {
                format!("{name}/{}", child.file_name().to_string_lossy())
            };
            if child_path.is_dir() {
                add_directory_inner(cx, &child_path, Some(&child_name), true, level, ancestors)?;
            } else {
                add_file(cx, &child_path, Some(&child_name), level)?;
            }
        }
    }

    ancestors.remove(&identity);
    Ok(())
}

/// One batch through the container's parallel path when eligible,
/// otherwise the sequential fallback; archive order is always preserved.
pub(crate) fn add_batch(cx: &mut dyn Engine, entries: &[BatchEntry<'_>]) -> RarResult<()> {
    cx.check_cancel()?;
    set_rar4_dict_bits(cx, entries)?;
    #[cfg(feature = "parallel")]
    {
        if !cx.is_legacy()
            && !cx.write_ctx().solid.mode
            && !cx.write_ctx().meta.streams
            && !entries.is_empty()
        {
            return crate::format::rar5::write::batch::add_batch_parallel(cx, entries);
        }
        // RAR4: independent non-solid file members compress in parallel
        // waves too (solid runs stay sequential - shared window; a
        // deferred solid append buffers its additions for the close-time
        // repack and must never stream-write).
        if cx.is_rar4()
            && !cx.write_ctx().solid.mode
            && !cx.write_ctx().rar4.solid_append
            && !entries.is_empty()
        {
            return crate::format::rar4::write::batch::add_batch_parallel_rar4(cx, entries);
        }
    }
    progress_set_batch_total(cx, entries)?;
    for (i, entry) in entries.iter().enumerate() {
        cx.set_progress_member(i);
        add_batch_entry_sequential(cx, entry)?;
    }
    Ok(())
}

/// Compute the archive-wide RAR4 dictionary/window bits WinRAR declares (the
/// largest member, or the whole run when solid) from the batch, so every
/// member header carries the same value. Single-add streaming has no known
/// member set; emission then falls back to a per-member safe value.
fn set_rar4_dict_bits(cx: &mut dyn Engine, entries: &[BatchEntry<'_>]) -> RarResult<()> {
    if !cx.is_rar4() {
        return Ok(());
    }
    let solid = cx.write_ctx().solid.mode;
    let mut max = 0u64;
    let mut sum = 0u64;
    for e in entries {
        let size = match e {
            BatchEntry::Bytes { data, .. } => data.len() as u64,
            BatchEntry::File { path, .. } => fs::metadata(path)?.len(),
            BatchEntry::Directory { .. } => 0,
        };
        max = max.max(size);
        sum = sum.saturating_add(size);
    }
    let size = if solid { sum } else { max };
    let bits = crate::format::rar4::write::archive_dict_bits(
        cx.write_ctx().solid.rar4_unp_ver,
        size,
        solid,
    );
    cx.write_ctx_mut().solid.rar4_dict_bits = Some(bits);
    Ok(())
}

/// Sum every member's input size so the progress denominator covers the
/// whole batch (parallel waves and sequential members alike).
pub(crate) fn progress_set_batch_total(
    cx: &dyn Engine,
    entries: &[BatchEntry<'_>],
) -> RarResult<()> {
    let mut total = 0u64;
    for e in entries {
        let size = match e {
            BatchEntry::Bytes { data, .. } => data.len() as u64,
            BatchEntry::File { path, .. } => fs::metadata(path)?.len(),
            BatchEntry::Directory { .. } => 0,
        };
        total = total.saturating_add(size);
    }
    if let Some((progress, _)) = cx.progress_slot() {
        progress.lock().expect("progress lock").set_total(total);
    }
    Ok(())
}

/// Route one batch entry through the format-neutral add* entry points.
pub(crate) fn add_batch_entry_sequential(
    cx: &mut dyn Engine,
    entry: &BatchEntry<'_>,
) -> RarResult<()> {
    cx.check_cancel()?;
    match *entry {
        BatchEntry::Bytes { name, data, level } => add_bytes(cx, name, data, level),
        BatchEntry::File { path, name, level } => match name {
            Some(name) => add_as(cx, path, name, level),
            None => add(cx, path, level),
        },
        BatchEntry::Directory { path, name } => {
            let name = match name {
                Some(name) => name.to_string(),
                None => path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            };
            add_directory_only(cx, path, &name)
        }
    }
}

/// Pre-RAR3 (and RAR 1.3/1.4) solid chains are derived from the
/// archive-level `MHD_SOLID` flag and member position: the reader keeps
/// one window across STORE members and directories, so the writer must
/// not drop the carried encoder for them (doing so desynchronises every
/// later member of the run).
pub(crate) fn solid_chain_is_position_derived(cx: &dyn Engine) -> bool {
    cx.is_rar13()
        || (cx.is_rar4()
            && crate::version::LegacyCodec::from_unp_ver(cx.write_ctx().solid.rar4_unp_ver)
                != Some(crate::version::LegacyCodec::Rar29))
}

/// Drop the solid-chain encoder state (call after any member that does
/// not participate in the LZ window: directories, STORE files, empty
/// files, or when compression fell back to STORE). Position-derived
/// chains call this only when the container can flag the break
/// (`v29` FHD_SOLID, RAR5): see [`solid_chain_is_position_derived`].
pub(crate) fn reset_solid_chain(cx: &mut dyn Engine) {
    cx.write_ctx_mut().solid.encoder_state = None;
    cx.write_ctx_mut().solid.chain_dict = None;
    cx.write_ctx_mut().solid.rar4_encoder = None;
    cx.write_ctx_mut().solid.legacy_encoder = None;
    cx.write_ctx_mut().solid.rar4_run_has_member = false;
    cx.write_ctx_mut().solid.last_ext = None;
}

/// Reset the solid chain when the next member's file extension differs
/// from the previous one (WinRAR `-se`). No-op unless solid mode is on
/// and `solid_reset` is `PerExtension`. Directories and STORE members
/// break the chain through `reset_solid_chain`, which also clears
/// `last_solid_ext`, so this only needs to run for compressed members.
pub(crate) fn maybe_reset_solid_for_extension(cx: &mut dyn Engine, name: &str) {
    if !cx.write_ctx().solid.mode
        || cx.write_ctx().solid.reset != crate::options::SolidReset::PerExtension
    {
        return;
    }
    let base = name.trim_end_matches('/');
    let ext = base.rsplit('.').next().unwrap_or("");
    match &cx.write_ctx().solid.last_ext {
        Some(prev) if prev == ext => {}
        _ => {
            cx.write_ctx_mut().solid.encoder_state = None;
            cx.write_ctx_mut().solid.chain_dict = None;
            cx.write_ctx_mut().solid.rar4_encoder = None;
            cx.write_ctx_mut().solid.legacy_encoder = None;
            cx.write_ctx_mut().solid.rar4_run_has_member = false;
            cx.write_ctx_mut().solid.last_ext = Some(ext.to_string());
        }
    }
}

/// Canonical filesystem identity of a directory for recursive-add cycle
/// detection. `canonicalize` resolves symlinks and junctions; when it fails
/// (e.g. overlong paths) the path as given is used as a best effort, so
/// traversal still terminates on identical lexical paths.
fn canonical_directory_id(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::archive_name_from_path;
    use std::path::Path;

    #[test]
    fn paths_without_a_file_name_are_rejected() {
        for path in [Path::new(".."), Path::new("/")] {
            let err = archive_name_from_path(path).unwrap_err();
            assert!(
                matches!(err, crate::error::RarError::InvalidOption(_)),
                "{path:?}: expected InvalidOption, got {err}"
            );
        }
        assert_eq!(
            archive_name_from_path(Path::new("dir/file.txt")).unwrap(),
            "file.txt"
        );
    }
}
