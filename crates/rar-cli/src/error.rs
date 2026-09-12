//! CLI error type carrying the process exit code.
//!
//! Both binaries print the message and exit with the code from the library's
//! [`rar_rs::ErrorCode`] where one is available, so scripts can distinguish a
//! wrong password from a CRC failure or a locked archive instead of seeing
//! every failure as exit 1.

// The exit-code table below is the documented WinRAR contract; several codes
// are not emitted yet (no path distinguishes them), so the unused ones are
// kept for reference rather than deleted.
#![allow(dead_code)]

use std::fmt;

use rar_rs::{ErrorCode, RarError};

/// WinRAR-compatible process exit codes.
pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_WARNING: i32 = 1;
pub const EXIT_FATAL: i32 = 2;
pub const EXIT_CRC: i32 = 3;
pub const EXIT_LOCKED: i32 = 4;
pub const EXIT_WRITE: i32 = 5;
pub const EXIT_OPEN: i32 = 6;
pub const EXIT_BAD_COMMAND: i32 = 7;
pub const EXIT_MEMORY: i32 = 8;
pub const EXIT_CREATE: i32 = 9;
pub const EXIT_NO_FILES: i32 = 10;
pub const EXIT_WRONG_PASSWORD: i32 = 11;
pub const EXIT_USER_BREAK: i32 = 255;

/// A user-facing failure together with the process exit code it maps to.
#[derive(Debug)]
pub struct CliError {
    message: String,
    exit_code: i32,
}

pub type CliResult<T> = Result<T, CliError>;

impl CliError {
    /// A generic fatal error (exit 2), the fallback for unclassified failures.
    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_FATAL,
        }
    }

    /// An error with an explicit exit code.
    pub fn with_code(message: impl Into<String>, exit_code: i32) -> Self {
        Self {
            message: message.into(),
            exit_code,
        }
    }

    /// Prefix the message with operation context while keeping the code, e.g.
    /// `extract a.bin: <mismatch>`.
    #[must_use]
    pub fn context(mut self, context: impl AsRef<str>) -> Self {
        self.message = format!("{}: {}", context.as_ref(), self.message);
        self
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        Self::fatal(message)
    }
}

impl From<&str> for CliError {
    fn from(message: &str) -> Self {
        Self::fatal(message.to_string())
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self::fatal(error.to_string())
    }
}

impl From<RarError> for CliError {
    fn from(error: RarError) -> Self {
        Self {
            exit_code: exit_code_for(error.code()),
            message: error.to_string(),
        }
    }
}

/// Map a library error category onto the WinRAR-compatible exit code.
pub fn exit_code_for(code: ErrorCode) -> i32 {
    match code {
        ErrorCode::WrongPassword | ErrorCode::Encrypted => EXIT_WRONG_PASSWORD,
        ErrorCode::CrcMismatch | ErrorCode::HashMismatch => EXIT_CRC,
        ErrorCode::ArchiveLocked => EXIT_LOCKED,
        ErrorCode::Cancelled => EXIT_USER_BREAK,
        ErrorCode::InvalidOption => EXIT_BAD_COMMAND,
        ErrorCode::MemberNotFound | ErrorCode::AmbiguousMember => EXIT_NO_FILES,
        ErrorCode::LimitExceeded => EXIT_MEMORY,
        ErrorCode::Format
        | ErrorCode::InvalidState
        | ErrorCode::Unsupported
        | ErrorCode::Security
        | ErrorCode::StaleEntryId
        | ErrorCode::Io => EXIT_FATAL,
        // `ErrorCode` is `#[non_exhaustive]`: future categories fall back to a
        // generic fatal error until they get a dedicated code here.
        _ => EXIT_FATAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_categories_map_to_distinct_exit_codes() {
        assert_eq!(CliError::from(RarError::WrongPassword).exit_code(), 11);
        assert_eq!(
            CliError::from(RarError::Crc {
                expected: 1,
                actual: 2,
                context: "x".into(),
            })
            .exit_code(),
            3
        );
        assert_eq!(CliError::from(RarError::ArchiveLocked).exit_code(), 4);
        assert_eq!(CliError::from(RarError::Cancelled).exit_code(), 255);
        assert_eq!(
            CliError::from(RarError::InvalidOption("x".into())).exit_code(),
            7
        );
        assert_eq!(
            CliError::from(RarError::MemberNotFound { name: "x".into() }).exit_code(),
            10
        );
        assert_eq!(CliError::from(RarError::Format("x".into())).exit_code(), 2);
    }

    #[test]
    fn context_keeps_the_code_and_prefixes_the_message() {
        let error = CliError::from(RarError::WrongPassword).context("extract a.bin");
        assert_eq!(error.exit_code(), 11);
        assert!(error.message().starts_with("extract a.bin: "));
    }
}
