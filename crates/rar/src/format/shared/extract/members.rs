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
use crate::fs::atomic::{read_write_create, replace_file, temp_sibling_path};
use crate::fs::safe_path::sanitize_archive_path_corrected;
#[cfg(feature = "parallel")]
use crate::model::FileHeader;
#[cfg(feature = "parallel")]
use crate::parallel::extraction_pool;

/// Memory budget for the optional parallel path: pre-read packed payloads
/// plus the decoded buffers of the members that decode at the same time.
/// Archives whose estimated peak exceeds it stream sequentially.
#[cfg(feature = "parallel")]
const PARALLEL_PEAK_BUDGET: u64 = 512 * 1024 * 1024;

/// Worker count the pool would use if the budget allowed it.
#[cfg(feature = "parallel")]
fn parallel_workers(opts: &crate::options::ExtractOptions) -> usize {
    extraction_pool(opts.threads).current_num_threads().max(1)
}

/// How many members may decode at once without exceeding the budget: the
/// pre-read payloads stay resident for the whole run, so the remaining
/// budget is filled with the largest decoded buffers first (the worst case
/// for a pool that picks up members in arbitrary order). Returns `0` when
/// not even one member fits.
#[cfg(feature = "parallel")]
fn live_workers_for(packed_total: u64, sizes: &[u64], workers: usize, budget: u64) -> usize {
    let mut largest: Vec<u64> = sizes.to_vec();
    largest.sort_unstable_by(|a, b| b.cmp(a));
    let mut resident = packed_total;
    let mut live = 0usize;
    for size in largest {
        if live == workers {
            break;
        }
        let next = resident.saturating_add(size);
        if next > budget {
            break;
        }
        resident = next;
        live += 1;
    }
    live
}
/// Per-member verify outcome keyed by catalog index.
#[cfg(feature = "parallel")]
type VerifyBatch = Vec<(usize, RarResult<()>)>;

/// Where a parallel worker gets a member's payload from.
#[cfg(feature = "parallel")]
enum ParallelPayload {
    /// RAR5: pre-read and decrypted through the single engine cursor.
    Modern(DecryptedPayload),
    /// Legacy: the worker opens volume 0 itself and the reader pulls the
    /// split chunks (and later volumes) it needs.
    Legacy,
}

/// State shared by the legacy parallel workers.
#[cfg(feature = "parallel")]
struct LegacyParallelContext {
    volume_paths: Vec<PathBuf>,
    password: Option<String>,
    max_packed_bytes: u64,
}

/// Whether `idx` belongs to a solid run in either family. Solid chains
/// share decoder state and must stay on the sequential path.
#[cfg(feature = "parallel")]
fn parallel_solid_member(cx: &dyn Engine, idx: usize) -> bool {
    if cx.is_legacy() {
        crate::format::rar4::extract::is_rar4_solid_member(cx, idx)
    } else {
        crate::format::rar5::extract::solid::is_solid_chain_member(cx, idx)
    }
}

#[cfg(feature = "parallel")]
fn legacy_parallel_context(cx: &mut dyn Engine) -> Option<LegacyParallelContext> {
    if !cx.is_legacy() {
        return None;
    }
    let (volume_paths, password) = {
        let p = cx.parts();
        (p.volume_paths.to_vec(), p.password.map(str::to_owned))
    };
    Some(LegacyParallelContext {
        volume_paths,
        password,
        max_packed_bytes: crate::format::shared::extract::max_packed_bytes(cx),
    })
}

/// Decode one non-solid legacy member from its own volume handle: reads
/// and decrypts the packed chunks (later volumes on demand), decodes, and
/// verifies the header CRC32.
#[cfg(feature = "parallel")]
fn decode_legacy_parallel(
    ctx: &LegacyParallelContext,
    hdr: &FileHeader,
    chunks: &[crate::model::DataChunk],
    opts: &crate::options::ExtractOptions,
) -> RarResult<Vec<u8>> {
    if let Some(limit) = opts.max_unpacked_bytes
        && hdr.unpacked_size > limit
    {
        return Err(RarError::limit_exceeded(
            limit,
            format!(
                "{}: unpacked size {} exceeds limit",
                hdr.name, hdr.unpacked_size
            ),
        ));
    }
    let mut file = File::open(&ctx.volume_paths[0])?;
    let data = crate::format::rar4::decode_member_bytes(
        &mut file,
        &ctx.volume_paths,
        chunks,
        hdr,
        crate::format::rar4::MemberDecodeOptions {
            password: ctx.password.as_deref(),
            decoder: None,
            max_alloc_packed_bytes: ctx.max_packed_bytes,
            max_stream_packed_bytes: ctx.max_packed_bytes,
        },
    )?;
    crate::format::rar4::extract::verify_member_crc(hdr, crc32fast::hash(&data))?;
    Ok(data)
}

/// Parallel extraction only engages for at least this many members...
#[cfg(feature = "parallel")]
const PARALLEL_MIN_MEMBERS: usize = 4;
/// ...and at least this much total unpacked data (Rayon overhead amortized).
#[cfg(feature = "parallel")]
const PARALLEL_MIN_UNPACKED: u64 = 8 * 1024 * 1024;

