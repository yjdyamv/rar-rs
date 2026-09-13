//! RAR 1.3/1.4 create-side policy owned by the format module: options the
//! DOS-era container cannot express are rejected here so RAR13 concerns stay
//! out of the shared archive writer layer.

use crate::error::{RarError, RarResult};

/// The subset of typed writer options the RAR 1.3/1.4 container cannot
/// express.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Rar13WriteOptions {
    pub quick_open: bool,
    pub blake2: bool,
    pub recovery_percent: Option<u8>,
    pub recovery_volumes_percent: Option<u8>,
    pub recovery_volume_count: Option<u32>,
    pub save_owner: bool,
    pub save_streams: bool,
    pub has_dictionary: bool,
    pub encrypt_headers: bool,
}

/// Reject typed options the RAR 1.3/1.4 container cannot express.
pub(crate) fn validate_rar13_only(options: Rar13WriteOptions) -> RarResult<()> {
    if options.quick_open {
        return Err(RarError::InvalidOption(
            "quick-open is not supported for RAR 1.3/1.4 archives".into(),
        ));
    }
    if options.blake2 {
        return Err(RarError::InvalidOption(
            "BLAKE2sp hashes are not supported for RAR 1.3/1.4 archives".into(),
        ));
    }
    if options.recovery_percent.is_some() {
        return Err(RarError::InvalidOption(
            "recovery records are not supported for RAR 1.3/1.4 archives".into(),
        ));
    }
    if options.recovery_volumes_percent.is_some() || options.recovery_volume_count.is_some() {
        return Err(RarError::InvalidOption(
            "recovery volumes are not supported for RAR 1.3/1.4 archives".into(),
        ));
    }
    if options.save_owner || options.save_streams {
        return Err(RarError::InvalidOption(
            "owner and stream records are not supported for RAR 1.3/1.4 archives".into(),
        ));
    }
    if options.has_dictionary {
        return Err(RarError::InvalidOption(
            "RAR 1.3/1.4 archives do not support configurable dictionary sizes".into(),
        ));
    }
    if options.encrypt_headers {
        return Err(RarError::InvalidOption(
            "header encryption is not supported for RAR 1.3/1.4 archives".into(),
        ));
    }
    Ok(())
}

/// Reject a member the RAR 1.3/1.4 container cannot describe: its file
/// header stores both sizes in 32-bit fields.
pub(crate) fn ensure_member_size(unpacked: u64) -> RarResult<()> {
    if unpacked > u32::MAX as u64 {
        return Err(RarError::InvalidOption(format!(
            "RAR 1.3/1.4 members cannot exceed {} bytes (got {unpacked})",
            u32::MAX
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Rar13WriteOptions, ensure_member_size, validate_rar13_only};

    #[test]
    fn rar13_member_size_above_u32_is_rejected() {
        assert!(ensure_member_size(0).is_ok());
        assert!(ensure_member_size(u32::MAX as u64).is_ok());
        assert!(ensure_member_size(u32::MAX as u64 + 1).is_err());
    }

    #[test]
    fn rar13_rejects_options_it_cannot_express() {
        assert!(validate_rar13_only(Rar13WriteOptions::default()).is_ok());
        for options in [
            Rar13WriteOptions {
                quick_open: true,
                ..Default::default()
            },
            Rar13WriteOptions {
                blake2: true,
                ..Default::default()
            },
            Rar13WriteOptions {
                recovery_percent: Some(5),
                ..Default::default()
            },
            Rar13WriteOptions {
                recovery_volume_count: Some(1),
                ..Default::default()
            },
            Rar13WriteOptions {
                save_owner: true,
                ..Default::default()
            },
            Rar13WriteOptions {
                save_streams: true,
                ..Default::default()
            },
            Rar13WriteOptions {
                has_dictionary: true,
                ..Default::default()
            },
            Rar13WriteOptions {
                encrypt_headers: true,
                ..Default::default()
            },
        ] {
            assert!(matches!(
                validate_rar13_only(options),
                Err(crate::error::RarError::InvalidOption(_))
            ));
        }
    }
}
