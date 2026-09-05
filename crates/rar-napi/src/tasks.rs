//! Async task glue: napi `Task` implementations and the factory functions
//! that build them, plus the entry-planning and progress helpers.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;

use crate::error::to_napi_error;
use crate::options::parse_dict_size;
use crate::{
  AppendArchiveOptions, CreateArchiveOptions, CreateResult, EntryInfo, EntryInput,
  ExtractArchiveOptions, ProgressData,
};

/// Maximum per-file size read into memory by the rar5 library (4 GiB).
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Maximum total input size across all entries (32 GiB).
const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024 * 1024;

struct PlannedEntry {
  kind: String,
  path: Option<PathBuf>,
  name: String,
  data: Option<Vec<u8>>,
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

fn write_entries(planned: &[PlannedEntry], level: u8) -> Result<Vec<rar5::WriteEntry<'_>>> {
  let compression = rar5::CompressionLevel::try_from(level)
    .map_err(|err| Error::new(Status::InvalidArg, format!("level: {err}")))?;
  let options = rar5::EntryWriteOptions::new().compression_level(compression);
  let mut batch: Vec<rar5::WriteEntry<'_>> = Vec::with_capacity(planned.len());
  for e in planned {
    match e.kind.as_str() {
      "file" => {
        let path = e.path.as_ref().expect("file path");
        batch.push(rar5::WriteEntry::File {
          path,
          name: if e.name.is_empty() {
            None
          } else {
            Some(&e.name)
          },
          options,
        });
      }
      "dir" => {
        let path = e.path.as_ref().expect("dir path");
        batch.push(rar5::WriteEntry::Directory {
          path,
          name: Some(&e.name),
        });
      }
      "bytes" => {
        let data = e.data.as_ref().expect("bytes data");
        batch.push(rar5::WriteEntry::Bytes {
          name: &e.name,
          data,
          options,
        });
      }
      _ => {}
    }
  }
  Ok(batch)
}