/// Destination resolution outcome for one member; the serial and parallel
/// extraction paths share it so `-e`, `-o-` and `-or` behave identically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    /// Extract the member to this path. `corrected` records that a Windows
    /// reserved device name was rewritten (WinRAR warns about that case).
    Extract {
        /// Where the member is written.
        path: PathBuf,
        /// Whether a device name was corrected during sanitization.
        corrected: bool,
    },
    /// `-o-`: the destination already exists and must be left untouched.
    Skip(PathBuf),
}

/// Whether an outcome for `entry` belongs in an [`ExtractionReport`]:
/// directories (their creation is silent) and `-ol-`-skipped links do not.
fn reports_outcome(entry: &ArchiveEntry, options: &crate::options::ExtractOptions) -> bool {
    !is_directory_entry(entry) && !(options.skip_links && entry.redirect().is_some())
}

/// Whether the member is a real directory, not a redirect that merely carries
/// WinRAR's DIRECTORY flag. WinRAR sets that flag on a junction (a directory
/// reparse point), but the member is still materialized as a link, so it must
/// never take the directory path — not in destination resolution (flat mode
/// folds directories into the destination root) and not in extraction.
fn is_directory_entry(entry: &ArchiveEntry) -> bool {
    entry.is_dir() && entry.redirect().is_none()
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

/// Install a staged member temp over its destination.
///
/// Regular files take the atomic [`replace_file`] path. A directory in the
/// way is handled the way WinRAR 7.30 does: an *empty* directory carrying the
/// member's name is replaced by the file once the overwrite policy allowed the
/// write (WhatsNew 7.30 beta 1: "If empty folder with the same name as
/// unpacking file exists and overwrite has been confirmed by a user, such
/// folder is replaced by the file"; older WinRAR failed with "Access is
/// denied"), while a non-empty directory is a create error. The directory
/// case has to be explicit on Windows, where `ReplaceFileW` cannot replace a
/// directory — and a failed install removes the staged temp rather than
/// leaving a stray `.name.rar5tmp-*` sibling behind.
fn install_member_file(tmp_path: &Path, dest_path: &Path) -> RarResult<()> {
    let result = if dest_path.is_dir() {
        match fs::remove_dir(dest_path) {
            Ok(()) => replace_file(tmp_path, dest_path),
            // A non-empty directory is a create error (WinRAR reports exit 9),
            // matching its `Cannot create ...: Directory with such name
            // already exists`.
            Err(_) => Err(RarError::create(
                dest_path,
                "directory with such name already exists",
            )),
        }
    } else {
        replace_file(tmp_path, dest_path)
    };
    if result.is_err() {
        let _ = fs::remove_file(tmp_path);
    }
    result
}

/// Create a directory member's destination, replacing a non-directory in the
/// way.
///
/// WinRAR replaces a file with a directory member once the overwrite policy
/// allowed the write (the mirror of the file-over-empty-directory case in
/// [`install_member_file`]). `create_dir_all` alone fails with `EEXIST`/
/// `ERROR_ALREADY_EXISTS` and would abort the run, so a plain file at the
/// destination is removed first.
fn materialize_directory(dest_path: &Path) -> RarResult<()> {
    match fs::symlink_metadata(dest_path) {
        Ok(meta) if !meta.is_dir() => fs::remove_file(dest_path)?,
        _ => {}
    }
    fs::create_dir_all(dest_path)?;
    Ok(())
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
        Ok(()) => install_member_file(&tmp_path, dest_path),
        Err(e) => {
            if keep_broken {
                // `-kb`: keep the partially extracted file.
                let _ = install_member_file(&tmp_path, dest_path);
            } else {
                let _ = fs::remove_file(&tmp_path);
            }
            Err(e)
        }
    }
}

/// What one extraction operation wrote and which members it left alone, each
/// in archive order.
///
/// Directory entries are neither: they carry no file data, so their creation
/// is silent. Members skipped by `-ol-` (`skip_links`) are not recorded
/// either. `skipped` covers the members the skip-existing policy left
/// untouched (`-o-`); a link refused because its target escapes the extraction
/// root (WinRAR's "Skipping the potentially unsafe ... link") is recorded
/// separately in `refused`. The report is the writer's own account — it cannot
/// disagree with what landed on disk, unlike a caller-side prediction.
///
/// `failures` is populated only under
/// [`ExtractErrorPolicy::Collect`](crate::ExtractErrorPolicy::Collect); with
/// the default `Abort` policy the first failing member ends the run and is
/// returned as the error instead, leaving no report to inspect.
///
/// Not `PartialEq`: the errors it can carry hold I/O sources, so two reports
/// of the same extraction are not comparable in general. Compare
/// [`written`](Self::written) and friends individually. Not `Clone` either:
/// a report owns its failures, and a failure cannot be duplicated.
#[derive(Debug, Default)]
pub struct ExtractionReport {
    written: Vec<PathBuf>,
    skipped: Vec<PathBuf>,
    refused: Vec<PathBuf>,
    corrected: Vec<String>,
    failures: Vec<ExtractionFailure>,
}

