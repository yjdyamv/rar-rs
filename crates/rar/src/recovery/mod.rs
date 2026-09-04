//! Recovery-record and recovery-volume support (RAR5 family / v50).

mod legacy;
pub use legacy::repair_legacy_archive_path;

/// Write-side legacy recovery-block helpers (crate-internal; the RAR4
/// creation path calls these from `archive/create.rs`).
pub(crate) mod legacy_rr {
    pub(crate) use super::legacy::{build_legacy_recovery_block, recovery_sector_count};
}
pub mod rar50;
pub mod rev50;

use crate::error::{RarError, RarResult};

impl From<rar50::Error> for RarError {
    fn from(e: rar50::Error) -> Self {
        match e {
            rar50::Error::Cancelled => RarError::Cancelled,
            other => RarError::Format(format!("repair: {other}")),
        }
    }
}

/// Repair an archive in memory using its inline recovery record (like
/// `rar r`). Returns the repaired bytes; an undamaged archive is returned
/// unchanged.
pub fn repair_archive(input: &[u8]) -> RarResult<Vec<u8>> {
    rar50::repair_inline_recovery_archive(input).map_err(RarError::from)
}

/// Streaming repair of a damaged archive on disk (like `rar r`): reads
/// `src` and writes the repaired archive to `dst`, holding only the
/// recovery data and damaged-shard outputs in memory — archives far
/// larger than RAM can be repaired. Returns `true` when damage was
/// found and repaired, `false` when the archive was already intact.
/// `dst` must not alias `src`.
pub fn repair_archive_path(src: &std::path::Path, dst: &std::path::Path) -> RarResult<bool> {
    repair_archive_path_with(src, dst, None, None)
}

/// [`repair_archive_path`] with a cancellation flag and progress
/// reporting. `cancel` is polled at every streaming checkpoint (scan,
/// shard recovery, copy); when set, the operation returns
/// [`crate::RarError::Cancelled`] and no partial file is left at `dst`.
/// `progress` receives `(done_bytes, total_bytes)` — non-decreasing
/// during the copy phase, reaching `total` on success.
pub fn repair_archive_path_with(
    src: &std::path::Path,
    dst: &std::path::Path,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress: Option<&mut dyn FnMut(u64, u64)>,
) -> RarResult<bool> {
    let mut input = std::fs::File::open(src).map_err(RarError::Io)?;
    // Stage into a temp sibling and rename on success, so a failed
    // repair never leaves a partial file at `dst`.
    let tmp = crate::io_util::temp_sibling_path(dst);
    let mut output = crate::io_util::read_write_create(&tmp).map_err(RarError::Io)?;
    let repaired =
        rar50::repair_inline_recovery_archive_path(&mut input, &mut output, cancel, progress)
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                RarError::from(e)
            })?;
    if !repaired {
        // Intact archive: `dst` stays untouched (like `rar r`'s "All OK").
        let _ = std::fs::remove_file(&tmp);
        return Ok(false);
    }
    output.sync_all().map_err(RarError::Io)?;
    crate::io_util::replace_file(&tmp, dst)?;
    Ok(true)
}

/// Rebuild missing volumes of a multi-volume set from its `.rev` recovery
/// volumes (like `rar rc`).
pub fn rebuild_missing_volumes(
    first_volume: &std::path::Path,
) -> RarResult<Vec<std::path::PathBuf>> {
    rebuild_missing_volumes_with(first_volume, None, None)
}

/// [`rebuild_missing_volumes`] with a cancellation flag and progress
/// reporting (`rebuilt_bytes, total_bytes`, non-decreasing).
pub fn rebuild_missing_volumes_with(
    first_volume: &std::path::Path,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress: Option<&mut dyn FnMut(u64, u64)>,
) -> RarResult<Vec<std::path::PathBuf>> {
    rev50::rebuild_missing_volumes_with(first_volume, cancel, progress)
}
