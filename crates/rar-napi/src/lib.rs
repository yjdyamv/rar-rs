//! RAR archive operations for the Smart Archive VS Code extension.
//!
//! Wraps the pure-Rust `rar5` crate behind a napi-rs API for creating, reading,
//! testing, listing, extracting, repairing, and modifying RAR archives.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;

/// Maximum per-file size read into memory by the rar5 library (4 GiB).
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Maximum total input size across all entries (32 GiB).
const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024 * 1024;
/// Largest integer that a JavaScript `number` can represent exactly.
const JS_MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[napi(object)]
pub struct EntryInput {
  /// "file" | "dir" | "bytes"
  pub kind: String,
  /// Filesystem path for "file" and "dir" entries.
  pub path: Option<String>,
  /// Archive entry name. For "file"/"dir" defaults to the basename, for
  /// "bytes" it is required.
  pub name: Option<String>,
  /// Byte payload for "bytes" entries.
  pub data: Option<Buffer>,
}

#[napi(object)]
pub struct CreateArchiveOptions {
  pub out_path: String,
  pub entries: Vec<EntryInput>,
  /// Compression level 0..=5 (default 3).
  pub level: Option<f64>,
  /// Optional AES-256 password (file-level encryption).
  pub password: Option<String>,
  /// Also encrypt the archive structure (file names). Requires `password`;
  /// incompatible with multi-volume.
  pub encrypt_headers: Option<bool>,
  /// Add a WinRAR-compatible inline recovery record protecting this percent
  /// (0-100) of the archive. Incompatible with multi-volume.
  pub recovery_percent: Option<f64>,
  /// Create this many `.rev` recovery volumes (WinRAR `-rv`); auto-capped
  /// at the actual data volume count. Requires `volume_size`.
  pub recovery_volume_count: Option<f64>,
  /// Volume size in bytes; when set, produces multi-volume archives
  /// (`name.part1.rar`, ...).
  pub volume_size: Option<f64>,
  /// Reject the operation when the summed input size exceeds this.
  pub max_total_bytes: Option<f64>,
  /// Dictionary size (like WinRAR `-md<size>[k|m|g]`, no unit = MiB).
  /// Values up to 4 GiB must be powers of two (128 KiB .. 4 GiB); values
  /// above 4 GiB are accepted as-is and produce RAR7 (v70) archives.
  pub dict_size: Option<String>,
  /// Create a solid archive (better ratio, slower random access).
  pub solid: Option<bool>,
  /// Add a quick-open record for fast member listing.
  pub quick_open: Option<bool>,
  /// Write BLAKE2sp hash records for every member (like WinRAR `-htb`).
  pub blake2: Option<bool>,
  /// Compression threads (1..=64).
  pub threads: Option<f64>,
  /// Save the creation time (Windows) / ctime (Unix) in the FILE_TIME
  /// extra record (like WinRAR `-tsc`).
  pub save_ctime: Option<bool>,
  /// Save the last access time (like WinRAR `-tsa`).
  pub save_atime: Option<bool>,
  /// Store timestamps at 1-second precision (like WinRAR `-ts...1`).
  pub time_precision_seconds: Option<bool>,
  /// Save the owner and group (numeric ids) on Unix (like WinRAR `-ow`).
  pub save_owner: Option<bool>,
  /// Save NTFS alternate data streams (like WinRAR `-os`; Windows only).
  pub save_streams: Option<bool>,
}

#[napi(object)]
pub struct ProgressData {
  pub done: f64,
  pub total: f64,
}

#[napi(object)]
pub struct CreateResult {
  /// Paths of all files produced (single archive or volumes).
  pub files: Vec<String>,
}

struct PlannedEntry {
  kind: String,
  path: Option<PathBuf>,
  name: String,
  data: Option<Vec<u8>>,
}

// ── Conversions from the JS-facing option structs to the rar5 library
// structs, so each field mapping lives in one place next to its struct.

fn checked_js_integer(value: f64, field: &str, min: u64, max: u64) -> Result<u64> {
  if !value.is_finite() || value.fract() != 0.0 || value.abs() > JS_MAX_SAFE_INTEGER {
    return Err(Error::new(
      Status::InvalidArg,
      format!("`{field}` must be a safe integer"),
    ));
  }
  if value < min as f64 || value > max as f64 {
    return Err(Error::new(
      Status::InvalidArg,
      format!("`{field}` must be in the range {min}..={max}"),
    ));
  }
  Ok(value as u64)
}

fn checked_optional_js_integer(
  value: Option<f64>,
  field: &str,
  min: u64,
  max: u64,
) -> Result<Option<u64>> {
  value
    .map(|value| checked_js_integer(value, field, min, max))
    .transpose()
}

impl CreateArchiveOptions {
  fn level(&self) -> Result<u8> {
    checked_js_integer(self.level.unwrap_or(3.0), "level", 0, 5).map(|value| value as u8)
  }