/// One member an extraction could not produce, under
/// [`ExtractErrorPolicy::Collect`](crate::ExtractErrorPolicy::Collect).
///
/// Not `Clone`: an error can hold a non-clonable I/O source. Keep the failure
/// in the report and borrow from it, or take it out with
/// [`into_parts`](Self::into_parts).
#[derive(Debug)]
pub struct ExtractionFailure {
    /// The member's stored name, so a front end can name it without
    /// resolving an id against a reader that may already be gone.
    name: String,
    /// Archive-order catalog index of the member.
    index: usize,
    /// The failure itself: decode, integrity check, or install.
    error: RarError,
}

impl ExtractionFailure {
    /// The stored name of the member that failed.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The member's archive-order position in the catalog.
    pub const fn index(&self) -> usize {
        self.index
    }

    /// The error that stopped this member.
    pub const fn error(&self) -> &RarError {
        &self.error
    }

    /// Consume the failure, yielding the member name and its error.
    pub fn into_parts(self) -> (String, RarError) {
        (self.name, self.error)
    }
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

    /// Destination paths of the links refused because their target escapes
    /// the extraction root, in archive order. Kept apart from [`Self::skipped`]
    /// so a caller can treat a refused link as a warning (WinRAR exits 1)
    /// rather than as an ordinary skip.
    pub fn refused(&self) -> &[PathBuf] {
        &self.refused
    }

    /// Number of links refused for an unsafe target.
    pub fn refused_count(&self) -> usize {
        self.refused.len()
    }

    /// Stored names whose Windows reserved device name was corrected during
    /// extraction (WinRAR prints `WARNING: Attempting to correct the invalid
    /// file or directory name` for each). Empty on POSIX.
    pub fn corrected(&self) -> &[String] {
        &self.corrected
    }

    /// Members this run could not extract, in archive order.
    ///
    /// Populated only under
    /// [`ExtractErrorPolicy::Collect`](crate::ExtractErrorPolicy::Collect);
    /// the default `Abort` policy returns the first failure as an error
    /// instead, so a caller never sees a report that silently hid a failure.
    pub fn failures(&self) -> &[ExtractionFailure] {
        &self.failures
    }

    /// Number of members that could not be extracted.
    pub fn failed_count(&self) -> usize {
        self.failures.len()
    }

    /// Take the collected failures, consuming the report.
    pub fn into_failures(self) -> Vec<ExtractionFailure> {
        self.failures
    }

    /// Whether this run extracted nothing at all.
    ///
    /// True for a genuinely empty archive and for a run whose every member
    /// was skipped or refused; a caller that must not report "extracted" for a
    /// wholly failed run should check [`Self::failed_count`] as well.
    pub fn is_empty(&self) -> bool {
        self.written.is_empty() && self.skipped.is_empty() && self.refused.is_empty()
    }

    pub(crate) fn record_written(&mut self, path: PathBuf) {
        self.written.push(path);
    }

    pub(crate) fn record_skipped(&mut self, path: PathBuf) {
        self.skipped.push(path);
    }

    pub(crate) fn record_refused(&mut self, path: PathBuf) {
        self.refused.push(path);
    }

    pub(crate) fn record_corrected(&mut self, name: String) {
        self.corrected.push(name);
    }

    pub(crate) fn record_failure(&mut self, failure: ExtractionFailure) {
        self.failures.push(failure);
    }
}

/// Validate per-entry header limits against the current extract options.
pub(crate) fn validate_entry_limits(cx: &dyn Engine, idx: usize) -> RarResult<()> {
    let hdr = &cx.entries()[idx].header;
    if hdr.comp_dict_size > MAX_DICT_SIZE_LOG {
        return Err(RarError::limit_exceeded(
            MAX_DICT_SIZE_LOG as u64,
            format!(
                "{}: dictionary size log {} exceeds supported maximum {}",
                hdr.name, hdr.comp_dict_size, MAX_DICT_SIZE_LOG
            ),
        ));
    }
    if let Some(limit) = cx.read_ctx().extract_options.max_unpacked_bytes
        && hdr.unpacked_size > limit
    {
        return Err(RarError::limit_exceeded(
            limit,
            format!(
                "{}: unpacked size {} exceeds limit",
                hdr.name, hdr.unpacked_size
            ),
        ));
    }
    Ok(())
}

/// Start the per-run progress tracker for `on_progress`, reporting against
/// `total` (the summed uncompressed size of the members this run will write).
///
/// The caller's closure is **borrowed through a clone of the shared sink, not
/// taken out of it**. `ExtractOptions` is `Clone` and documents that a clone
/// reports to the same callback, so consuming the closure here would silence
/// every later run that reuses the value — silently, since the sink is an
/// `Option` and an emptied one just looks like "no callback". The per-member
/// baseline lives in the fresh tracker, so two runs sharing one sink each
/// count from their own zero and both reach their own total.
///
/// Returns `None` when the caller installed no sink, and reports nothing when
/// the sink is poisoned (an earlier callback panicked on a build where the
/// guard was not held), matching the writer's "a bad callback cannot break the
/// operation" policy.
pub(crate) fn start_progress(
    on_progress: Option<crate::options::ExtractionProgress>,
    total: u64,
) -> Option<std::sync::Arc<std::sync::Mutex<crate::write_progress::ProgressTracker>>> {
    let sink = on_progress?;
    let callback: crate::options::ExtractionCallback = Box::new(move |committed, total| {
        if let Ok(mut guard) = sink.lock()
            && let Some(callback) = guard.as_mut()
        {
            callback(committed, total);
        }
    });
    let mut tracker = crate::write_progress::ProgressTracker::new(Some(callback));
    tracker.set_total(total);
    Some(std::sync::Arc::new(std::sync::Mutex::new(tracker)))
}

