//! Whole-archive and single-member extraction.
//!
//! [`extract_all_with_options`] picks the parallel path for large archives
//! when the memory budget allows, otherwise streams member by member;
//! [`extract_entry`] is the shared per-member body.

#[cfg(feature = "parallel")]
use crate::format::rar5::extract::{capped_dict_bytes, verify_integrity_for};

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(feature = "parallel")]
use crate::engine::DecryptedPayload;
use crate::engine::Engine;
use crate::engine::{ArchiveEntry, MAX_DICT_SIZE_LOG};
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::parse_redirect_record;
use crate::fs::atomic::{read_write_create, replace_file, temp_sibling_path};
use crate::fs::safe_path::sanitize_archive_path;
#[cfg(feature = "parallel")]
use crate::model::FileHeader;
#[cfg(feature = "parallel")]
use crate::parallel::extraction_pool;

/// Memory budget (packed + unpacked) for the optional parallel extraction
/// path; larger archives stream sequentially to stay bounded.
#[cfg(feature = "parallel")]
const PARALLEL_BUFFER_LIMIT: u64 = 256 * 1024 * 1024;
/// Parallel extraction only engages for at least this many members...
#[cfg(feature = "parallel")]
const PARALLEL_MIN_MEMBERS: usize = 4;
/// ...and at least this much total unpacked data (Rayon overhead amortized).
#[cfg(feature = "parallel")]
const PARALLEL_MIN_UNPACKED: u64 = 64 * 1024 * 1024;

/// Destination resolution outcome for one member; the serial and parallel
/// extraction paths share it so `-e`, `-o-` and `-or` behave identically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    /// Extract the member to this path.
    Extract(PathBuf),
    /// `-o-`: the destination already exists and must be left untouched.
    Skip(PathBuf),
}

/// Whether an outcome for `entry` belongs in an [`ExtractionReport`]:
/// directories (their creation is silent) and `-ol-`-skipped links do not.
fn reports_outcome(entry: &ArchiveEntry, options: &crate::options::ExtractOptions) -> bool {
    !entry.is_dir() && !(options.skip_links && entry.redirect().is_some())
}

/// The member's stored modification time as an instant, when the header
/// carries one (legacy local-civil converted, like `apply_member_times`).
fn member_mtime(hdr: &crate::model::FileHeader) -> Option<std::time::SystemTime> {
    use std::time::{Duration, UNIX_EPOCH};

    if !crate::format::shared::entry_ext::file_header_has_mtime(hdr) {
        return None;
    }
    let secs = if hdr.uses_local_civil_time() {
        crate::format::shared::legacy_time::local_civil_to_epoch(hdr.mtime)
    } else {
        hdr.mtime
    };
    Some(
        UNIX_EPOCH
            + Duration::from_secs(u64::from(secs))
            + Duration::from_nanos(u64::from(hdr.mtime_ns.unwrap_or(0))),
    )
}

/// Materialize one member file through a temp sibling of `dest_path`.
///
/// `produce` writes the member's bytes and reports the member's outcome:
/// `Ok(())` installs the temp over the destination; `Err(e)` means the
/// member failed (decode, integrity check, or the write itself) — with `-kb`
/// (`keep_broken`) the partial temp is installed anyway, otherwise it is
/// removed, and `e` is returned unchanged. Reporting a member failure
/// through this closure is what applies the `-kb` policy, so a caller holding
/// already-failed bytes must return `Err`, never `Ok`.
///
/// The streaming serial path and the buffered parallel path share this
/// policy so `-kb` cannot drift apart. It is deliberately not a
/// [`crate::fs::atomic::StagedFile`]: extraction never fsyncs a member, and
/// the failure path needs install-on-error rather than unconditional
/// cleanup.
fn materialize_member_file<F>(dest_path: &Path, keep_broken: bool, produce: F) -> RarResult<()>
where
    F: FnOnce(&mut File) -> RarResult<()>,
{
    let tmp_path = temp_sibling_path(dest_path);
    let result = (|| -> RarResult<()> {
        let mut file = read_write_create(&tmp_path)?;
        produce(&mut file)?;
        file.flush()?;
        Ok(())
    })();
    match result {
        Ok(()) => replace_file(&tmp_path, dest_path),
        Err(e) => {
            if keep_broken {
                // `-kb`: keep the partially extracted file.
                let _ = replace_file(&tmp_path, dest_path);
            } else {
                let _ = fs::remove_file(&tmp_path);
            }
            Err(e)
        }
    }
}