  fn max_total_bytes(&self) -> Result<Option<u64>> {
    checked_optional_js_integer(
      self.max_total_bytes,
      "maxTotalBytes",
      0,
      JS_MAX_SAFE_INTEGER as u64,
    )
  }

  fn to_create_options(&self) -> Result<rar5::CreateOptions> {
    let recovery_percent =
      checked_optional_js_integer(self.recovery_percent, "recoveryPercent", 0, 100)?
        .filter(|&value| value != 0)
        .map(|value| value as u8);
    let recovery_volume_count = checked_optional_js_integer(
      self.recovery_volume_count,
      "recoveryVolumeCount",
      0,
      u32::MAX as u64,
    )?
    .filter(|&value| value != 0)
    .map(|value| value as u32);
    let volume_size = checked_optional_js_integer(
      self.volume_size,
      "volumeSize",
      1,
      JS_MAX_SAFE_INTEGER as u64,
    )?;
    let threads =
      checked_optional_js_integer(self.threads, "threads", 1, 64)?.map(|value| value as usize);
    let password = self.password.as_deref().filter(|p| !p.is_empty());
    let (dict_size_log, dict_size_bytes) = match self.dict_size.as_deref() {
      Some(s) => parse_dict_size(s)?,
      None => (None, None),
    };
    Ok(rar5::CreateOptions {
      solid: self.solid.unwrap_or(false),
      quick_open: self.quick_open.unwrap_or(false),
      blake2: self.blake2.unwrap_or(false),
      password: password.map(|p| p.to_string()),
      encrypt_headers: self.encrypt_headers.unwrap_or(false),
      recovery_percent,
      recovery_volumes_percent: None,
      recovery_volume_count,
      volume_size,
      dict_size_log,
      dict_size_bytes,
      save_ctime: self.save_ctime.unwrap_or(false),
      save_atime: self.save_atime.unwrap_or(false),
      save_mtime: true,
      time_precision_seconds: self.time_precision_seconds.unwrap_or(false),
      save_owner: self.save_owner.unwrap_or(false),
      save_streams: self.save_streams.unwrap_or(false),
      threads,
      ..Default::default()
    })
  }
}

impl AppendArchiveOptions {
  fn level(&self) -> Result<u8> {
    checked_js_integer(self.level.unwrap_or(3.0), "level", 0, 5).map(|value| value as u8)
  }
}

impl ExtractArchiveOptions {
  /// `max_dict_size`: None (unset) keeps the WinRAR-style 4 GiB default
  /// cap; Some(0) means unlimited; other values raise/lower the cap.
  fn to_extract_options(&self) -> Result<rar5::ExtractOptions> {
    let max_dict_size = match checked_optional_js_integer(
      self.max_dict_size,
      "maxDictSize",
      0,
      JS_MAX_SAFE_INTEGER as u64,
    )? {
      None => Some(4 * 1024 * 1024 * 1024),
      Some(0) => None,
      Some(value) => Some(value),
    };
    Ok(rar5::ExtractOptions {
      flat_paths: self.flat.unwrap_or(false),
      max_unpacked_bytes: None,
      max_total_unpacked_bytes: None,
      max_dict_size,
      ..Default::default()
    })
  }
}

pub struct CreateArchiveTask {
  opts: CreateArchiveOptions,
  progress: Option<ThreadsafeFunction<ProgressData, ()>>,
  /// Send-safe cancellation flag wired to the JS AbortSignal in the
  /// factory function (the signal itself is `!Send` and cannot live in
  /// the task).
  cancel: Option<Arc<AtomicBool>>,
}

fn plan_entries(entries: &[EntryInput]) -> Result<Vec<PlannedEntry>> {
  let mut planned = Vec::with_capacity(entries.len());
  for e in entries {
    match e.kind.as_str() {
      "file" => {
        let path = e
          .path
          .as_ref()
          .ok_or_else(|| Error::new(Status::InvalidArg, "file entry missing `path`"))?;
        let path = PathBuf::from(path);
        let meta = fs::metadata(&path).map_err(|err| {
          Error::new(
            Status::InvalidArg,
            format!("cannot stat {}: {err}", path.display()),
          )
        })?;
        if !meta.is_file() {
          return Err(Error::new(
            Status::InvalidArg,
            format!("{} is not a file", path.display()),
          ));
        }
        if meta.len() > MAX_FILE_BYTES {
          return Err(Error::new(
            Status::InvalidArg,
            format!(
              "{} is {:.1} GiB, the binding supports inputs up to 4 GiB",
              path.display(),
              meta.len() as f64 / (1 << 30) as f64
            ),
          ));
        }
        planned.push(PlannedEntry {
          kind: "file".into(),
          name: e.name.clone().unwrap_or_else(|| basename(&path)),
          path: Some(path),
          data: None,
        });
      }
      "dir" => {
        let path = e
          .path
          .as_ref()
          .ok_or_else(|| Error::new(Status::InvalidArg, "dir entry missing `path`"))?;
        let path = PathBuf::from(path);
        if !path.is_dir() {
          return Err(Error::new(
            Status::InvalidArg,
            format!("{} is not a directory", path.display()),
          ));
        }
        planned.push(PlannedEntry {
          kind: "dir".into(),
          name: e.name.clone().unwrap_or_else(|| basename(&path)),
          path: Some(path),
          data: None,
        });
      }
      "bytes" => {
        let data = e
          .data
          .as_ref()
          .ok_or_else(|| Error::new(Status::InvalidArg, "bytes entry missing `data`"))?
          .as_ref()
          .to_vec();
        if data.len() as u64 > MAX_FILE_BYTES {
          return Err(Error::new(Status::InvalidArg, "bytes entry exceeds 4 GiB"));
        }
        let name = e
          .name
          .clone()
          .ok_or_else(|| Error::new(Status::InvalidArg, "bytes entry missing `name`"))?;
        planned.push(PlannedEntry {
          kind: "bytes".into(),
          name,
          path: None,
          data: Some(data),
        });
      }
      other => {
        return Err(Error::new(
          Status::InvalidArg,
          format!("unknown entry kind: {other}"),
        ));
      }
    }
  }
  if planned.is_empty() {
    return Err(Error::new(Status::InvalidArg, "no entries to archive"));
  }
  Ok(planned)
}

