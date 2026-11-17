//! Recovery-record and recovery-volume support.
//!
//! RAR5 uses the 16-bit inline `RR` records (`rar50`) and the REV5 `.rev`
//! container (`rev50`); the legacy RAR 1.5–4.x family uses NEWSUB `RR`
//! records (`legacy`) and the GF(2^8) parity `.rev` files (`rev3`).

// Same rule as `format`: the tree is `pub(crate)` (ADR 0007 retired the
// `raw` feature); a few wire-level helpers have no in-tree caller.

mod legacy;
pub use legacy::{
    LegacyDamagedSector, LegacyRepair, repair_legacy_archive_path,
    repair_legacy_archive_path_with_password,
};

/// Legacy recovery-record helpers (crate-internal): the RAR4 creation
/// path (`archive/create.rs`) builds NEWSUB blocks through here, and the
/// RAR4 edit path (`archive/rar4_edit/`) scans for an existing record
/// to strip and rebuild.
pub(crate) mod legacy_rr {
    pub(crate) use super::legacy::{
        build_legacy_recovery_block, recovery_sector_count, scan_protect_file,
    };
    // The slice scanners have no production caller left (the edit path always
    // goes through the password-aware file variant); the RAR4 edit tests
    // still use them to assert a record is present.
    #[cfg(test)]
    pub(crate) use super::legacy::{
        build_legacy_protect_head_block, scan_protect, scan_protect_with_password,
    };
}
pub(crate) mod parity;
pub mod rar50;
pub(crate) mod rev3;
pub mod rev50;

use crate::error::{RarError, RarResult};

impl From<rar50::Error> for RarError {
    fn from(e: rar50::Error) -> Self {
        match e {
            rar50::Error::Cancelled => RarError::Cancelled,
            // "No record at all" is not corruption: report it as unsupported
            // (the same category the legacy repair path uses) so a caller can
            // tell "nothing to repair with" apart from a malformed record,
            // and so the CLI can render it without an "unsupported:" prefix.
            rar50::Error::NoRecoveryRecord => RarError::Unsupported(e.to_string()),
            // No "repair: " prefix here: the caller supplies the operation as
            // context, and this used to make the CLI print it twice
            // ("repair: RAR format error: repair: …").
            other => RarError::Format(other.to_string()),
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
///
/// [`repair_archive_path_report`] is the same run with the record's own state
/// reported too, for callers that must tell a healthy archive from one whose
/// members are fine but whose protection is gone.
pub fn repair_archive_path(src: &std::path::Path, dst: &std::path::Path) -> RarResult<bool> {
    Ok(repair_archive_path_report(src, dst)? == RepairOutcome::Repaired)
}

/// What a `rar r` run found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairOutcome {
    /// Nothing needed repairing: the data is intact and any recovery record the
    /// archive carries is whole.
    Intact,
    /// Damage was found and the repaired archive was written to `dst`.
    Repaired,
    /// The members are intact but the recovery record itself is damaged (a
    /// truncated archive usually takes the record's tail with it), so the
    /// archive kept its data and lost its protection. `rar r` reports this and
    /// exits 3, as WinRAR does, rather than calling the archive healthy.
    RecordDamaged,
}

/// [`repair_archive_path`] with the record's state reported: `RecordDamaged`
/// means the archive's own bytes are fine but its recovery record is not.
pub fn repair_archive_path_report(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> RarResult<RepairOutcome> {
    repair_archive_path_with_report(src, dst, None, None)
}

/// [`repair_archive_path_report`] with a cancellation flag and progress
/// reporting.
pub fn repair_archive_path_with_report(
    src: &std::path::Path,
    dst: &std::path::Path,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress: Option<&mut dyn FnMut(u64, u64)>,
) -> RarResult<RepairOutcome> {
    let mut input = std::fs::File::open(src).map_err(RarError::Io)?;
    // Stage into a temp sibling and rename on success, so a failed
    // repair never leaves a partial file at `dst`.
    let (mut staged, mut output) = crate::fs::atomic::StagedFile::create(dst)?;
    let (repaired, record_truncated) =
        rar50::repair_inline_recovery_archive_path(&mut input, &mut output, cancel, progress)
            .map_err(RarError::from)?;
    if !repaired {
        // Intact archive: `dst` stays untouched (like `rar r`'s "All OK").
        return Ok(if record_truncated {
            RepairOutcome::RecordDamaged
        } else {
            RepairOutcome::Intact
        });
    }
    drop(output);
    staged.commit()?;
    Ok(RepairOutcome::Repaired)
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
    Ok(repair_archive_path_with_report(src, dst, cancel, progress)? == RepairOutcome::Repaired)
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