/// The one place a multi-member extraction decides what a member's outcome
/// means: advance progress for a member that landed, then apply
/// `error_policy` — [`ExtractErrorPolicy::Abort`] propagates the first
/// failure, [`ExtractErrorPolicy::Collect`] records it and lets the run
/// continue.
///
/// Both extraction loops call this (the whole-archive one and the
/// id-selected one behind [`ArchiveReader::extract_ids_with_options`]), so
/// `Collect` cannot mean "abort" on one of them. A cancellation is the
/// caller's own request, never a per-member failure, so it always aborts.
pub(crate) fn record_member_outcome(
    member: usize,
    name: &str,
    size: u64,
    outcome: RarResult<PathBuf>,
    policy: crate::options::ExtractErrorPolicy,
    report: &mut ExtractionReport,
    progress: Option<&std::sync::Arc<std::sync::Mutex<crate::write_progress::ProgressTracker>>>,
) -> RarResult<()> {
    match outcome {
        Ok(_) => {
            if let Some(progress) = progress {
                progress
                    .lock()
                    .expect("progress lock")
                    .report(member, size, size);
            }
            Ok(())
        }
        Err(error) if error.code() == crate::ErrorCode::Cancelled => Err(error),
        Err(error) => match policy {
            crate::options::ExtractErrorPolicy::Abort => Err(error),
            crate::options::ExtractErrorPolicy::Collect => {
                report.record_failure(ExtractionFailure {
                    name: name.to_string(),
                    index: member,
                    error,
                });
                Ok(())
            }
        },
    }
}