/// What one extraction operation wrote and what the skip-existing policy left
/// untouched, each in archive order.
///
/// Directory entries are neither: they carry no file data, so their creation
/// is silent. Members skipped by `-ol-` (`skip_links`) are not recorded
/// either. The report is the writer's own account — it cannot disagree with
/// what landed on disk, unlike a caller-side prediction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractionReport {
    written: Vec<PathBuf>,
    skipped: Vec<PathBuf>,
}

impl ExtractionReport {
    /// Destination paths of the members written by this run (files and
    /// created links), in archive order.
    pub fn written(&self) -> &[PathBuf] {
        &self.written
    }

    /// Number of members written.
    pub fn written_count(&self) -> usize {
        self.written.len()
    }

    /// Destination paths of the members the skip-existing policy left
    /// untouched (`-o-`), in archive order.
    pub fn skipped(&self) -> &[PathBuf] {
        &self.skipped
    }

    /// Number of members left untouched by the skip-existing policy.
    pub fn skipped_count(&self) -> usize {
        self.skipped.len()
    }

    pub(crate) fn record_written(&mut self, path: PathBuf) {
        self.written.push(path);
    }

    pub(crate) fn record_skipped(&mut self, path: PathBuf) {
        self.skipped.push(path);
    }
}

/// Validate per-entry header limits against the current extract options.
pub(crate) fn validate_entry_limits(cx: &dyn Engine, idx: usize) -> RarResult<()> {
    let hdr = &cx.entries()[idx].header;
    if hdr.comp_dict_size > MAX_DICT_SIZE_LOG {
        return Err(RarError::LimitExceeded {
            limit: MAX_DICT_SIZE_LOG as u64,
            context: format!(
                "{}: dictionary size log {} exceeds supported maximum {}",
                hdr.name, hdr.comp_dict_size, MAX_DICT_SIZE_LOG
            ),
        });
    }
    if let Some(limit) = cx.read_ctx().extract_options.max_unpacked_bytes
        && hdr.unpacked_size > limit
    {
        return Err(RarError::LimitExceeded {
            limit,
            context: format!(
                "{}: unpacked size {} exceeds limit",
                hdr.name, hdr.unpacked_size
            ),
        });
    }
    Ok(())
}

/// Extract all archive contents with explicit options, returning what was
/// written and what the skip-existing policy left untouched.
pub(crate) fn extract_all_with_options(
    cx: &mut dyn Engine,
    dest_dir: impl AsRef<Path>,
    opts: crate::options::ExtractOptions,
) -> RarResult<ExtractionReport> {
    let dest = dest_dir.as_ref();
    fs::create_dir_all(dest)?;
    cx.read_ctx_mut().extract_options = opts;
    // A quick-open catalog carries no "STM" service records: replace it
    // with the scanned catalog before extraction restores streams.
    crate::format::rar5::extract::open::ensure_full_catalog(cx)?;

    #[cfg(feature = "parallel")]
    {
        if let Some(report) = extract_all_parallel(cx, dest, opts)? {
            return Ok(report);
        }
    }

    let mut total_unpacked = 0u64;
    let mut report = ExtractionReport::default();
    let entries: Vec<_> = cx.entries().to_vec();
    for (index, entry) in entries.iter().enumerate() {
        cx.check_cancel()?;
        total_unpacked = total_unpacked
            .checked_add(entry.header.unpacked_size)
            .ok_or_else(|| RarError::LimitExceeded {
                limit: opts.max_total_unpacked_bytes.unwrap_or(u64::MAX),
                context: "total unpacked size overflow".into(),
            })?;
        if let Some(limit) = opts.max_total_unpacked_bytes
            && total_unpacked > limit
        {
            return Err(RarError::LimitExceeded {
                limit,
                context: format!(
                    "total unpacked size {total_unpacked} exceeds limit while extracting {}",
                    entry.name()
                ),
            });
        }
        extract_entry(cx, index, entry, dest, &mut report)?;
    }
    Ok(report)
}

