//! RAR5 create-side policy owned by the RAR5 format module: mapping the
//! typed dictionary request onto the legacy create options. RAR7 (v70)
//! selection rules live here so RAR5/RAR7 concerns stay out of the
//! shared archive writer layer.

use crate::DictionarySize;

/// Member codec version within the RAR5 container.
///
/// RAR5 headers define exactly two member codec versions: `comp_version 0`
/// (v50) and `comp_version 1` (RAR7 / v70 with the 80-entry distance
/// table). RAR7 (v70) members are only ever written when selected
/// explicitly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CompressionVersion {
    /// RAR5 v50 members (the default). Dictionaries are limited to 4 GiB.
    #[default]
    V50,
    /// RAR7 (v70) members with a declared byte dictionary.
    V70,
}

/// Map a requested dictionary onto the legacy `(dict_size_log,
/// dict_size_bytes)` create fields.
///
/// `v70` selects RAR7 (v70) members, which always declare an actual byte
/// count (32 MiB when unset). Otherwise the size maps to a RAR5 log when
/// it fits (<= 4 GiB) and passes the byte count through above that:
/// like WinRAR's `-md`, a > 4 GiB request is capped at twice the member
/// size, so small members stay plain v50 with the capped log and only
/// members whose effective dictionary exceeds 4 GiB become v70.
pub(crate) fn dictionary_fields(
    v70: bool,
    dictionary: Option<DictionarySize>,
) -> (Option<u8>, Option<u64>) {
    if v70 {
        (
            None,
            Some(dictionary.unwrap_or(DictionarySize::DEFAULT).bytes()),
        )
    } else {
        (
            dictionary.and_then(DictionarySize::rar5_log),
            dictionary
                .filter(|size| size.rar5_log().is_none())
                .map(DictionarySize::bytes),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{DictionarySize, dictionary_fields};

    #[test]
    fn v50_maps_logs_and_passes_large_byte_counts_through() {
        let small = DictionarySize::try_from(64 * 1024 * 1024u64).unwrap();
        assert_eq!(dictionary_fields(false, Some(small)), (Some(9), None));
        assert_eq!(dictionary_fields(false, None), (None, None));
        let big = DictionarySize::try_from(6 * 1024 * 1024 * 1024u64).unwrap();
        assert_eq!(
            dictionary_fields(false, Some(big)),
            (None, Some(6 * 1024 * 1024 * 1024))
        );
    }

    #[test]
    fn v70_declares_byte_sizes_and_defaults_to_32_mib() {
        assert_eq!(
            dictionary_fields(true, None),
            (None, Some(32 * 1024 * 1024))
        );
        let small = DictionarySize::try_from(64 * 1024 * 1024u64).unwrap();
        assert_eq!(
            dictionary_fields(true, Some(small)),
            (None, Some(64 * 1024 * 1024))
        );
    }
}
