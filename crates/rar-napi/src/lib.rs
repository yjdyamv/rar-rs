//! RAR archive operations for the Smart Archive VS Code extension.
//!
//! Wraps the pure-Rust `rar-rs` crate behind a napi-rs API for creating, reading,
//! testing, listing, extracting, repairing, and modifying RAR archives.
use napi::bindgen_prelude::*;
use napi_derive::napi;

mod error;
mod options;
mod tasks;

pub use tasks::{
  append_entries, create_archive, delete_entries, extract_archive, extract_member, list_entries,
  list_entries_detailed, list_entries_quick, lock_archive, read_member, rebuild_missing_volumes,
  rename_entries, repair_archive, set_comment, set_recovery, test_archive,
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
  /// Values up to 4 GiB must be powers of two (128 KiB .. 4 GiB) under
  /// `format: 'rar5'`; values above 4 GiB, and any size under
  /// `format: 'rar7'`, are accepted as byte counts (including
  /// non-power-of-two values like `6m`) and produce RAR7 (v70) members.
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
  /// Request a specific archive format: "rar5" (default), "rar7", "rar4",
  /// "rar2", "rar15" or "rar13". "rar5" auto-promotes an individual member to
  /// RAR7 (v70) when its effective dictionary exceeds 4 GiB. "rar7" forces
  /// v70 members at any dictionary (32 MiB by default). "rar4"/"rar2"/
  /// "rar15" write a legacy RAR 4.x / 2.x / 1.5 archive and "rar13" the
  /// DOS-era RAR 1.3/1.4 (`RE~^`, single-volume) container; the RAR5-only
  /// options (dictionary size, quick-open, BLAKE2sp, owner/stream records)
  /// are rejected there, and "rar13" additionally rejects header encryption
  /// and recovery volumes.
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
  /// Host OS of the producing archiver (0 = MS-DOS, 1 = OS/2, 2 = Win32,
  /// 3 = Unix/32-bit, 6 = Unix/64-bit, 7 = macOS).
  pub host_os: f64,
  /// Raw attribute word stored in the header (platform specific).
  pub attributes: f64,
  /// RAR5/RAR7 compression version: 0 = RAR5, 1 = RAR7. RAR4 legacy members
  /// always report 0; their unpack version (15/20/26/29/36) is exposed as
  /// `version` instead.
  pub comp_version: u8,
  /// Member version name ("v15" .. "v70").
  pub version: String,
  /// Compressed dictionary size in bytes (undefined when unknown).
  pub dict_size_bytes: Option<f64>,
  /// Per-member comment bytes (undefined when absent; RAR4 only — RAR5
  /// has no member-comment block, so those members always report
  /// `undefined`).
  pub comment: Option<Buffer>,
  /// Whether the member belongs to a solid chain (shares a window with
  /// its predecessor).
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