fn entry_size(e: &PlannedEntry) -> Result<u64> {
  match e.kind.as_str() {
    "bytes" => Ok(e.data.as_ref().map(|d| d.len() as u64).unwrap_or(0)),
    "file" => {
      let meta = fs::metadata(e.path.as_ref().expect("file path")).map_err(|err| {
        Error::new(
          Status::GenericFailure,
          format!("stat {}: {err}", e.path.as_ref().unwrap().display()),
        )
      })?;
      Ok(meta.len())
    }
    // Directory entries write only a header — their children arrive as
    // explicit file entries, so counting the tree again would double-count
    // the progress denominator (and the 32 GiB budget).
    "dir" => Ok(0),
    _ => Ok(0),
  }
}

fn basename(path: &Path) -> String {
  path
    .file_name()
    .map(|n| n.to_string_lossy().into_owned())
    .unwrap_or_default()
}

fn discovered_paths_in_order(paths: Vec<PathBuf>) -> Vec<String> {
  let mut seen = HashSet::with_capacity(paths.len());
  paths
    .into_iter()
    .map(|path| path.to_string_lossy().into_owned())
    .filter(|path| seen.insert(path.clone()))
    .collect()
}

/// Parse a WinRAR-style dictionary size (`-md<size>[k|m|g]`, no unit =
/// MiB) into the two `CreateOptions` fields: values up to 4 GiB must be
/// powers of two (RAR5 dict log), anything above is accepted as-is and
/// selects RAR7 (v70) with an actual byte size.
fn parse_dict_size(s: &str) -> Result<(Option<u8>, Option<u64>)> {
  rar5::parse_dict_size(s)
    .ok_or_else(|| Error::new(Status::InvalidArg, format!("invalid dictionary size: {s}")))
}

/// Build a cancellation flag from the JS `AbortSignal`: when the signal
/// fires, the flag is set and the rar5 operation returns `Cancelled` at
/// its next per-member/per-chunk check point. Returns `None` when no
/// signal was passed (never cancelled). The signal itself is kept alive
/// by the task for the duration of the operation.
fn abort_flag(signal: Option<&AbortSignal>) -> Option<Arc<AtomicBool>> {
  signal.map(|signal| {
    let flag = Arc::new(AtomicBool::new(false));
    let setter = flag.clone();
    signal.on_abort(move || setter.store(true, Ordering::Relaxed));
    flag
  })
}

fn to_napi_error(err: rar5::RarError) -> Error {
  // napi-rs tasks expose N-API Status strings as JS `error.code`. N-API has
  // no dedicated unsupported/security/not-found statuses, so keep a stable
  // semantic split instead of assigning unrelated status names.
  let status = match &err {
    rar5::RarError::Format(_)
    | rar5::RarError::InvalidOption(_)
    | rar5::RarError::Encrypted(_)
    | rar5::RarError::Security(_)
    | rar5::RarError::LimitExceeded { .. }
    | rar5::RarError::MemberNotFound { .. }
    | rar5::RarError::AmbiguousMember { .. }
    | rar5::RarError::StaleEntryId
    | rar5::RarError::WrongPassword => Status::InvalidArg,
    rar5::RarError::Cancelled => Status::Cancelled,
    rar5::RarError::InvalidState(_)
    | rar5::RarError::Crc { .. }
    | rar5::RarError::HashMismatch { .. }
    | rar5::RarError::Unsupported(_)
    | rar5::RarError::ArchiveLocked
    | rar5::RarError::Io(_) => Status::GenericFailure,
    _ => Status::GenericFailure,
  };
  Error::new(status, format!("rar: {err}"))
}