/// Extract all archive contents with explicit options, returning what was
/// written and what the skip-existing policy left untouched.
pub(crate) fn extract_all_with_options(
    cx: &mut dyn Engine,
    dest_dir: impl AsRef<Path>,
    mut opts: crate::options::ExtractOptions,
) -> RarResult<ExtractionReport> {
    let dest = dest_dir.as_ref();
    fs::create_dir_all(dest)?;
    cx.read_ctx_mut().extract_options = opts.clone();
    // A quick-open catalog carries no "STM" service records: replace it
    // with the scanned catalog before extraction restores streams.
    crate::format::shared::extract::ensure_full_catalog(cx)?;

    // The progress total is the whole catalog's uncompressed size, known only
    // after the full catalog exists.
    let total: u64 = cx
        .entries()
        .iter()
        .map(|entry| entry.header.unpacked_size)
        .fold(0u64, u64::saturating_add);
    let progress = start_progress(opts.on_progress.take(), total);

    #[cfg(feature = "parallel")]
    {
        if let Some(report) = extract_all_parallel(cx, dest, opts.clone())? {
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
            .ok_or_else(|| {
                RarError::limit_exceeded(
                    opts.max_total_unpacked_bytes.unwrap_or(u64::MAX),
                    "total unpacked size overflow",
                )
            })?;
        if let Some(limit) = opts.max_total_unpacked_bytes
            && total_unpacked > limit
        {
            return Err(RarError::limit_exceeded(
                limit,
                format!(
                    "total unpacked size {total_unpacked} exceeds limit while extracting {}",
                    entry.name()
                ),
            ));
        }
        let outcome = extract_entry(cx, index, entry, dest, &mut report);
        record_member_outcome(
            index,
            entry.name(),
            entry.header.unpacked_size,
            outcome,
            opts.error_policy,
            &mut report,
            progress.as_ref(),
        )?;
    }
    // Under `Collect` the report is the outcome even when nothing landed:
    // `failed_count()` and `is_empty()` let the caller decide whether an
    // all-failed run is an error, instead of this layer guessing.
    Ok(report)
}

/// Parallel extraction for eligible archives (optional `parallel`
/// feature).
///
/// Eligible: at least [`PARALLEL_MIN_MEMBERS`] members, no solid chains,
/// no progress callback, and total packed + unpacked sizes within a
/// bounded memory budget. RAR5 payloads are read and decrypted
/// sequentially (one engine cursor); legacy workers open their own volume
/// handle, so split and multi-volume members parallelize too. Decoded
/// members are integrity-checked on Rayon workers and the writes are
/// replayed in archive order, overlapping I/O with the decode of later
/// members. Ineligible archives fall back to the sequential path
/// unchanged.
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
    // The prompt is interactive and order-sensitive: keep it on the serial
    // path so answers apply in archive order.
    if opts.prompt_overwrite {
        return Ok(None);
    }
    // Per-member outcomes need archive order and a decision per member: the
    // parallel path decodes every member up front, so `Abort` could not stop
    // at the first failure and `Collect` would report a failure the serial
    // ordering had already passed. Both go to the serial path.
    if opts.error_policy != crate::options::ExtractErrorPolicy::Abort {
        return Ok(None);
    }
    // Progress is per-member, so it needs the serial ordering this path
    // gives up; a run that asked for progress reports from the serial loop.
    if cx.progress_slot().is_some()
        || opts.on_progress.is_some()
        || cx.entries().len() < PARALLEL_MIN_MEMBERS
    {
        return Ok(None);
    }
    for i in 0..cx.entries().len() {
        if parallel_solid_member(cx, i) {
            return Ok(None);
        }
    }
    let rar5 = !cx.is_legacy();
    let mut total_packed = 0u64;
    let mut total_unpacked = 0u64;
    let mut sizes = Vec::with_capacity(cx.entries().len());
    for e in cx.entries() {
        total_packed = total_packed.saturating_add(e.header.packed_size);
        total_unpacked = total_unpacked.saturating_add(e.header.unpacked_size);
        sizes.push(e.header.unpacked_size);
    }
    let packed_budget = if rar5 { total_packed } else { 0 };
    // Throttle concurrency to the memory budget instead of sending the whole
    // archive to the serial path: 8×90 MB members with 200 MB of pre-read
    // payloads run three at a time instead of one after another.
    let live = live_workers_for(
        packed_budget,
        &sizes,
        parallel_workers(&opts),
        PARALLEL_PEAK_BUDGET,
    );
    if live < 2 {
        return Ok(None);
    }
    if total_unpacked < PARALLEL_MIN_UNPACKED {
        return Ok(None);
    }
    if let Some(limit) = opts.max_total_unpacked_bytes
        && total_unpacked > limit
    {
        return Err(RarError::limit_exceeded(
            limit,
            "total unpacked size exceeds limit",
        ));
    }

    // Phase 1 (RAR5 only): read + decrypt all payloads sequentially. A
    // legacy worker needs no such pass: it opens volume 0 itself.
    let legacy_ctx = legacy_parallel_context(cx);
    let payloads: Vec<(usize, ParallelPayload)> = if legacy_ctx.is_some() {
        (0..cx.entries().len())
            .map(|i| (i, ParallelPayload::Legacy))
            .collect()
    } else {
        let mut payloads = Vec::with_capacity(cx.entries().len());
        for i in 0..cx.entries().len() {
            payloads.push((
                i,
                ParallelPayload::Modern(crate::format::rar5::extract::decode::read_packed_data(
                    cx, i,
                )?),
            ));
        }
        payloads
    };
    let headers: Vec<FileHeader> = cx.entries().iter().map(|e| e.header.clone()).collect();
    let legacy_chunks: Vec<Vec<crate::model::DataChunk>> = if legacy_ctx.is_some() {
        cx.entries().iter().map(|e| e.chunks.clone()).collect()
    } else {
        Vec::new()
    };

    /// One member decoded in phase 2: `error` carries the failure that
    /// interrupted the decode, with `data` holding the bytes produced so
    /// far so the replay can honor `-kb`.
    struct DecodedMember {
        idx: usize,
        data: Vec<u8>,
        error: Option<RarError>,
    }

    // Phase 2+3: decode and integrity-check on the Rayon pool while this
    // thread replays the finished members in archive order. Writes stay
    // single-threaded (all `-o-`/`-kb`/overwrite semantics live here), but
    // they overlap with the decode of later members instead of starting
    // after a full parallel pass — on write-bound runs the wall time is
    // then roughly the write time, not decode + write. Results arrive out
    // of order; `pending` re-orders them and holds at most a small window
    // because the workers block on the bounded channel.
    let keep_broken = cx.read_ctx().extract_options.keep_broken;
    let payload_count = payloads.len();
    let window = crate::parallel::pool_threads(4).max(1) * 2;
    let (tx, rx) = std::sync::mpsc::sync_channel::<DecodedMember>(window);
    let mut pending: std::collections::BTreeMap<usize, DecodedMember> =
        std::collections::BTreeMap::new();
    let mut next = 0usize;
    let mut report = ExtractionReport::default();
    let replay = |cx: &mut dyn Engine,
                  report: &mut ExtractionReport,
                  member: DecodedMember|
     -> RarResult<()> {
        let idx = member.idx;
        // `resolve_member_target` and `finish_member` need the archive, so
        // the entry is cloned out of the catalog first (the serial loop
        // keeps a whole-catalog snapshot for the same reason).
        let entry = cx.entries()[idx].clone();
        // The directory/redirect/skip prelude is shared with the serial path.
        let dest_path = match resolve_member_target(cx, &entry, dest, report)? {
            MemberTarget::Done(_) => return Ok(()),
            MemberTarget::File(path) => path,
        };
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
                Ok(()) => Err(RarError::invalid_state("failed member staged as complete")),
            };
        }
        materialize_member_file(&dest_path, keep_broken, |file| {
            file.write_all(&member.data)?;
            Ok(())
        })?;
        finish_member(cx, idx, &entry, dest_path, report)?;
        Ok(())
    };
    let outcome: RarResult<()> = std::thread::scope(|scope| {
        scope.spawn(|| {
            payloads
                .into_par_iter()
                .for_each_with(tx, |tx, (i, payload)| {
                    let hdr = &headers[i];
                    // The one member decoder (STORE bound, decode, size check)
                    // the serial paths use; the Vec sink keeps the decoded bytes
                    // for the ordered replay on the other side of the channel.
                    let mut data = Vec::new();
                    let mut error = None;
                    let decoded = (|| -> RarResult<()> {
                        if let Some(limit) = opts.max_unpacked_bytes
                            && hdr.unpacked_size > limit
                        {
                            return Err(RarError::limit_exceeded(
                                limit,
                                format!(
                                    "{}: unpacked size {} exceeds limit",
                                    hdr.name, hdr.unpacked_size
                                ),
                            ));
                        }
                        match payload {
                            ParallelPayload::Modern(payload) => {
                                if hdr.comp_dict_size > MAX_DICT_SIZE_LOG {
                                    return Err(RarError::limit_exceeded(
                                        MAX_DICT_SIZE_LOG as u64,
                                        format!(
                                            "{}: dictionary size log {} exceeds supported maximum {}",
                                            hdr.name, hdr.comp_dict_size, MAX_DICT_SIZE_LOG
                                        ),
                                    ));
                                }
                                let _ = capped_dict_bytes(hdr, opts.max_dict_size)?;
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
                            }
                            ParallelPayload::Legacy => {
                                let ctx = legacy_ctx.as_ref().ok_or_else(|| {
                                    RarError::invalid_state("legacy parallel context missing")
                                })?;
                                data = decode_legacy_parallel(ctx, hdr, &legacy_chunks[i], &opts)?;
                                Ok(())
                            }
                        }
                    })();
                    if let Err(e) = decoded {
                        error = Some(e);
                    }
                    // A closed receiver means the run already failed; dropping
                    // the member and stopping is correct.
                    let _ = tx.send(DecodedMember {
                        idx: i,
                        data,
                        error,
                    });
                })
        });
        for member in rx.iter() {
            pending.insert(member.idx, member);
            while let Some(member) = pending.remove(&next) {
                replay(cx, &mut report, member)?;
                next += 1;
            }
        }
        Ok(())
    });
    outcome?;
    if !pending.is_empty() || next != payload_count {
        return Err(RarError::invalid_state(
            "parallel extraction lost a member result",
        ));
    }
    Ok(Some(report))
}