/// Parallel extraction for eligible archives (optional `parallel`
/// feature).
///
/// Eligible: at least [`PARALLEL_MIN_MEMBERS`] members, no solid chains,
/// no split/multi-volume members, no progress callback, and total packed
/// + unpacked sizes within a bounded memory budget. Packed payloads are
///   read sequentially, then decoded and integrity-checked with Rayon
///   workers (archive order preserved by replaying writes sequentially
///   afterwards). Ineligible archives fall back to the sequential path
///   unchanged. The codec's decode is memory-bandwidth-bound, so the
///   parallel path mainly helps on machines where decompression is
///   CPU-bound; it engages only for member counts and sizes where Rayon
///   overhead is amortized.
#[cfg(feature = "parallel")]
fn extract_all_parallel(
    cx: &mut dyn Engine,
    dest: &Path,
    opts: crate::options::ExtractOptions,
) -> RarResult<Option<ExtractionReport>> {
    use rayon::prelude::*;

    if !crate::format::shared::extract::supports_parallel_extract(cx) {
        return Ok(None);
    }
    if cx.progress_slot().is_some() || cx.entries().len() < PARALLEL_MIN_MEMBERS {
        return Ok(None);
    }
    for (i, e) in cx.entries().iter().enumerate() {
        if crate::format::rar5::extract::solid::is_solid_chain_member(cx, i) || e.chunks.len() != 1
        {
            return Ok(None);
        }
    }
    let mut total_packed = 0u64;
    let mut total_unpacked = 0u64;
    for e in cx.entries() {
        total_packed = total_packed.saturating_add(e.header.packed_size);
        total_unpacked = total_unpacked.saturating_add(e.header.unpacked_size);
        if total_packed > PARALLEL_BUFFER_LIMIT || total_unpacked > PARALLEL_BUFFER_LIMIT {
            return Ok(None);
        }
    }
    if total_unpacked < PARALLEL_MIN_UNPACKED {
        return Ok(None);
    }
    if let Some(limit) = opts.max_total_unpacked_bytes
        && total_unpacked > limit
    {
        return Err(RarError::LimitExceeded {
            limit,
            context: "total unpacked size exceeds limit".into(),
        });
    }

    // Phase 1: read + decrypt all payloads sequentially.
    let mut payloads: Vec<(usize, DecryptedPayload)> = Vec::with_capacity(cx.entries().len());
    for i in 0..cx.entries().len() {
        payloads.push((
            i,
            crate::format::rar5::extract::decode::read_packed_data(cx, i)?,
        ));
    }
    let headers: Vec<FileHeader> = cx.entries().iter().map(|e| e.header.clone()).collect();

    /// One member decoded in phase 2: `error` carries the failure that
    /// interrupted the decode, with `data` holding the bytes produced so
    /// far so the replay can honor `-kb`.
    struct DecodedMember {
        idx: usize,
        data: Vec<u8>,
        error: Option<RarError>,
    }

    // Phase 2: decode + integrity-check in parallel. Validation failures
    // abort before that member's output is staged (like the serial path,
    // which validates before creating its temp file); decode and
    // integrity failures travel back with the partial bytes.
    let results: Vec<RarResult<DecodedMember>> = extraction_pool().install(|| {
        payloads
            .into_par_iter()
            .map(|(i, payload)| {
                let hdr = &headers[i];
                if hdr.comp_dict_size > MAX_DICT_SIZE_LOG {
                    return Err(RarError::LimitExceeded {
                        limit: MAX_DICT_SIZE_LOG as u64,
                        context: format!(
                            "{}: dictionary size log {} exceeds supported maximum {}",
                            hdr.name, hdr.comp_dict_size, MAX_DICT_SIZE_LOG
                        ),
                    });
                }
                // The RAR7 byte dictionary bypasses the 4-bit log: enforce
                // the extraction cap here too.
                let _ = capped_dict_bytes(hdr, opts.max_dict_size)?;
                if let Some(limit) = opts.max_unpacked_bytes
                    && hdr.unpacked_size > limit
                {
                    return Err(RarError::LimitExceeded {
                        limit,
                        context: format!(
                            "{}: unpacked size {} exceeds limit",
                            hdr.name, hdr.unpacked_size
                        ),
                    });
                }

                // The one member decoder (STORE bound, decode, size
                // check) the serial paths use; the Vec sink keeps the
                // decoded bytes for the sequential replay below. Empty
                // members have nothing to decode but still carry an
                // integrity value (a crafted zero-size header must not
                // bypass the check), so verification runs either way.
                let mut data = Vec::new();
                let mut error = None;
                let outcome = (|| -> RarResult<()> {
                    if hdr.packed_size != 0 || hdr.unpacked_size != 0 {
                        crate::format::rar5::payload::decode_member(
                            hdr, &payload, None, &mut data,
                        )?;
                    }
                    let crc = crc32fast::hash(&data);
                    let blake = hdr
                        .hash_value
                        .map(|_| crate::format::rar5::blake2sp::hash(&data));
                    verify_integrity_for(
                        hdr,
                        crc,
                        blake,
                        payload.params.as_ref(),
                        payload.keys.as_ref(),
                    )
                })();
                if let Err(e) = outcome {
                    error = Some(e);
                }
                Ok(DecodedMember {
                    idx: i,
                    data,
                    error,
                })
            })
            .collect()
    });

    // Phase 3: replay writes sequentially in archive order, through the
    // same per-member materialization and post-write steps as the serial
    // path. A member whose decode failed still stages its partial bytes
    // so `-kb` behaves identically, then aborts the run.
    let mut report = ExtractionReport::default();
    let keep_broken = cx.read_ctx().extract_options.keep_broken;
    for result in results {
        let member = result?;
        let idx = member.idx;
        // `resolve_dest_path` and `finish_member` both need the archive,
        // and the latter takes `&mut dyn Engine`, so the entry is cloned out
        // of the catalog first (the serial loop keeps a whole-catalog
        // snapshot for the same reason).
        let entry = cx.entries()[idx].clone();
        let dest_path = match resolve_dest_path(cx, &entry, dest)? {
            Destination::Extract(path) => path,
            Destination::Skip(path) => {
                if reports_outcome(&entry, &cx.read_ctx().extract_options) {
                    report.record_skipped(path);
                }
                continue;
            }
        };
        if entry.is_dir() {
            fs::create_dir_all(&dest_path)?;
            // Flat extraction resolves directories to the destination
            // root itself; the archived mode must not be applied to the
            // caller's directory.
            if dest_path.as_path() != dest {
                crate::format::shared::extract::dest::apply_member_attributes(
                    cx,
                    &entry.header,
                    &dest_path,
                );
            }
            continue;
        }
        if let Some(redir) = parse_redirect_record(&entry.header.extra_data) {
            if !cx.read_ctx().extract_options.skip_links {
                crate::format::shared::extract::dest::extract_redirection(
                    cx, dest, &dest_path, &redir,
                )?;
                report.record_written(dest_path);
            }
            continue;
        }
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(err) = member.error {
            // Report the failure through the closure: that is what makes
            // `materialize_member_file` apply the `-kb` policy to the
            // decoded bytes (returning `Ok` here would install them even
            // without `-kb`). A staging I/O error would win over `err`.
            let staged = materialize_member_file(&dest_path, keep_broken, |file| {
                file.write_all(&member.data)?;
                Err(err)
            });
            return match staged {
                Err(e) => Err(e),
                // Unreachable: the closure above never reports success.
                Ok(()) => Err(RarError::InvalidState(
                    "failed member staged as complete".into(),
                )),
            };
        }
        materialize_member_file(&dest_path, keep_broken, |file| {
            file.write_all(&member.data)?;
            Ok(())
        })?;
        finish_member(cx, idx, &entry, dest_path, &mut report)?;
    }
    Ok(Some(report))
}