fn build_batch(planned: &[PlannedEntry], level: u8) -> Vec<rar5::BatchEntry<'_>> {
  let mut batch: Vec<rar5::BatchEntry<'_>> = Vec::with_capacity(planned.len());
  for e in planned {
    match e.kind.as_str() {
      "file" => {
        let path = e.path.as_ref().expect("file path");
        batch.push(rar5::BatchEntry::File {
          path,
          name: if e.name.is_empty() {
            None
          } else {
            Some(&e.name)
          },
          level,
        });
      }
      "dir" => {
        let path = e.path.as_ref().expect("dir path");
        batch.push(rar5::BatchEntry::Directory {
          path,
          name: Some(&e.name),
        });
      }
      "bytes" => {
        let data = e.data.as_ref().expect("bytes data");
        batch.push(rar5::BatchEntry::Bytes {
          name: &e.name,
          data,
          level,
        });
      }
      _ => {}
    }
  }
  batch
}

fn write_batch(
  archive: &mut rar5::RarArchive,
  batch: &[rar5::BatchEntry<'_>],
  total_bytes: u64,
  progress: Option<ThreadsafeFunction<ProgressData, ()>>,
) -> Result<()> {
  let terminal = progress.map(Arc::new);
  if let Some(tsfn) = terminal.as_ref() {
    let cb_tsfn = tsfn.clone();
    // rar-rs already aggregates every member's deltas (sequential and
    // parallel-wave alike) into one monotonic, operation-global stream, so
    // this side just forwards `(committed, total)`.
    archive.set_progress_callback(Some(Box::new(move |done, total| {
      let _ = cb_tsfn.call(
        Ok(ProgressData {
          done: done.min(total) as f64,
          total: total as f64,
        }),
        ThreadsafeFunctionCallMode::NonBlocking,
      );
    })));
    archive.add_batch(batch).map_err(to_napi_error)?;
  } else {
    archive.add_batch(batch).map_err(to_napi_error)?;
  }

  archive.close().map_err(to_napi_error)?;

  if let Some(tsfn) = terminal {
    // Terminal 100% event after the archive is fully closed (including
    // recovery records and volume finalization). Delivery is asynchronous,
    // so the JS side may still observe it a tick after the promise
    // resolves.
    let _ = tsfn.call(
      Ok(ProgressData {
        done: total_bytes as f64,
        total: total_bytes as f64,
      }),
      ThreadsafeFunctionCallMode::Blocking,
    );
  }
  Ok(())
}

#[napi]
impl Task for CreateArchiveTask {
  type Output = CreateResult;
  type JsValue = CreateResult;

  fn compute(&mut self) -> Result<Self::Output> {
    let planned = plan_entries(&self.opts.entries)?;
    let total_bytes: u64 = planned.iter().try_fold(0u64, |acc, e| {
      let s = entry_size(e)?;
      let next = acc.saturating_add(s);
      if next > MAX_TOTAL_BYTES {
        return Err(Error::new(
          Status::InvalidArg,
          "total input size exceeds 32 GiB limit",
        ));
      }
      Ok(next)
    })?;

    if let Some(limit) = self.opts.max_total_bytes()?
      && total_bytes > limit
    {
      return Err(Error::new(
        Status::InvalidArg,
        format!(
          "total input size {:.1} MiB exceeds limit {:.1} MiB",
          total_bytes as f64 / 1048576.0,
          limit as f64 / 1048576.0
        ),
      ));
    }
    let level = self.opts.level()?;
    let batch = build_batch(&planned, level);
    let out = Path::new(&self.opts.out_path);
    if let Some(parent) = out.parent() {
      fs::create_dir_all(parent)
        .map_err(|err| Error::new(Status::GenericFailure, format!("mkdir: {err}")))?;
    }

    // Per-archive thread count (scoped, so concurrent creates with
    // different `threads` never interfere); extraction stays on the global
    // default pool.
    let create_opts = self.opts.to_create_options()?;
    let mut archive =
      rar5::RarArchive::create_with_options(out, create_opts).map_err(to_napi_error)?;
    archive.set_cancel_flag(self.cancel.take());

    write_batch(&mut archive, &batch, total_bytes, self.progress.take())?;
    drop(archive);

    let files = discovered_paths_in_order(rar5::discover_volumes(out));

    Ok(CreateResult { files })
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// Repair a damaged archive using its inline recovery record.
///
/// Reads `input_path`, rebuilds any damaged data shards from the `{RB}`
/// parity shards and writes the repaired archive to `output_path`.
/// Streaming: memory stays bounded (only the recovery data + damaged
/// shards are held), so archives far larger than RAM can be repaired.
/// Returns `false` when the archive was already intact (no output file
/// is written in that case, like `rar r`'s "All OK").
#[napi(ts_return_type = "Promise<boolean>")]
pub fn repair_archive(
  input_path: String,
  output_path: String,
  on_progress: Option<ThreadsafeFunction<ProgressData, ()>>,
  signal: Option<AbortSignal>,
) -> AsyncTask<RepairArchiveTask> {
  AsyncTask::with_optional_signal(
    RepairArchiveTask {
      input_path,
      output_path,
      progress: on_progress,
      cancel: abort_flag(signal.as_ref()),
    },
    signal,
  )
}

pub struct RepairArchiveTask {
  input_path: String,
  output_path: String,
  progress: Option<ThreadsafeFunction<ProgressData, ()>>,
  cancel: Option<Arc<AtomicBool>>,
}

#[napi]
impl Task for RepairArchiveTask {
  type Output = bool;
  type JsValue = bool;

  fn compute(&mut self) -> Result<Self::Output> {
    let progress = self.progress.take();
    let mut report = |done: u64, total: u64| {
      if let Some(tsfn) = progress.as_ref() {
        let _ = tsfn.call(
          Ok(ProgressData {
            done: done.min(total) as f64,
            total: total as f64,
          }),
          ThreadsafeFunctionCallMode::NonBlocking,
        );
      }
    };
    rar5::repair_archive_path_with(
      Path::new(&self.input_path),
      Path::new(&self.output_path),
      self.cancel.as_deref(),
      Some(&mut report),
    )
    .map_err(to_napi_error)
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// Rebuild missing volumes of a multi-volume set from its `.rev` recovery
/// volumes (like WinRAR `rc`).
///
/// `first_volume` is the path of `name.part1.rar`; every missing volume is
/// reconstructed from the `.rev` parity volumes into the same directory.
/// Returns the paths of all volumes produced.
#[napi(ts_return_type = "Promise<Array<string>>")]
pub fn rebuild_missing_volumes(
  first_volume: String,
  on_progress: Option<ThreadsafeFunction<ProgressData, ()>>,
  signal: Option<AbortSignal>,
) -> AsyncTask<RebuildVolumesTask> {
  AsyncTask::with_optional_signal(
    RebuildVolumesTask {
      first_volume,
      progress: on_progress,
      cancel: abort_flag(signal.as_ref()),
    },
    signal,
  )
}

pub struct RebuildVolumesTask {
  first_volume: String,
  progress: Option<ThreadsafeFunction<ProgressData, ()>>,
  cancel: Option<Arc<AtomicBool>>,
}

#[napi]
impl Task for RebuildVolumesTask {
  type Output = Vec<String>;
  type JsValue = Vec<String>;

  fn compute(&mut self) -> Result<Self::Output> {
    let progress = self.progress.take();
    let mut report = |done: u64, total: u64| {
      if let Some(tsfn) = progress.as_ref() {
        let _ = tsfn.call(
          Ok(ProgressData {
            done: done.min(total) as f64,
            total: total as f64,
          }),
          ThreadsafeFunctionCallMode::NonBlocking,
        );
      }
    };
    let paths = rar5::rebuild_missing_volumes_with(
      Path::new(&self.first_volume),
      self.cancel.as_deref(),
      Some(&mut report),
    )
    .map_err(to_napi_error)?;
    Ok(
      paths
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect(),
    )
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

#[napi(object)]
pub struct AppendArchiveOptions {
  /// Existing archive to append to (single-volume only).
  pub archive_path: String,
  pub entries: Vec<EntryInput>,
  /// Compression level 0..=5 (default 3).
  pub level: Option<f64>,
  /// Password of the existing archive (needed when its content is
  /// encrypted so the solid chain can be extended).
  pub password: Option<String>,
  /// Dictionary size for the added members (like `-md`; see
  /// [`CreateArchiveOptions::dict_size`]).
  pub dict_size: Option<String>,
}

pub struct AppendArchiveTask {
  opts: AppendArchiveOptions,
  progress: Option<ThreadsafeFunction<ProgressData, ()>>,
  cancel: Option<Arc<AtomicBool>>,
}

#[napi]
impl Task for AppendArchiveTask {
  type Output = CreateResult;
  type JsValue = CreateResult;

  fn compute(&mut self) -> Result<Self::Output> {
    let planned = plan_entries(&self.opts.entries)?;
    let total_bytes: u64 = planned.iter().try_fold(0u64, |acc, e| {
      let s = entry_size(e)?;
      let next = acc.saturating_add(s);
      if next > MAX_TOTAL_BYTES {
        return Err(Error::new(
          Status::InvalidArg,
          "total input size exceeds 32 GiB limit",
        ));
      }
      Ok(next)
    })?;

    let level = self.opts.level()?;
    let batch = build_batch(&planned, level);
    let (dict_size_log, dict_size_bytes) = match self.opts.dict_size.as_deref() {
      Some(s) => parse_dict_size(s)?,
      None => (None, None),
    };
    let mut archive = match self.opts.password.as_deref() {
      Some(pw) if !pw.is_empty() => {
        rar5::RarArchive::open_append_with_password(&self.opts.archive_path, pw)
          .map_err(to_napi_error)?
      }
      _ => rar5::RarArchive::open_append(&self.opts.archive_path).map_err(to_napi_error)?,
    };
    archive.set_cancel_flag(self.cancel.take());
    archive
      .set_dictionary(dict_size_log, dict_size_bytes)
      .map_err(to_napi_error)?;

    write_batch(&mut archive, &batch, total_bytes, self.progress.take())?;
    drop(archive);

    let files =
      discovered_paths_in_order(rar5::discover_volumes(Path::new(&self.opts.archive_path)));

    Ok(CreateResult { files })
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// Append entries to an existing archive without rebuilding it.
///
/// Existing members are preserved verbatim (never recompressed); only the
/// trailing quick-open/recovery/end blocks are truncated and rewritten.
/// Recovery records are regenerated over the whole archive. Multi-volume
/// archives are not supported (matching the official `rar` CLI).
#[napi]
pub fn append_entries(
  opts: AppendArchiveOptions,
  on_progress: Option<ThreadsafeFunction<ProgressData, ()>>,
  signal: Option<AbortSignal>,
) -> AsyncTask<AppendArchiveTask> {
  AsyncTask::with_optional_signal(
    AppendArchiveTask {
      opts,
      progress: on_progress,
      cancel: abort_flag(signal.as_ref()),
    },
    signal,
  )
}

/// Delete members from an archive without rebuilding it.
///
/// Non-solid archives are rewritten surgically: kept members are copied
/// verbatim, never recompressed (like the official `rar d`). Solid chains
/// that lose a member are recompressed from the chain start only. For
/// multi-volume archives, kept payloads are re-split at the volume size
/// limit and `.rev` recovery volumes are regenerated.
///
/// Fails when any requested name is not present, or when the archive is
/// locked. Returns the number of deleted members.
#[napi(ts_return_type = "Promise<number>")]
pub fn delete_entries(
  archive_path: String,
  names: Vec<String>,
  password: Option<String>,
  on_progress: Option<ThreadsafeFunction<ProgressData, ()>>,
  signal: Option<AbortSignal>,
) -> AsyncTask<DeleteEntriesTask> {
  AsyncTask::with_optional_signal(
    DeleteEntriesTask {
      archive_path,
      names,
      password,
      progress: on_progress,
      cancel: abort_flag(signal.as_ref()),
    },
    signal,
  )
}

pub struct DeleteEntriesTask {
  archive_path: String,
  names: Vec<String>,
  password: Option<String>,
  progress: Option<ThreadsafeFunction<ProgressData, ()>>,
  cancel: Option<Arc<AtomicBool>>,
}

#[napi]
impl Task for DeleteEntriesTask {
  type Output = u32;
  type JsValue = u32;

  fn compute(&mut self) -> Result<Self::Output> {
    let mut archive = match self.password.as_deref() {
      Some(pw) if !pw.is_empty() => {
        rar5::RarArchive::open_with_password(&self.archive_path, pw).map_err(to_napi_error)?
      }
      _ => rar5::RarArchive::open(&self.archive_path).map_err(to_napi_error)?,
    };
    archive.set_cancel_flag(self.cancel.take());
    let progress = self.progress.take();
    let refs: Vec<&str> = self.names.iter().map(|s| s.as_str()).collect();
    let count = archive
      .delete_with_progress(
        &refs,
        Some(Box::new(move |done: u64, total: u64| {
          if let Some(tsfn) = progress.as_ref() {
            let _ = tsfn.call(
              Ok(ProgressData {
                done: done.min(total) as f64,
                total: total as f64,
              }),
              ThreadsafeFunctionCallMode::NonBlocking,
            );
          }
        })),
      )
      .map_err(to_napi_error)?;
    Ok(count as u32)
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// Read one member's uncompressed content into memory (like previewing a
/// file inside the archive). Bounded by the library's default 4 GiB
/// per-member read limit; use `extractArchive` for arbitrarily large
/// members.
#[napi(ts_return_type = "Promise<Buffer>")]
pub fn read_member(
  archive_path: String,
  name: String,
  password: Option<String>,
) -> AsyncTask<ReadMemberTask> {
  AsyncTask::new(ReadMemberTask {
    archive_path,
    name,
    password,
  })
}

pub struct ReadMemberTask {
  archive_path: String,
  name: String,
  password: Option<String>,
}

#[napi]
impl Task for ReadMemberTask {
  type Output = Vec<u8>;
  type JsValue = Buffer;

  fn compute(&mut self) -> Result<Self::Output> {
    let mut archive = match self.password.as_deref() {
      Some(pw) if !pw.is_empty() => {
        rar5::RarArchive::open_with_password(&self.archive_path, pw).map_err(to_napi_error)?
      }
      _ => rar5::RarArchive::open(&self.archive_path).map_err(to_napi_error)?,
    };
    archive.read(&self.name).map_err(to_napi_error)
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output.into())
  }
}

/// Test the integrity of every member (like `rar t`); returns the
/// `(checked, failed)` counts. A damaged member is reported, never
/// thrown.
#[napi(ts_return_type = "Promise<Array<number>>")]
pub fn test_archive(archive_path: String, password: Option<String>) -> AsyncTask<TestArchiveTask> {
  AsyncTask::new(TestArchiveTask {
    archive_path,
    password,
  })
}

pub struct TestArchiveTask {
  archive_path: String,
  password: Option<String>,
}

#[napi]
impl Task for TestArchiveTask {
  type Output = Vec<u32>;
  type JsValue = Vec<u32>;

  fn compute(&mut self) -> Result<Self::Output> {
    let mut archive = match self.password.as_deref() {
      Some(pw) if !pw.is_empty() => {
        rar5::RarArchive::open_with_password(&self.archive_path, pw).map_err(to_napi_error)?
      }
      _ => rar5::RarArchive::open(&self.archive_path).map_err(to_napi_error)?,
    };
    let (checked, failed) = archive.test().map_err(to_napi_error)?;
    Ok(vec![checked as u32, failed as u32])
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// List the member names of an archive without blocking the JS thread.
#[napi(ts_return_type = "Promise<Array<string>>")]
pub fn list_entries(archive_path: String, password: Option<String>) -> AsyncTask<ListEntriesTask> {
  AsyncTask::new(ListEntriesTask {
    archive_path,
    password,
  })
}

pub struct ListEntriesTask {
  archive_path: String,
  password: Option<String>,
}

#[napi]
impl Task for ListEntriesTask {
  type Output = Vec<String>;
  type JsValue = Vec<String>;

  fn compute(&mut self) -> Result<Self::Output> {
    let archive = match self.password.as_deref() {
      Some(pw) if !pw.is_empty() => {
        rar5::RarArchive::open_with_password(&self.archive_path, pw).map_err(to_napi_error)?
      }
      _ => rar5::RarArchive::open(&self.archive_path).map_err(to_napi_error)?,
    };
    Ok(
      archive
        .namelist()
        .into_iter()
        .map(|name| name.to_string())
        .collect(),
    )
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// Create an archive from the given entries.
#[napi]
pub fn create_archive(
  opts: CreateArchiveOptions,
  on_progress: Option<ThreadsafeFunction<ProgressData, ()>>,
  signal: Option<AbortSignal>,
) -> AsyncTask<CreateArchiveTask> {
  AsyncTask::with_optional_signal(
    CreateArchiveTask {
      opts,
      progress: on_progress,
      cancel: abort_flag(signal.as_ref()),
    },
    signal,
  )
}

/// One member's details for [`list_entries_detailed`].
#[napi(object)]
pub struct EntryInfo {
  pub name: String,
  /// Uncompressed size in bytes (JS number; exact up to 2^53).
  pub size: f64,
  /// On-disk (packed) size in bytes.
  pub packed_size: f64,
  /// Compression method: 0 = store, 1..=5 (level).
  pub method: u8,
  pub is_dir: bool,
  /// Modification time as Unix seconds (0 when unknown).
  pub mtime: f64,
}

/// List archive members with sizes and methods without blocking the JS thread.
#[napi(ts_return_type = "Promise<Array<EntryInfo>>")]
pub fn list_entries_detailed(
  archive_path: String,
  password: Option<String>,
) -> AsyncTask<ListEntriesDetailedTask> {
  AsyncTask::new(ListEntriesDetailedTask {
    archive_path,
    password,
    quick: false,
  })
}

/// List the members of an archive through its quick-open record — the fast
/// path for archives created with `quickOpen`. Archives without a usable
/// record transparently fall back to a full scan on a worker thread.
#[napi(ts_return_type = "Promise<Array<EntryInfo>>")]
pub fn list_entries_quick(
  archive_path: String,
  password: Option<String>,
) -> AsyncTask<ListEntriesDetailedTask> {
  AsyncTask::new(ListEntriesDetailedTask {
    archive_path,
    password,
    quick: true,
  })
}

pub struct ListEntriesDetailedTask {
  archive_path: String,
  password: Option<String>,
  quick: bool,
}

#[napi]
impl Task for ListEntriesDetailedTask {
  type Output = Vec<EntryInfo>;
  type JsValue = Vec<EntryInfo>;

  fn compute(&mut self) -> Result<Self::Output> {
    let archive = match (self.quick, self.password.as_deref()) {
      (true, Some(pw)) if !pw.is_empty() => {
        rar5::RarArchive::open_quick_with_password(&self.archive_path, pw).map_err(to_napi_error)?
      }
      (true, _) => rar5::RarArchive::open_quick(&self.archive_path).map_err(to_napi_error)?,
      (false, Some(pw)) if !pw.is_empty() => {
        rar5::RarArchive::open_with_password(&self.archive_path, pw).map_err(to_napi_error)?
      }
      (false, _) => rar5::RarArchive::open(&self.archive_path).map_err(to_napi_error)?,
    };
    Ok(entry_infos(&archive))
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

fn entry_infos(archive: &rar5::RarArchive) -> Vec<EntryInfo> {
  archive
    .list()
    .iter()
    .map(|e| EntryInfo {
      name: e.name().to_string(),
      size: e.size() as f64,
      packed_size: e.compressed_size() as f64,
      method: e.method(),
      is_dir: e.is_dir(),
      mtime: e.mtime() as f64,
    })
    .collect()
}

/// Options for [`extract_archive`].
#[napi(object)]
pub struct ExtractArchiveOptions {
  /// Destination directory (created when missing).
  pub dest_path: String,
  /// Password for encrypted archives.
  pub password: Option<String>,
  /// Extract members flat (basename only, no directory tree).
  pub flat: Option<bool>,
  /// Maximum dictionary size in bytes accepted when decoding a member.
  /// WinRAR-compatible default: 4 GiB (RAR7 v70 members with larger
  /// dictionaries are refused). Pass 0 for no limit.
  pub max_dict_size: Option<f64>,
}

pub struct ExtractArchiveTask {
  archive_path: String,
  opts: ExtractArchiveOptions,
  cancel: Option<Arc<AtomicBool>>,
}

#[napi]
impl Task for ExtractArchiveTask {
  type Output = ();
  type JsValue = ();

  fn compute(&mut self) -> Result<Self::Output> {
    let mut archive = match self.opts.password.as_deref() {
      Some(pw) if !pw.is_empty() => {
        rar5::RarArchive::open_with_password(&self.archive_path, pw).map_err(to_napi_error)?
      }
      _ => rar5::RarArchive::open(&self.archive_path).map_err(to_napi_error)?,
    };
    archive.set_cancel_flag(self.cancel.take());
    let dest = Path::new(&self.opts.dest_path);
    fs::create_dir_all(dest)
      .map_err(|err| Error::new(Status::GenericFailure, format!("mkdir: {err}")))?;
    archive
      .extract_all_with_options(dest, self.opts.to_extract_options()?)
      .map_err(to_napi_error)?;
    Ok(())
  }

  fn resolve(&mut self, _env: Env, _output: Self::Output) -> Result<Self::JsValue> {
    Ok(())
  }
}

/// Extract an archive into a directory (fully streaming: no per-member or
/// total size limits, so arbitrarily large members work).
#[napi]
pub fn extract_archive(
  archive_path: String,
  opts: ExtractArchiveOptions,
  signal: Option<AbortSignal>,
) -> AsyncTask<ExtractArchiveTask> {
  AsyncTask::with_optional_signal(
    ExtractArchiveTask {
      archive_path,
      opts,
      cancel: abort_flag(signal.as_ref()),
    },
    signal,
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn js_integer_validation_rejects_lossy_or_out_of_range_values() {
    for value in [
      f64::NAN,
      f64::INFINITY,
      -1.0,
      1.5,
      JS_MAX_SAFE_INTEGER + 1.0,
    ] {
      let err = checked_js_integer(value, "value", 0, JS_MAX_SAFE_INTEGER as u64).unwrap_err();
      assert_eq!(err.status, Status::InvalidArg);
    }
    assert_eq!(
      checked_js_integer(JS_MAX_SAFE_INTEGER, "value", 0, JS_MAX_SAFE_INTEGER as u64).unwrap(),
      JS_MAX_SAFE_INTEGER as u64
    );
  }

  #[test]
  fn core_error_variants_have_stable_napi_statuses() {
    let invalid_arguments = [
      rar5::RarError::Format("bad header".into()),
      rar5::RarError::InvalidOption("bad option".into()),
      rar5::RarError::Encrypted("password required".into()),
      rar5::RarError::Security("unsafe path".into()),
      rar5::RarError::LimitExceeded {
        limit: 1,
        context: "test".into(),
      },
      rar5::RarError::MemberNotFound { name: "x".into() },
      rar5::RarError::AmbiguousMember {
        name: "x".into(),
        matches: 2,
      },
      rar5::RarError::StaleEntryId,
      rar5::RarError::WrongPassword,
    ];
    for err in invalid_arguments {
      assert_eq!(to_napi_error(err).status, Status::InvalidArg);
    }

    let operation_failures = [
      rar5::RarError::InvalidState("read mode".into()),
      rar5::RarError::Unsupported("feature".into()),
      rar5::RarError::ArchiveLocked,
      rar5::RarError::Crc {
        expected: 1,
        actual: 2,
        context: "member".into(),
      },
      rar5::RarError::HashMismatch {
        expected: [1; 32],
        actual: [2; 32],
        context: "member".into(),
      },
      rar5::RarError::Io(std::io::Error::other("disk")),
    ];
    for err in operation_failures {
      assert_eq!(to_napi_error(err).status, Status::GenericFailure);
    }

    assert_eq!(
      to_napi_error(rar5::RarError::Cancelled).status,
      Status::Cancelled
    );
  }

  #[test]
  fn discovered_paths_keep_natural_volume_order_and_deduplicate() {
    let mut paths = (1..=12)
      .map(|part| PathBuf::from(format!("archive.part{part}.rar")))
      .collect::<Vec<_>>();
    paths.push(PathBuf::from("archive.part10.rar"));

    let result = discovered_paths_in_order(paths);
    assert_eq!(result.len(), 12);
    assert_eq!(result[1], "archive.part2.rar");
    assert_eq!(result[9], "archive.part10.rar");
    assert_eq!(result[11], "archive.part12.rar");
  }
}
