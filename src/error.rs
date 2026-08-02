/// Error types for RAR5 archive operations.
use std::fmt;
use std::io;

#[derive(Debug)]
pub enum RarError {
    /// Invalid or unexpected archive format.
    Format(String),
    /// CRC32 checksum mismatch.
    Crc {
        expected: u32,
        actual: u32,
        context: String,
    },
    /// Encrypted content encountered without a password.
    Encrypted(String),
    /// Valid RAR5 feature not yet implemented.
    Unsupported(String),
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
            RarError::Encrypted(msg) => write!(f, "encrypted: {msg}"),
            RarError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            RarError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
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
