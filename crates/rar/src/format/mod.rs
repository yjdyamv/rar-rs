//! Format implementations: RAR 1.3/1.4 (the `RE~^` family), RAR4 (the
//! legacy `Rar!\x1a\x07\x00` container family) and RAR5 (the modern
//! container, including RAR7 v70 members). Internal home of the historical
//! `crate::format::rar4` / `crate::rar50` module trees.

// The trees are `pub(crate)` (ADR 0007 retired the downstream `raw`
// feature); wire-level helpers needed outside the crate are re-exported
// through `wire`, and the remaining constants/helpers stay crate-internal.

pub mod rar13;
pub mod rar4;
pub mod rar5;
pub(crate) mod shared;

/// Decode legacy host-encoded text with the process's system ANSI code page
/// (`CP_ACP`: CP936 on Chinese Windows, CP1252 on Western systems, CP65001
/// when the UTF-8 locale is enabled).
///
/// Legacy RAR member names (RAR4 without `FHD_UNICODE`, RAR 1.3/1.4) carry
/// bytes in the writer's OEM/ANSI encoding. On Windows the ANSI code page is
/// the closest native reading; off Windows these encodings are unknowable,
/// so callers keep their existing UTF-8-lossy behavior.
///
/// Returns `None` when the conversion fails or when the code page cannot
/// represent the bytes (with flags `0` those become U+FFFD, which must never
/// leak into member names).
#[cfg(windows)]
pub(crate) fn decode_system_ansi(bytes: &[u8]) -> Option<String> {
    use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar};

    if bytes.is_empty() {
        return Some(String::new());
    }
    let len = i32::try_from(bytes.len()).ok()?;
    // Size the UTF-16 buffer first, then convert into it.
    let wide_len =
        unsafe { MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0) };
    if wide_len <= 0 {
        return None;
    }
    let mut wide = vec![0u16; wide_len as usize];
    let written =
        unsafe { MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, wide.as_mut_ptr(), wide_len) };
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);
    let decoded = String::from_utf16(&wide).ok()?;
    (!decoded.contains(char::REPLACEMENT_CHARACTER)).then_some(decoded)
}

/// Non-Windows stub: the legacy Windows code pages have no native
/// equivalent here, so the callers' existing fallbacks stand.
#[cfg(not(windows))]
pub(crate) fn decode_system_ansi(_bytes: &[u8]) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::decode_system_ansi;

    /// `CP_ACP` is locale-dependent — CP936 on Chinese Windows, CP1252 on
    /// Western systems, CP65001 on hosts with the UTF-8 locale enabled — so
    /// these bytes cannot be asserted to decode to particular characters.
    /// Genuine CP936 behavior needs a Chinese-locale Windows runner; here we
    /// pin the contract that a successful decode never smuggles replacement
    /// characters into member names.
    #[test]
    fn system_ansi_never_returns_replacement_characters() {
        let cases: [&[u8]; 5] = [
            b"plain.txt",
            &[0xE9],
            &[0xE9, b'.', b't'],
            &[0xD6, 0xD0], // "中" in CP936
            &[0xC3, 0xA9], // "é" in UTF-8 / CP65001
        ];
        for bytes in cases {
            if let Some(text) = decode_system_ansi(bytes) {
                assert!(
                    !text.contains(char::REPLACEMENT_CHARACTER),
                    "ANSI decode of {bytes:02X?} leaked U+FFFD: {text:?}"
                );
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn system_ansi_handles_empty_and_ascii() {
        assert_eq!(decode_system_ansi(b""), Some(String::new()));
        assert_eq!(decode_system_ansi(b"plain.txt"), Some("plain.txt".into()));
    }

    #[cfg(not(windows))]
    #[test]
    fn system_ansi_is_none_off_windows() {
        assert_eq!(decode_system_ansi(b""), None);
        assert_eq!(decode_system_ansi(b"plain.txt"), None);
    }
}