/// Extract an entry selected by its archive-order catalog index.
pub(crate) fn extract_at_index_with_options(
    cx: &mut dyn Engine,
    idx: usize,
    dest_dir: impl AsRef<Path>,
    opts: crate::options::ExtractOptions,
) -> RarResult<PathBuf> {
    let mut report = ExtractionReport::default();
    extract_index_with_options(cx, idx, dest_dir, opts, &mut report)
}

/// [`extract_at_index_with_options`] with the outcome recorded in `report`:
/// batch callers build one report for the whole run.
pub(crate) fn extract_index_with_options(
    cx: &mut dyn Engine,
    idx: usize,
    dest_dir: impl AsRef<Path>,
    opts: crate::options::ExtractOptions,
    report: &mut ExtractionReport,
) -> RarResult<PathBuf> {
    if idx >= cx.entries().len() {
        return Err(RarError::InvalidState(
            "entry index is outside the current catalog".into(),
        ));
    }
    // Capture the member's payload position before the catalog can be
    // rebuilt. A quick-open catalog can order entries differently from
    // the full scan, so the index alone is not stable across the rescan
    // while the payload offset identifies the same member in both.
    let data_offset = cx.entries()[idx].chunks.first().map(|c| c.data_offset);
    let rebuilt = cx.read_ctx().quick_open_catalog;
    let dest = dest_dir.as_ref();
    fs::create_dir_all(dest)?;
    cx.read_ctx_mut().extract_options = opts;
    // A quick-open catalog carries no "STM" service records: replace it
    // with the scanned catalog before extraction restores streams.
    crate::format::rar5::extract::open::ensure_full_catalog(cx)?;
    let idx = if rebuilt {
        data_offset
            .and_then(|offset| {
                cx.entries().iter().position(|entry| {
                    entry
                        .chunks
                        .first()
                        .is_some_and(|chunk| chunk.data_offset == offset)
                })
            })
            .ok_or(RarError::StaleEntryId)?
    } else {
        idx
    };
    if idx >= cx.entries().len() {
        return Err(RarError::InvalidState(
            "entry index is outside the scanned catalog".into(),
        ));
    }
    validate_entry_limits(cx, idx)?;
    let entry = cx.entries()[idx].clone();
    extract_entry(cx, idx, &entry, dest, report)
}