/// Add the members through the typed writer and commit with `finish()`,
/// forwarding `(committed, total)` progress and delivering a terminal 100%
/// event only after the archive is fully closed (including recovery
/// records and volume finalization). Returns the committed volume paths
/// from the write report — no filesystem rediscovery needed.
fn write_transaction(
  mut writer: rar5::ArchiveWriter,
  batch: &[rar5::WriteEntry<'_>],
  total_bytes: u64,
  progress: Option<ThreadsafeFunction<ProgressData, ()>>,
) -> Result<rar5::WriteReport> {
  let terminal = progress.map(Arc::new);
  if let Some(tsfn) = terminal.as_ref() {
    let cb_tsfn = tsfn.clone();
    // rar-rs already aggregates every member's deltas (sequential and
    // parallel-wave alike) into one monotonic, operation-global stream, so
    // this side just forwards `(committed, total)`.
    writer
      .set_progress_callback(Some(Box::new(move |done, total| {
        let _ = cb_tsfn.call(
          Ok(ProgressData {
            done: done.min(total) as f64,
            total: total as f64,
          }),
          ThreadsafeFunctionCallMode::NonBlocking,
        );
      })))
      .map_err(to_napi_error)?;
  }
  writer.add_batch(batch).map_err(to_napi_error)?;
  let report = writer.finish().map_err(to_napi_error)?;
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
  Ok(report)
}

fn report_files(report: rar5::WriteReport) -> Vec<String> {
  report
    .into_volume_paths()
    .into_iter()
    .map(|path| path.to_string_lossy().into_owned())
    .collect()
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
    let batch = write_entries(&planned, level)?;
    let out = Path::new(&self.opts.out_path);
    if let Some(parent) = out.parent() {
      fs::create_dir_all(parent)
        .map_err(|err| Error::new(Status::GenericFailure, format!("mkdir: {err}")))?;
    }

    // The typed writer stages everything and commits only in `finish()`:
    // a failed or cancelled add aborts the transaction and leaves nothing
    // at the output path.
    let writer_opts = self.opts.to_writer_options()?;
    let mut writer = rar5::ArchiveWriter::create_with(out, writer_opts).map_err(to_napi_error)?;
    writer
      .set_cancel_flag(self.cancel.take())
      .map_err(to_napi_error)?;

    let report = write_transaction(writer, &batch, total_bytes, self.progress.take())?;

    Ok(CreateResult {
      files: report_files(report),
    })
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
    let batch = write_entries(&planned, level)?;
    let mut append_opts = rar5::AppendOptions::new();
    if let Some(pw) = self.opts.password.as_deref().filter(|pw| !pw.is_empty()) {
      append_opts = append_opts.password(pw.to_string());
    }
    if let Some(spec) = self.opts.dict_size.as_deref() {
      let (dict_log, dict_bytes) = parse_dict_size(spec)?;
      let bytes = dict_bytes
        .or_else(|| dict_log.map(|log| (128u64 * 1024) << log))
        .expect("dictionary parse returns a log or a byte count");
      append_opts =
        append_opts.dictionary_size(rar5::DictionarySize::try_from(bytes).map_err(|err| {
          Error::new(
            Status::InvalidArg,
            format!("invalid dictionary size: {err}"),
          )
        })?);
    }
    let mut writer = rar5::ArchiveWriter::append_with(&self.opts.archive_path, append_opts)
      .map_err(to_napi_error)?;
    writer
      .set_cancel_flag(self.cancel.take())
      .map_err(to_napi_error)?;

    let report = write_transaction(writer, &batch, total_bytes, self.progress.take())?;

    Ok(CreateResult {
      files: report_files(report),
    })
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
    let mut editor = match self.password.as_deref() {
      Some(pw) if !pw.is_empty() => {
        rar5::ArchiveEditor::open_with_password(&self.archive_path, pw).map_err(to_napi_error)?
      }
      _ => rar5::ArchiveEditor::open(&self.archive_path).map_err(to_napi_error)?,
    };
    editor.set_cancel_flag(self.cancel.take());
    // Preserve the legacy name semantics: every name deletes the first
    // matching member not already selected, so repeated names delete
    // successive duplicates, and a missing name fails the whole plan
    // before any rewrite starts.
    let mut ids: Vec<rar5::EntryId> = Vec::with_capacity(self.names.len());
    for name in &self.names {
      let id = editor
        .entries_named(name)
        .map(|entry| entry.id())
        .find(|id| !ids.contains(id))
        .ok_or_else(|| to_napi_error(rar5::RarError::MemberNotFound { name: name.clone() }))?;
      ids.push(id);
    }
    let progress = self.progress.take();
    let count = editor
      .delete_entries_with_progress(
        &ids,
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
    let mut options = rar5::OpenOptions::new();
    if let Some(pw) = self.password.as_deref().filter(|pw| !pw.is_empty()) {
      options = options.password(pw);
    }
    let archive =
      rar5::ArchiveReader::open_with(&self.archive_path, options).map_err(to_napi_error)?;
    Ok(
      archive
        .entries()
        .map(|entry| entry.name().to_string())
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
    let mut options = rar5::OpenOptions::new();
    if let Some(pw) = self.password.as_deref().filter(|pw| !pw.is_empty()) {
      options = options.password(pw);
    }
    // `ArchiveReader` transparently prefers the quick-open record and falls
    // back to a full scan, so the explicit `quick` selector is subsumed.
    let _ = self.quick;
    let archive =
      rar5::ArchiveReader::open_with(&self.archive_path, options).map_err(to_napi_error)?;
    Ok(entry_infos(&archive))
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

fn entry_infos(archive: &rar5::ArchiveReader) -> Vec<EntryInfo> {
  archive
    .entries()
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
    let mut options = rar5::OpenOptions::new();
    if let Some(pw) = self.password.as_deref().filter(|pw| !pw.is_empty()) {
      options = options.password(pw);
    }
    let mut archive =
      rar5::ArchiveReader::open_with(&self.archive_path, options).map_err(to_napi_error)?;
    let id = archive.unique_entry(&self.name).map_err(to_napi_error)?;
    archive.read_entry(id).map_err(to_napi_error)
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output.into())
  }
}
