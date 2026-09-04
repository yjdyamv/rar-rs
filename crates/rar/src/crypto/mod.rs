//! Crypto layer: per-format-family encryption primitives.
//!
//! Mirrors the reference layout — `crypto/rar50.rs` holds the RAR5
//! AES-256-CBC stack (PBKDF2-style key derivation, hash-key MAC, streaming
//! CBC). Later format families add their own `crypto/rar*.rs` modules.

pub mod rar15;
pub mod rar20;
pub mod rar30;
pub mod rar50;

pub use rar15::Rar15Cipher;
pub use rar20::{Error as Rar20Error, Rar20Cipher};
pub use rar30::{Error as Rar30Error, Rar30Cipher};
pub use rar50::*;

/// The longest password any RAR key derivation sees.
///
/// Reference builds copy the password into a 128-element `wchar_t` buffer and
/// keep 127 entries plus a terminator, so nothing past character 127 reaches
/// the KDF. Measured against RAR 7.12: a 133-character password and its first
/// 127 characters open the same archive, and the first 126 do not.
pub const MAX_PASSWORD_CHARS: usize = 127;

/// Cuts a password down to the [`MAX_PASSWORD_CHARS`] the reference tools keep.
///
/// Without this rars derives a key no official tool can reproduce, in both
/// directions: it cannot open a WinRAR archive when handed the same long
/// password, and the archives it writes from character 128 on cannot be opened
/// by WinRAR with that password or with any prefix of it. The boundary is
/// sharp, so a password of exactly 127 characters is untouched.
///
/// A character here is a Unicode scalar, which matches the Linux reference
/// where `wchar_t` is 4 bytes wide. The Windows build has a 2-byte `wchar_t`
/// and so keeps 127 UTF-16 code units instead. The two disagree only for a
/// password carrying astral characters past position 127, where they derive
/// different keys from each other, so no single rule matches both. Scalars win
/// here because truncating UTF-16 can split a surrogate pair, and a Rust `str`
/// cannot hold the lone surrogate that would produce.
pub fn clamp_password(password: &[u8]) -> &[u8] {
    match std::str::from_utf8(password) {
        Ok(text) => match text.char_indices().nth(MAX_PASSWORD_CHARS) {
            Some((byte_offset, _)) => &password[..byte_offset],
            None => password,
        },
        // RAR 1.3 through 2.0 take single-byte OEM/ANSI passwords, so one byte
        // is one character.
        Err(_) => &password[..password.len().min(MAX_PASSWORD_CHARS)],
    }
}