/// Verify the listed catalog indices in parallel, or `None` when the
/// archive is not eligible (callers then take the serial path).
///
/// The gates mirror [`extract_all_parallel`]: RAR5, at least
/// [`PARALLEL_MIN_MEMBERS`] members, single-chunk non-solid members, no
/// progress callback (progress is per-member and ordered), and a bounded
/// in-memory budget. Packed payloads are read sequentially because a single
/// `Engine` cursor cannot be shared across workers; decoding and integrity
/// checking — the CPU-bound part — then runs on the Rayon pool.
///
/// Results are returned in the caller's index order with the decoded bytes
/// discarded, so a `t` run holds at most the packed payloads plus the
/// largest member buffer per worker.
#[cfg(feature = "parallel")]
pub(crate) fn verify_indices_parallel(
    cx: &mut dyn Engine,
    indices: &[usize],
    opts: &crate::options::ExtractOptions,
) -> RarResult<Option<VerifyBatch>> {
    use rayon::prelude::*;

    if !crate::format::shared::extract::supports_parallel_extract(cx)
        || indices.len() < PARALLEL_MIN_MEMBERS
        || cx.progress_slot().is_some()
        || opts.on_progress.is_some()
    {
        return Ok(None);
    }
    if opts.prompt_overwrite {
        return Ok(None);
    }
    let rar5 = !cx.is_legacy();
    let mut total_packed = 0u64;
    let mut total_unpacked = 0u64;
    let mut sizes = Vec::with_capacity(indices.len());
    for &idx in indices {
        let Some(entry) = cx.entries().get(idx) else {
            return Ok(None);
        };
        if parallel_solid_member(cx, idx) {
            return Ok(None);
        }
        total_packed = total_packed.saturating_add(entry.header.packed_size);
        total_unpacked = total_unpacked.saturating_add(entry.header.unpacked_size);
        sizes.push(entry.header.unpacked_size);
    }
    let packed_budget = if rar5 { total_packed } else { 0 };
    // Throttle concurrency to the memory budget instead of sending the whole
    // archive to the serial path: 8×90 MB members with 200 MB of pre-read
    // payloads run three at a time instead of one after another.
    let live = live_workers_for(
        packed_budget,
        &sizes,
        parallel_workers(opts),
        PARALLEL_PEAK_BUDGET,
    );
    if live < 2 {
        return Ok(None);
    }
    if total_unpacked < PARALLEL_MIN_UNPACKED {
        return Ok(None);
    }
    if let Some(limit) = opts.max_total_unpacked_bytes
        && total_unpacked > limit
    {
        return Err(RarError::limit_exceeded(
            limit,
            "total unpacked size exceeds limit",
        ));
    }

    // Phase 1 (RAR5 only): read + decrypt payloads through the single
    // engine cursor. Legacy workers read their own volume instead.
    let legacy_ctx = legacy_parallel_context(cx);
    let payloads: Vec<(usize, ParallelPayload)> = if legacy_ctx.is_some() {
        indices
            .iter()
            .map(|&idx| (idx, ParallelPayload::Legacy))
            .collect()
    } else {
        let mut payloads = Vec::with_capacity(indices.len());
        for &idx in indices {
            cx.check_cancel()?;
            payloads.push((
                idx,
                ParallelPayload::Modern(crate::format::rar5::extract::decode::read_packed_data(
                    cx, idx,
                )?),
            ));
        }
        payloads
    };
    let headers: Vec<FileHeader> = payloads
        .iter()
        .map(|(idx, _)| cx.entries()[*idx].header.clone())
        .collect();
    let legacy_chunks: Vec<Vec<crate::model::DataChunk>> = if legacy_ctx.is_some() {
        payloads
            .iter()
            .map(|(idx, _)| cx.entries()[*idx].chunks.clone())
            .collect()
    } else {
        Vec::new()
    };

    // Phase 2: decode + integrity-check in parallel. Every worker owns its
    // header/payload and drops the decoded bytes as soon as the hashes are
    // computed.
    let results = extraction_pool(Some(live)).install(|| {
        payloads
            .into_par_iter()
            .enumerate()
            .map(|(slot, (idx, payload))| {
                let hdr = &headers[slot];
                let outcome = (|| -> RarResult<()> {
                    if let Some(limit) = opts.max_unpacked_bytes
                        && hdr.unpacked_size > limit
                    {
                        return Err(RarError::limit_exceeded(
                            limit,
                            format!(
                                "{}: unpacked size {} exceeds limit",
                                hdr.name, hdr.unpacked_size
                            ),
                        ));
                    }
                    match payload {
                        ParallelPayload::Modern(payload) => {
                            if hdr.comp_dict_size > MAX_DICT_SIZE_LOG {
                                return Err(RarError::limit_exceeded(
                                    MAX_DICT_SIZE_LOG as u64,
                                    format!(
                                        "{}: dictionary size log {} exceeds supported maximum {}",
                                        hdr.name, hdr.comp_dict_size, MAX_DICT_SIZE_LOG
                                    ),
                                ));
                            }
                            let _ = capped_dict_bytes(hdr, opts.max_dict_size)?;
                            let mut data = Vec::new();
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
                        }
                        ParallelPayload::Legacy => {
                            let ctx = legacy_ctx.as_ref().ok_or_else(|| {
                                RarError::invalid_state("legacy parallel context missing")
                            })?;
                            decode_legacy_parallel(ctx, hdr, &legacy_chunks[slot], opts)?;
                            Ok(())
                        }
                    }
                })();
                (idx, outcome)
            })
            .collect()
    });
    cx.check_cancel()?;
    Ok(Some(results))
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
        return Err(RarError::invalid_state(
            "entry index is outside the current catalog",
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
    cx.read_ctx_mut().extract_options = opts.clone();
    // A quick-open catalog carries no "STM" service records: replace it
    // with the scanned catalog before extraction restores streams.
    crate::format::shared::extract::ensure_full_catalog(cx)?;
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
        return Err(RarError::invalid_state(
            "entry index is outside the scanned catalog",
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
    // `-ver` without a number extracts every version and keeps the version in
    // the file name (`name;N`), like WinRAR.
    let mut member_name = entry.header.name.clone();
    if options.file_version_suffix
        && let Some(version) = entry.file_version()
        && version > 0
    {
        member_name = format!("{member_name};{version}");
    }
    let (dest_path, corrected) = if options.flat_paths {
        if is_directory_entry(entry) {
            return Ok(Destination::Extract {
                path: dest_dir.to_path_buf(),
                corrected: false,
            });
        }
        let (safe_name, corrected) =
            sanitize_archive_path_corrected(&member_name, options.allow_incompatible_names)?;
        let base = safe_name.rsplit('/').next().unwrap_or(&safe_name);
        (dest_dir.join(base), corrected)
    } else {
        crate::format::shared::extract::dest::safe_dest_path_with_correction(
            cx,
            dest_dir,
            &member_name,
            options.safe_paths,
            options.allow_incompatible_names,
        )?
    };

    // `-o-` (skip existing): members whose destination already exists
    // are left untouched.
    if options.skip_existing && dest_path.exists() {
        return Ok(Destination::Skip(dest_path));
    }

    // Interactive overwrite prompt (WinRAR's console mode): with an installed
    // prompt and `prompt_overwrite`, ask about each existing file. Without a
    // prompt installed the flag falls back to skipping, never a silent
    // overwrite. Directories never prompt (an existing directory is a no-op),
    // and `-or` (`auto_rename`) takes precedence as documented: with both set
    // the member is renamed without asking.
    if options.prompt_overwrite
        && !options.auto_rename
        && !is_directory_entry(entry)
        && dest_path.exists()
    {
        let prompt = cx.read_ctx().overwrite_prompt.clone();
        let choice = match prompt {
            Some(prompt) => prompt(&dest_path),
            None => crate::options::OverwriteChoice::Skip,
        };
        match choice {
            crate::options::OverwriteChoice::Overwrite
            | crate::options::OverwriteChoice::OverwriteAll => {}
            crate::options::OverwriteChoice::Skip => {
                return Ok(Destination::Skip(dest_path));
            }
            crate::options::OverwriteChoice::Rename => {
                return Ok(Destination::Extract {
                    path: next_free_name(&dest_path),
                    corrected,
                });
            }
            crate::options::OverwriteChoice::Quit => return Err(RarError::Cancelled),
        }
    }

    // `-f` / `-u` (freshen/update): only replace a destination that is
    // older than the archived member. A missing destination is skipped
    // by freshen and extracted by update.
    if !is_directory_entry(entry) && (options.freshen || options.update) {
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
    if options.auto_rename && !is_directory_entry(entry) {
        dest_path = next_free_name(&dest_path);
    }
    Ok(Destination::Extract {
        path: dest_path,
        corrected,
    })
}

/// The first free sibling of `path` in the `name(N).ext` numbering WinRAR
/// uses for `-or` and the interactive "Rename" answer (`a.txt` ->
/// `a(1).txt`, then `a(2).txt`, ...).
fn next_free_name(path: &Path) -> PathBuf {
    // The stem and extension come from the *original* name: re-deriving them
    // from each renamed candidate would nest the suffix (`a(1)(2).txt`
    // instead of `a(2).txt`).
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (stem, ext) = match file_name.rfind('.') {
        Some(dot) if dot > 0 => (&file_name[..dot], &file_name[dot..]),
        _ => (file_name.as_str(), ""),
    };
    let mut dest = path.to_path_buf();
    let mut n = 1;
    while dest.exists() {
        dest = path.with_file_name(format!("{stem}({n}){ext}"));
        n += 1;
    }
    dest
}

/// The pre-write outcome for one member: either it is fully handled (a
/// directory created, a redirect written, a skipped/refused member) or there
/// is a file destination left to write.
enum MemberTarget {
    /// Nothing left to write; the path is the produced one (or the skipped
    /// destination).
    Done(PathBuf),
    /// A regular file destination to materialize.
    File(PathBuf),
}

/// Resolve one member's destination and handle the cases that carry no file
/// payload: a directory entry, a redirect member, and the skip/refuse
/// policies. Shared by the serial ([`extract_entry`]) and parallel replay
/// paths so the directory/redirect/`-ol-` policy cannot drift between them.
fn resolve_member_target(
    cx: &mut dyn Engine,
    entry: &ArchiveEntry,
    dest_dir: &Path,
    report: &mut ExtractionReport,
) -> RarResult<MemberTarget> {
    let dest_path = match resolve_dest_path(cx, entry, dest_dir)? {
        Destination::Extract { path, corrected } => {
            if corrected && reports_outcome(entry, &cx.read_ctx().extract_options) {
                report.record_corrected(entry.name().to_string());
            }
            path
        }
        Destination::Skip(path) => {
            if reports_outcome(entry, &cx.read_ctx().extract_options) {
                report.record_skipped(path.clone());
            }
            return Ok(MemberTarget::Done(path));
        }
    };

    if is_directory_entry(entry) {
        materialize_directory(&dest_path)?;
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
        return Ok(MemberTarget::Done(dest_path));
    }

    // RAR5 redirect records (symlinks, hardlinks, file copies): the entry
    // carries no data, only the target reference. `-ol-` skips them entirely.
    if let Some(redir) = crate::format::shared::entry_ext::redirect_of(entry) {
        if cx.read_ctx().extract_options.skip_links {
            return Ok(MemberTarget::Done(dest_path));
        }
        return match crate::format::shared::extract::dest::extract_redirection(
            cx, dest_dir, &dest_path, &redir,
        )? {
            Some(path) => {
                report.record_written(path.clone());
                Ok(MemberTarget::Done(path))
            }
            // Refused link target: skip just this member, like WinRAR.
            None => {
                report.record_refused(dest_path.clone());
                Ok(MemberTarget::Done(dest_path))
            }
        };
    }

    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(MemberTarget::File(dest_path))
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

    let dest_path = match resolve_member_target(cx, entry, dest_dir, report)? {
        MemberTarget::Done(path) => return Ok(path),
        MemberTarget::File(path) => path,
    };

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

#[cfg(all(test, feature = "parallel"))]
mod parallel_budget_tests {
    use super::live_workers_for;

    /// Budget 100, packed 10: 90 + 10 fits one member, 45 + 10 fits two,
    /// and the largest are admitted first (worst case for the pool).
    #[test]
    fn concurrency_is_throttled_by_the_resident_budget() {
        assert_eq!(live_workers_for(10, &[90, 45, 45], 8, 100), 1);
        assert_eq!(live_workers_for(10, &[45, 45, 45, 45], 8, 100), 2);
        assert_eq!(live_workers_for(10, &[10, 10, 10, 10], 3, 100), 3);
        // Nothing fits: the caller must fall back to the serial path.
        assert_eq!(live_workers_for(100, &[90], 8, 150), 0);
        // Fewer members than workers: everything may run at once.
        assert_eq!(live_workers_for(0, &[1, 2, 3], 24, 8), 3);
    }
}
