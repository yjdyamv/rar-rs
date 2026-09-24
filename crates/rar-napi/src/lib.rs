//! RAR archive operations for the Smart Archive VS Code extension.
//!
//! Wraps the pure-Rust `rar-rs` crate behind a napi-rs API for creating, reading,
//! testing, listing, extracting, repairing, and modifying RAR archives.
//!
//! Deliberate omissions from the writer options: the per-archive compression
//! filter policy (`-mc` / `WriterOptions::filters`) is not exposed; automatic
//! filter selection stays in effect for every create/append call. Mark of the
//! Web propagation (`-om`) and queueing the archive comment before creation
//! (`-z` reads stdin in the CLI) are also not exposed — callers can set the
//! comment with [`set_comment`] once the archive exists.
use napi::bindgen_prelude::*;
use napi_derive::napi;

mod error;
mod guard;
mod options;
mod tasks;

pub use tasks::{
  append_entries, create_archive, delete_entries, extract_archive, extract_member, list_entries,
  list_entries_detailed, list_entries_quick, lock_archive, read_member, rebuild_missing_volumes,
  rename_entries, repair_archive, set_comment, set_member_comment, set_recovery, test_archive,
};

#[napi(object)]
pub struct EntryInput {
  /// "file" | "dir" | "bytes" | "redirect"
  pub kind: String,
  /// Filesystem path for "file" and "dir" entries.
  pub path: Option<String>,
  /// Archive entry name. For "file"/"dir" defaults to the basename, for
  /// "bytes" it is required, for "redirect" it is the archive member name.
  pub name: Option<String>,
  /// Byte payload for "bytes" entries.
  pub data: Option<Buffer>,
  /// Redirect type 1..=5 for "redirect" entries: 1 = Unix symlink,
  /// 2 = Windows symlink, 3 = Windows junction, 4 = hardlink, 5 = file
  /// copy. The entry carries no data.
  pub redir_type: Option<f64>,
  /// Redirect target archive member name for "redirect" entries.
  pub target: Option<String>,
}
#[napi(object)]
pub struct CreateArchiveOptions {
  pub out_path: String,
  pub entries: Vec<EntryInput>,
  /// Compression level 0..=5 (default 3).
  pub level: Option<f64>,
  /// Optional AES-256 password (file-level encryption).
  pub password: Option<String>,
  /// Also encrypt the archive structure (file names). Requires `password`.
  pub encrypt_headers: Option<bool>,
  /// Add a WinRAR-compatible inline recovery record protecting this percent
  /// (0-100) of the archive. With `volume_size`, every data volume carries
  /// its own record protecting that volume (WinRAR's `-rr` with `-v`), and
  /// `.rev` recovery volumes may be combined with it.
  pub recovery_percent: Option<f64>,
  /// Add an inline recovery record with exactly this many parity sectors
  /// (the legacy RAR4 record's native unit, like a bare `-rr<N>`).
  /// Mutually exclusive with `recovery_percent`; a RAR5 record is sized by
  /// percent only and rejects it.
  pub recovery_sectors: Option<f64>,
  /// Create this many `.rev` recovery volumes (WinRAR `-rv`); auto-capped
  /// at the actual data volume count. Requires `volume_size`.
  pub recovery_volume_count: Option<f64>,
  /// Create recovery volumes as a percentage of the data-volume count
  /// (WinRAR `-rv<N%>`); requires `volume_size` and cannot be combined with
  /// `recovery_volume_count`.
  pub recovery_volumes_percent: Option<f64>,
  /// Volume size in bytes; when set, produces multi-volume archives
  /// (`name.part1.rar`, ... for RAR5/RAR7; `name.rar`/`name.r00`, ... for
  /// the legacy and RAR 1.3/1.4 formats).
  pub volume_size: Option<f64>,
  /// Reject the operation when the summed input size exceeds this.
  pub max_total_bytes: Option<f64>,
  /// Dictionary size (like WinRAR `-md<size>[k|m|g]`, no unit = MiB).
  /// Values up to 4 GiB must be powers of two (128 KiB .. 4 GiB) under
  /// `format: 'rar5'`; values above 4 GiB, and any size under
  /// `format: 'rar7'`, are accepted as byte counts (including
  /// non-power-of-two values like `6m`) and produce RAR7 (v70) members.
  pub dict_size: Option<String>,
  /// Create a solid archive (better ratio, slower random access).
  pub solid: Option<bool>,
  /// How the solid chain splits (WinRAR `-sd`/`-sv`/`-se`): "continuous"
  /// (default), "volume" (reset per volume) or "extension" (reset when the
  /// member extension changes). Implies `solid`.
  pub solid_reset: Option<String>,
  /// Add a quick-open record for fast member listing.
  pub quick_open: Option<bool>,
  /// Write BLAKE2sp hash records for every member (like WinRAR `-htb`).
  pub blake2: Option<bool>,
  /// Compression threads (1..=64; 0 = automatic, the library default).
  pub threads: Option<f64>,
  /// Save the creation time (Windows) / ctime (Unix) in the FILE_TIME
  /// extra record (like WinRAR `-tsc`).
  pub save_ctime: Option<bool>,
  /// Save the last access time (like WinRAR `-tsa`).
  pub save_atime: Option<bool>,
  /// Save the modification time (default `true`; `false` omits it for
  /// RAR5/RAR7, which have no time field to omit once written). RAR 1.3–4.x
  /// fixed headers always carry DOS local time, so the switch is ignored
  /// there, like WinRAR.
  pub save_mtime: Option<bool>,
  /// Store timestamps at 1-second precision (like WinRAR `-ts...1`).
  pub time_precision_seconds: Option<bool>,
  /// Save the owner and group (numeric ids) on Unix (like WinRAR `-ow`).
  pub save_owner: Option<bool>,
  /// Save NTFS alternate data streams (like WinRAR `-os`; Windows only).
  pub save_streams: Option<bool>,
  /// Request a specific archive format: "rar5" (default), "rar7", "rar4",
  /// "rar2", "rar15" or "rar13". "rar5" auto-promotes an individual member to
  /// RAR7 (v70) when its effective dictionary exceeds 4 GiB. "rar7" forces
  /// v70 members at any dictionary (32 MiB by default). "rar4"/"rar2"/
  /// "rar15" write a legacy RAR 4.x / 2.x / 1.5 archive and "rar13" the
  /// DOS-era RAR 1.3/1.4 (`RE~^`) container (old-style `.rar`/`.r00`
  /// volume sets with `volume_size`); the RAR5-only options (dictionary
  /// size, quick-open, BLAKE2sp, owner/stream records) are rejected there,
  /// and "rar13" additionally rejects header encryption, inline recovery
  /// records and recovery volumes.
  pub format: Option<String>,
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
  /// Compression threads for the appended members (1..=64; 0 = automatic).
  pub thread_count: Option<f64>,
}
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
  /// CRC32 of the member (undefined when unknown, e.g. while streaming).
  pub crc32: Option<u32>,
  /// Creation time as Unix seconds (undefined for members without a
  /// FILE_TIME extra record).
  pub ctime: Option<f64>,
  /// Last-access time as Unix seconds (undefined when not recorded).
  pub atime: Option<f64>,
  /// Host OS normalized across containers: 0 = Windows, 1 = Unix (RAR4's
  /// raw DOS/OS2/Win32/Unix/… values are mapped onto these two).
  pub host_os: f64,
  /// Raw attribute word stored in the header (platform specific).
  pub attributes: f64,
  /// RAR5/RAR7 compression version: 0 = RAR5, 1 = RAR7. RAR4 legacy members
  /// always report 0; their unpack version (15/20/26/29/36) is exposed as
  /// `version` instead.
  pub comp_version: u8,
  /// Member version name ("v14" .. "v70").
  pub version: String,
  /// Compressed dictionary size in bytes (undefined when unknown).
  pub dict_size_bytes: Option<f64>,
  /// Per-member comment bytes (undefined when absent; RAR4 only — RAR5
  /// has no member-comment block, so those members always report
  /// `undefined`).
  pub comment: Option<Buffer>,
  /// The per-member solid flag (RAR5/RAR 1.3/4.x chain continuations).
  /// RAR 1.5/2.x chains are archive-level (`ArchiveReader::is_solid`) and
  /// may report `false` here, matching the on-disk bit.
  pub solid: bool,
}
#[napi(object)]
pub struct RenameEntry {
  /// Existing member name (full stored path or basename).
  pub from: String,
  /// New name.
  pub to: String,
}
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
  /// Ceiling on the declared size of a service payload buffered whole
  /// while reading: the archive comment and NTFS alternate data streams.
  /// Default: 64 MiB. Raise it for archives with large alternate data
  /// streams; pass 0 for no limit (trusted archives only).
  pub max_metadata_bytes: Option<f64>,
  /// Skip members whose destination path already exists (like `-o-`).
  pub skip_existing: Option<bool>,
  /// Freshen (`-f`): extract a member only when its destination exists and
  /// the archived modification time is newer; a missing destination is
  /// skipped.
  pub freshen: Option<bool>,
  /// Update (`-u`): like `freshen`, but a missing destination is extracted
  /// too. Takes precedence when both are set.
  pub update: Option<bool>,
  /// Extraction worker threads for this run (1..=64; 0 = automatic).
  /// Scoped to this call, like `-mt<N>`; falls back to the process-global
  /// default when unset.
  pub threads: Option<f64>,
  /// Maximum uncompressed size of a single member, in bytes (like
  /// `-mdx`-style caps). Unset or 0 means no limit; extraction to disk is
  /// streaming, so this is opt-in hardening.
  pub max_unpacked_bytes: Option<f64>,
  /// Maximum total uncompressed size for this run, in bytes. Unset or 0
  /// means no limit.
  pub max_total_unpacked_bytes: Option<f64>,
  /// Rename colliding outputs `name(1).ext` instead of overwriting
  /// (like `-or`).
  pub auto_rename: Option<bool>,
  /// Keep partially written files after a decode error (like `-k`);
  /// otherwise the partial file is removed.
  pub keep_broken: Option<bool>,
  /// Restore the creation time on extracted files.
  pub set_creation_time: Option<bool>,
  /// Restore the last-access time on extracted files.
  pub set_access_time: Option<bool>,
  /// Skip symbolic links instead of materializing them (like `-ol-`).
  pub skip_links: Option<bool>,
  /// Extract links with dangerous targets as-is (like `-ola`).
  pub allow_unsafe_links: Option<bool>,
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::options::{JS_MAX_SAFE_INTEGER, checked_js_integer};

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
}
