/// Error types for RAR5 archive operations.
use std::fmt;
use std::io;

#[derive(Debug)]
#[non_exhaustive]
pub enum RarError {
    /// Invalid or unexpected archive format.
    Format(String),
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
    /// Valid RAR5 feature not yet implemented.
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
    /// Underlying I/O error.
    Io(io::Error),
}

impl fmt::Display for RarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RarError::Format(msg) => write!(f, "RAR format error: {msg}"),
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
