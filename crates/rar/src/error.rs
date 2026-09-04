/// Error types for RAR archive operations.
use std::fmt;
use std::io;

/// Stable, machine-readable category for a [`RarError`].
///
/// Unlike formatted error messages, these values are suitable for logs,
/// bindings, telemetry, and command-line exit-code mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    Format,
    InvalidState,
    InvalidOption,
    CrcMismatch,
    HashMismatch,
    Encrypted,
    Unsupported,
    Security,
    LimitExceeded,
    MemberNotFound,
    AmbiguousMember,
    StaleEntryId,
    ArchiveLocked,
    Cancelled,
    WrongPassword,
    Io,
}

impl ErrorCode {
    /// Return the stable snake-case representation of this error category.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Format => "format",
            Self::InvalidState => "invalid_state",
            Self::InvalidOption => "invalid_option",
            Self::CrcMismatch => "crc_mismatch",
            Self::HashMismatch => "hash_mismatch",
            Self::Encrypted => "encrypted",
            Self::Unsupported => "unsupported",
            Self::Security => "security",
            Self::LimitExceeded => "limit_exceeded",
            Self::MemberNotFound => "member_not_found",
            Self::AmbiguousMember => "ambiguous_member",
            Self::StaleEntryId => "stale_entry_id",
            Self::ArchiveLocked => "archive_locked",
            Self::Cancelled => "cancelled",
            Self::WrongPassword => "wrong_password",
            Self::Io => "io",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum RarError {
    /// Invalid or unexpected archive format.
    Format(String),
    /// The requested operation is not valid for the archive's current mode.
    InvalidState(String),
    /// An API option is invalid or uses an unsupported value combination.
    InvalidOption(String),
    /// CRC32 checksum mismatch.
    Crc {
        expected: u32,
        actual: u32,
        context: String,
    },
    /// BLAKE2sp (or other file hash) mismatch.
    HashMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
        context: String,
    },
    /// Encrypted content encountered without a password.
    Encrypted(String),
    /// Valid RAR feature not yet implemented.
    Unsupported(String),
    /// Security policy violation (path traversal, unsafe member names, etc.).
    Security(String),
    /// A configured size or resource limit was exceeded.
    LimitExceeded {
        /// The configured limit (bytes, iterations, etc.).
        limit: u64,
        /// What was being done when the limit was hit.
        context: String,
    },
    /// The requested member does not exist in the archive.
    MemberNotFound { name: String },
    /// A name expected to identify one member matched multiple entries.
    AmbiguousMember {
        /// The duplicate archive member name.
        name: String,
        /// Number of entries carrying `name`.
        matches: usize,
    },
    /// An opaque entry ID belongs to another or outdated archive catalog.
    StaleEntryId,
    /// The archive is locked (read-only).
    ArchiveLocked,
    /// The operation was cancelled through the caller's cancellation flag
    /// (see [`crate::RarArchive::set_cancel_flag`]).
    Cancelled,
    /// An encrypted archive was opened with the wrong password.
    WrongPassword,
    /// Underlying I/O error.
    Io(io::Error),
}

impl RarError {
    /// Return the stable machine-readable category for this error.
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Format(_) => ErrorCode::Format,
            Self::InvalidState(_) => ErrorCode::InvalidState,
            Self::InvalidOption(_) => ErrorCode::InvalidOption,
            Self::Crc { .. } => ErrorCode::CrcMismatch,
            Self::HashMismatch { .. } => ErrorCode::HashMismatch,
            Self::Encrypted(_) => ErrorCode::Encrypted,
            Self::Unsupported(_) => ErrorCode::Unsupported,
            Self::Security(_) => ErrorCode::Security,
            Self::LimitExceeded { .. } => ErrorCode::LimitExceeded,
            Self::MemberNotFound { .. } => ErrorCode::MemberNotFound,
            Self::AmbiguousMember { .. } => ErrorCode::AmbiguousMember,
            Self::StaleEntryId => ErrorCode::StaleEntryId,
            Self::ArchiveLocked => ErrorCode::ArchiveLocked,
            Self::Cancelled => ErrorCode::Cancelled,
            Self::WrongPassword => ErrorCode::WrongPassword,
            Self::Io(_) => ErrorCode::Io,
        }
    }
}

impl fmt::Display for RarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RarError::Format(msg) => write!(f, "RAR format error: {msg}"),
            RarError::InvalidState(msg) => write!(f, "invalid archive state: {msg}"),
            RarError::InvalidOption(msg) => write!(f, "invalid option: {msg}"),
            RarError::Crc {
                expected,
                actual,
                context,
            } => write!(
                f,
                "CRC mismatch in {context}: expected {expected:#010X}, got {actual:#010X}"
            ),
            RarError::HashMismatch {
                expected,
                actual,
                context,
            } => write!(
                f,
                "hash mismatch in {context}: expected {}, got {}",
                hex32(expected),
                hex32(actual)
            ),
            RarError::Encrypted(msg) => write!(f, "encrypted: {msg}"),
            RarError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            RarError::Security(msg) => write!(f, "security: {msg}"),
            RarError::LimitExceeded { limit, context } => {
                write!(f, "limit exceeded ({limit}): {context}")
            }
            RarError::MemberNotFound { name } => write!(f, "member not found: {name}"),
            RarError::AmbiguousMember { name, matches } => {
                write!(f, "member name is ambiguous: {name} ({matches} matches)")
            }
            RarError::StaleEntryId => write!(f, "entry ID belongs to another or outdated catalog"),
            RarError::ArchiveLocked => write!(f, "archive is locked"),
            RarError::Cancelled => write!(f, "operation cancelled"),
            RarError::WrongPassword => write!(f, "encrypted: wrong password"),
            RarError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

fn hex32(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

impl std::error::Error for RarError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RarError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for RarError {
    fn from(e: io::Error) -> Self {
        RarError::Io(e)
    }
}

pub type RarResult<T> = Result<T, RarError>;

#[cfg(test)]
mod tests {
    use super::{ErrorCode, RarError};

    #[test]
    fn error_codes_are_stable_and_machine_readable() {
        let cases = [
            (RarError::Format(String::new()), ErrorCode::Format, "format"),
            (
                RarError::AmbiguousMember {
                    name: "duplicate".into(),
                    matches: 2,
                },
                ErrorCode::AmbiguousMember,
                "ambiguous_member",
            ),
            (
                RarError::StaleEntryId,
                ErrorCode::StaleEntryId,
                "stale_entry_id",
            ),
            (
                RarError::Io(std::io::Error::other("disk")),
                ErrorCode::Io,
                "io",
            ),
        ];

        for (error, expected, text) in cases {
            assert_eq!(error.code(), expected);
            assert_eq!(expected.as_str(), text);
            assert_eq!(expected.to_string(), text);
        }
    }
}