/// Resolve the destination path for one member, applying the shared
/// extraction policies: flat extraction (`-e`), `-o-` (skip existing)
/// and `-or` (auto rename).
///
/// Flat extraction (`rar e` / `unrar e`) lands members in the
/// destination directory under their basename. The safe-path policy
/// always applies — the full member name is sanitized (which rejects
/// `..`/absolute/drive names) before its basename is used, so
/// traversal-shaped names cannot escape the destination. A directory
/// in flat mode resolves to the destination directory itself.
///
/// `Skip` means the destination exists and `skip_existing` is set, so
/// the caller must leave it untouched. Both the serial ([`extract_entry`])
/// and parallel (phase 3) paths call this, so they stay identical.
fn resolve_dest_path(
    cx: &dyn Engine,
    entry: &ArchiveEntry,
    dest_dir: &Path,
) -> RarResult<Destination> {
    resolve_dest_path_with(cx, entry, dest_dir, &cx.read_ctx().extract_options)
}

/// [`resolve_dest_path`] with explicit options, for callers that
/// resolve members outside the extraction loop.
pub(crate) fn resolve_dest_path_with(
    cx: &dyn Engine,
    entry: &ArchiveEntry,
    dest_dir: &Path,
    options: &crate::options::ExtractOptions,
) -> RarResult<Destination> {
    let dest_path = if options.flat_paths {
        if entry.is_dir() {
            return Ok(Destination::Extract(dest_dir.to_path_buf()));
        }
        let safe_name = sanitize_archive_path(&entry.header.name)?;
        let base = safe_name.rsplit('/').next().unwrap_or(&safe_name);
        dest_dir.join(base)
    } else {
        crate::format::shared::extract::dest::safe_dest_path_with(
            cx,
            dest_dir,
            &entry.header.name,
            options.safe_paths,
        )?
    };

    // `-o-` (skip existing): members whose destination already exists
    // are left untouched.
    if options.skip_existing && dest_path.exists() {
        return Ok(Destination::Skip(dest_path));
    }

    // `-f` / `-u` (freshen/update): only replace a destination that is
    // older than the archived member. A missing destination is skipped
    // by freshen and extracted by update.
    if !entry.is_dir() && (options.freshen || options.update) {
        match fs::metadata(&dest_path) {
            Ok(meta) => {
                let newer = match (member_mtime(&entry.header), meta.modified().ok()) {
                    (Some(archived), Some(dest_time)) => archived > dest_time,
                    _ => false,
                };
                if !newer {
                    return Ok(Destination::Skip(dest_path));
                }
            }
            Err(_) => {
                if options.freshen && !options.update {
                    return Ok(Destination::Skip(dest_path));
                }
            }
        }
    }

    // `-or` (auto rename): when the destination exists, insert `(N)`
    // before the extension (like WinRAR: a.txt -> a(1).txt).
    let mut dest_path = dest_path;
    if options.auto_rename && !entry.is_dir() {
        let mut n = 1;
        while dest_path.exists() {
            let file_name = dest_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let (stem, ext) = match file_name.rfind('.') {
                Some(dot) if dot > 0 => (&file_name[..dot], &file_name[dot..]),
                _ => (file_name.as_str(), ""),
            };
            dest_path = dest_path.with_file_name(format!("{stem}({n}){ext}"));
            n += 1;
        }
    }
    Ok(Destination::Extract(dest_path))
}

