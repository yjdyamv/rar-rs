//! Error types for RAR archive operations.
use std::fmt;
use std::io;

/// Stable, machine-readable category for a [`RarError`].
///
/// Unlike formatted error messages, these values are suitable for logs,
/// bindings, telemetry, and command-line exit-code mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    /// Malformed archive bytes: a header, block or field the format cannot
    /// produce.
    Format,
    /// The call is invalid for the archive's current mode (writing an archive
    /// opened read-only, finishing twice, ...).
    InvalidState,
    /// An option value or combination the writer/reader refuses.
    InvalidOption,
    /// A stored CRC32 does not match the decoded bytes.
    CrcMismatch,
    /// A stored BLAKE2sp (or other file hash) does not match.
    HashMismatch,
    /// Encrypted content was reached without a usable password.
    Encrypted,
    /// The archive uses a valid RAR feature this crate does not implement.
    Unsupported,
    /// A security policy refused the operation (path escape, unsafe link,
    /// set-ID owner, ...).
    Security,
    /// A configured size, dictionary or resource limit was exceeded.
    LimitExceeded,
    /// `unique_entry` (or an extractor selector) found no such member.
    MemberNotFound,
    /// A name expected to identify one member matched several entries.
    AmbiguousMember,
    /// An [`EntryId`](crate::EntryId) came from an older catalog generation
    /// (see [`ArchiveEditor::apply`](crate::ArchiveEditor::apply)).
    StaleEntryId,
    /// The archive carries the lock bit and cannot be rewritten.
    ArchiveLocked,
    /// The caller's cancellation flag was signalled.
    Cancelled,
    /// Header-encrypted archive opened with the wrong password.
    WrongPassword,
    /// Underlying I/O failure.
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

/// Every failure this crate reports, with a structured payload where the
/// caller can act on it (compare a CRC, retry a password, name the member).
///
/// [`RarError::code`] maps a value to the stable, machine-readable
/// [`ErrorCode`] used by bindings, logs and exit-code mapping. The enum is
/// `#[non_exhaustive]`, so match with a trailing `_` arm.
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
        /// The CRC32 the archive stored for the member.
        expected: u32,
        /// The CRC32 computed from the decoded bytes.
        actual: u32,
        /// What was being decoded, for the error message.
        context: String,
    },
    /// BLAKE2sp (or other file hash) mismatch.
    HashMismatch {
        /// The hash the archive stored.
        expected: [u8; 32],
        /// The hash computed from the decoded bytes.
        actual: [u8; 32],
        /// What was being decoded, for the error message.
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
    MemberNotFound {
        /// The member name (or selector) that matched nothing.
        name: String,
    },
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
    /// (see [`crate::ArchiveWriter::set_cancel_flag`]).
    Cancelled,
    /// An encrypted archive was opened with the wrong password.
    WrongPassword,
    /// Underlying I/O error.
    Io(io::Error),
}

/// Constructor functions: the single place that builds a [`RarError`].
///
/// Every fallible path should build its error through one of these rather
/// than a variant literal, so message style stays uniform and a future
/// change (a family prefix, a context field) lands once. Match arms and
/// `matches!` keep using the variants directly — only *construction* goes
/// through here.
///
/// Message style: lowercase, no trailing period, and a container-family
/// prefix (`"RAR4: "`, `"RAR 1.3: "`, `"RAR5: "`, `"RARVM: "`) when the site
/// is family-specific. Interpolating messages use `format!`.
impl RarError {
    /// Malformed archive bytes: a header, block or field the format cannot
    /// produce.
    pub fn format(message: impl Into<String>) -> Self {
        Self::Format(message.into())
    }

    /// The call is invalid for the archive's current mode (writing a
    /// read-only archive, finishing twice, ...).
    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::InvalidState(message.into())
    }

    /// An option value or combination the writer/reader refuses.
    pub fn invalid_option(message: impl Into<String>) -> Self {
        Self::InvalidOption(message.into())
    }

    /// Encrypted content was reached without a usable password.
    pub fn encrypted(message: impl Into<String>) -> Self {
        Self::Encrypted(message.into())
    }

    /// The archive uses a valid RAR feature this crate does not implement.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }

    /// A security policy refused the operation (path escape, unsafe link,
    /// set-ID owner, ...).
    pub fn security(message: impl Into<String>) -> Self {
        Self::Security(message.into())
    }

    /// A stored CRC32 does not match the decoded bytes.
    pub fn crc(expected: u32, actual: u32, context: impl Into<String>) -> Self {
        Self::Crc {
            expected,
            actual,
            context: context.into(),
        }
    }

    /// A stored BLAKE2sp (or other file hash) does not match.
    pub fn hash_mismatch(expected: [u8; 32], actual: [u8; 32], context: impl Into<String>) -> Self {
        Self::HashMismatch {
            expected,
            actual,
            context: context.into(),
        }
    }

    /// A configured size, dictionary or resource limit was exceeded.
    pub fn limit_exceeded(limit: u64, context: impl Into<String>) -> Self {
        Self::LimitExceeded {
            limit,
            context: context.into(),
        }
    }

    /// `unique_entry` (or an extractor selector) found no such member.
    pub fn member_not_found(name: impl Into<String>) -> Self {
        Self::MemberNotFound { name: name.into() }
    }

    /// A name expected to identify one member matched several entries.
    pub fn ambiguous_member(name: impl Into<String>, matches: usize) -> Self {
        Self::AmbiguousMember {
            name: name.into(),
            matches,
        }
    }
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

/// Result type of every fallible entry point in this crate.
pub type RarResult<T> = Result<T, RarError>;

#[cfg(test)]
mod tests {
    use super::{ErrorCode, RarError};

    #[test]
    fn error_codes_are_stable_and_machine_readable() {
        let cases = [
            (RarError::Format(String::new()), ErrorCode::Format, "format"),
            (
                RarError::ambiguous_member("duplicate", 2),
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
