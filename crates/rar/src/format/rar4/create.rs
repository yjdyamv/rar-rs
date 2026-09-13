//! RAR4 create-side policy owned by the RAR4 format module: options the
//! legacy container cannot express are rejected here so RAR4 concerns stay
//! out of the shared archive writer layer.

use crate::error::{RarError, RarResult};

/// The subset of typed writer options the RAR4 container cannot express.
///
/// Grouped so the shared writer hands the RAR4 policy exactly what it needs
/// to reject, keeping the legacy container decoupled from the typed writer
/// layer (no `archive::` type crosses into the format module).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Rar4WriteOptions {
    pub quick_open: bool,
    pub blake2: bool,
    pub save_owner: bool,
    pub save_streams: bool,
    pub has_dictionary: bool,
}

/// Reject typed options the RAR4 container cannot express. Called by the
/// shared writer validation when the RAR4 format is selected.
pub(crate) fn validate_rar4_only(options: Rar4WriteOptions) -> RarResult<()> {
    if options.quick_open {
        return Err(RarError::InvalidOption(
            "quick-open is not supported for RAR4 archives".into(),
        ));
    }
    if options.blake2 {
        return Err(RarError::InvalidOption(
            "BLAKE2sp hashes are not supported for RAR4 archives".into(),
        ));
    }
    if options.save_owner || options.save_streams {
        return Err(RarError::InvalidOption(
            "owner and stream records are not supported for RAR4 archives".into(),
        ));
    }
    // The RAR4 writer selects its own per-member window (64 KiB - 4 MiB,
    // from the member size); it has no configurable dictionary, so a
    // requested size would be silently ignored by the legacy layer.
    if options.has_dictionary {
        return Err(RarError::InvalidOption(
            "RAR4 archives do not support configurable dictionary sizes".into(),
        ));
    }
    Ok(())
}

/// Reject a member the RAR4 container cannot describe: its FILE_HEAD stores
/// the packed and unpacked sizes in 32-bit fields, so a larger member would
/// be written in full while its header declares a truncated length.
pub(crate) fn ensure_member_size(unpacked: u64) -> RarResult<()> {
    if unpacked > u32::MAX as u64 {
        return Err(RarError::InvalidOption(format!(
            "RAR4 members cannot exceed {} bytes (got {unpacked})",
            u32::MAX
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ensure_member_size;

    #[test]
    fn rar4_member_size_above_u32_is_rejected() {
        assert!(ensure_member_size(0).is_ok());
        assert!(ensure_member_size(u32::MAX as u64).is_ok());
        assert!(ensure_member_size(u32::MAX as u64 + 1).is_err());
    }
}