/// Extract one entry, recording the outcome in `report`. File contents
/// are decoded to a temporary file and renamed over the destination only
/// after integrity checks pass, so a failure leaves no output behind —
/// unless `-kb` asked for the partial file
/// ([`materialize_member_file`]).
fn extract_entry(
    cx: &mut dyn Engine,
    idx: usize,
    entry: &ArchiveEntry,
    dest_dir: &Path,
    report: &mut ExtractionReport,
) -> RarResult<PathBuf> {
    validate_entry_limits(cx, idx)?;

    let dest_path = match resolve_dest_path(cx, entry, dest_dir)? {
        Destination::Extract(path) => path,
        Destination::Skip(path) => {
            if reports_outcome(entry, &cx.read_ctx().extract_options) {
                report.record_skipped(path.clone());
            }
            return Ok(path);
        }
    };

    if entry.is_dir() {
        fs::create_dir_all(&dest_path)?;
        // Flat extraction resolves directories to the destination root
        // itself; the archived mode must not be applied to the caller's
        // directory.
        if dest_path.as_path() != dest_dir {
            crate::format::shared::extract::dest::apply_member_attributes(
                cx,
                &entry.header,
                &dest_path,
            );
        }
        return Ok(dest_path);
    }

    // RAR5 redirect records (symlinks, hardlinks, file copies): the
    // entry carries no data, only the target reference. `-ol-` skips
    // them entirely.
    if let Some(redir) = parse_redirect_record(&entry.header.extra_data) {
        if cx.read_ctx().extract_options.skip_links {
            return Ok(dest_path);
        }
        let path = crate::format::shared::extract::dest::extract_redirection(
            cx, dest_dir, &dest_path, &redir,
        )?;
        report.record_written(path.clone());
        return Ok(path);
    }

    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let keep_broken = cx.read_ctx().extract_options.keep_broken;
    materialize_member_file(&dest_path, keep_broken, |file| {
        crate::format::shared::extract::decode_entry_to(cx, idx, file).map(|_| ())
    })?;

    finish_member(cx, idx, entry, dest_path, report)
}

/// Post-write steps for one extracted file, shared by the serial and
/// parallel paths: restore mtime and attributes, attach NTFS streams,
/// carry the mark of the web over, and record the outcome. Returns the
/// destination path.
fn finish_member(
    cx: &mut dyn Engine,
    idx: usize,
    entry: &ArchiveEntry,
    dest_path: PathBuf,
    report: &mut ExtractionReport,
) -> RarResult<PathBuf> {
    // Restore mtime (best-effort), including the nanosecond fraction
    // from the FILE_TIME extra record when present.
    if crate::format::shared::entry_ext::file_header_has_mtime(&entry.header) {
        crate::format::shared::extract::dest::apply_member_times(cx, &entry.header, &dest_path);
    }
    // Restore NTFS alternate data streams attached to this member
    // (no-op on non-Windows, like the reference extractor).
    crate::format::shared::extract::dest::extract_member_streams(cx, idx, &dest_path)?;
    crate::format::shared::extract::dest::propagate_member_mark_of_the_web(cx, &dest_path);
    crate::format::shared::extract::dest::apply_member_attributes(cx, &entry.header, &dest_path);
    report.record_written(dest_path.clone());
    Ok(dest_path)
}
