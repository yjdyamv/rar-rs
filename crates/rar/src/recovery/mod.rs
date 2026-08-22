//! Recovery-record and recovery-volume support (RAR5).

pub mod rar5;
pub mod rev5;

use crate::error::{RarError, RarResult};

/// Repair an archive in memory using its inline recovery record (like
/// `rar r`). Returns the repaired bytes; an undamaged archive is returned
/// unchanged.
pub fn repair_archive(input: &[u8]) -> RarResult<Vec<u8>> {
    rar5::repair_inline_recovery_archive(input)
        .map_err(|e| RarError::Format(format!("repair: {e}")))
}

/// Rebuild missing volumes of a multi-volume set from its `.rev` recovery
/// volumes (like `rar rc`).
pub fn rebuild_missing_volumes(
    first_volume: &std::path::Path,
) -> RarResult<Vec<std::path::PathBuf>> {
    rev5::rebuild_missing_volumes(first_volume)
}
