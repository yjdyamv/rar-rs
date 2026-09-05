//! RAR4 create-side policy owned by the RAR4 format module: options the
//! legacy container cannot express are rejected here so RAR4 concerns stay
//! out of the shared archive writer layer.

use crate::error::{RarError, RarResult};

/// Reject typed options the RAR4 container cannot express. Called by the
/// shared writer validation when the RAR4 format is selected.
pub(crate) fn validate_rar4_only(
    quick_open: bool,
    blake2: bool,
    recovery_volumes_percent: Option<u8>,
    recovery_volume_count: Option<u32>,
    save_owner: bool,
    save_streams: bool,
    has_dictionary: bool,
) -> RarResult<()> {
    if quick_open {
        return Err(RarError::InvalidOption(
            "quick-open is not supported for RAR4 archives".into(),
        ));
    }
    if blake2 {
        return Err(RarError::InvalidOption(
            "BLAKE2sp hashes are not supported for RAR4 archives".into(),
        ));
    }
    if recovery_volumes_percent.is_some() || recovery_volume_count.is_some() {
        return Err(RarError::InvalidOption(
            "recovery volumes are not supported for RAR4 archives".into(),
        ));
    }
    if save_owner || save_streams {
        return Err(RarError::InvalidOption(
            "owner and stream records are not supported for RAR4 archives".into(),
        ));
    }
    // The RAR4 writer selects its own per-member window (64 KiB - 4 MiB,
    // from the member size); it has no configurable dictionary, so a
    // requested size would be silently ignored by the legacy layer.
    if has_dictionary {
        return Err(RarError::InvalidOption(
            "RAR4 archives do not support configurable dictionary sizes".into(),
        ));
    }
    Ok(())
}
