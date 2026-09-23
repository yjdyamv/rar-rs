//! Platform metadata style for RAR5 members.
//!
//! WinRAR stores the metadata of the host it runs on, and its readers
//! interpret the member header through that marker: a Windows archive carries
//! `host_os = 0`, DOS attribute bits and Windows FILETIME in the FILE_TIME
//! record; a Unix archive carries `host_os = 1`, the raw `st_mode` and Unix
//! seconds. `model::FileHeader::host_attributes` reads the attribute field
//! through exactly this marker, and a Windows `UnRAR` derives the restored
//! attributes (and its name handling) from it too.
//!
//! So matching WinRAR means writing the running platform's style rather than a
//! single fixed one: on Windows a read-only/hidden/system file keeps its bits
//! across extraction instead of flattening to `ARCHIVE`, and WinRAR stops
//! NFC-composing our decomposed member names. This module is the one place
//! that decides those values; `engine` may name it (it is a leaf, like
//! `time`), which is why it lives at the crate root and not under `format`.

/// DOS attribute bits WinRAR stores in the RAR5 attribute field for a
/// Windows-host member (the subset of `FILE_ATTRIBUTE_*` that survives the
/// round trip; `FILE_ATTRIBUTE_NORMAL` is not stored).
#[cfg(windows)]
const STORED_DOS_ATTRIBUTES: u32 = 0x0001 // READONLY
    | 0x0002 // HIDDEN
    | 0x0004 // SYSTEM
    | 0x0010 // DIRECTORY
    | 0x0020 // ARCHIVE
    | 0x0400; // REPARSE_POINT

/// RAR5 `host_os` for the running platform: `0` = Windows, `1` = Unix, like
/// WinRAR. Readers interpret the attribute field and the time form through it.
pub(crate) const fn host_os() -> u64 {
    if cfg!(windows) { 0 } else { 1 }
}

/// Whether member times go into the FILE_TIME record as Windows FILETIME
/// (100 ns ticks since 1601) with the record's Unix-format bit clear, instead
/// of Unix seconds. WinRAR does this on Windows whenever it is not truncating
/// to whole seconds (`-ts1`), and clears `FILE_FLAG_TIME_UNIX` on the header
/// so the 4-byte Unix mtime field is absent.
pub(crate) const fn file_time_is_windows() -> bool {
    cfg!(windows)
}

/// Stored attributes for a regular file.
pub(crate) fn file_attributes(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mode() as u64
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        u64::from(meta.file_attributes() & STORED_DOS_ATTRIBUTES)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = meta;
        0o100644
    }
}

/// Stored attributes for a directory.
pub(crate) fn directory_attributes(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mode() as u64
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        u64::from(meta.file_attributes() & STORED_DOS_ATTRIBUTES)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = meta;
        0o040755
    }
}

/// Stored attributes for a member written without filesystem metadata (a
/// `-si` stdin member or an in-memory `add_bytes`).
pub(crate) const fn memory_attributes() -> u64 {
    if cfg!(windows) { 0x0020 } else { 0o100644 }
}

/// Stored attributes for a redirect member, by its redirect type (1 = Unix
/// symlink, 2 = Windows symlink, 3 = junction, 4 = hardlink, 5 = file copy).
/// WinRAR on Windows marks a *link* as a reparse point (directory for a
/// junction) but stores a hardlink or file copy as a plain archive-only file;
/// elsewhere the attribute field is left at the model default (redirects carry
/// no data and Unix readers ignore the mode).
pub(crate) const fn redirect_attributes(redir_type: u64) -> u64 {
    #[cfg(windows)]
    {
        match redir_type {
            3 => 0x0410,     // junction: reparse point | directory
            1 | 2 => 0x0420, // symlink: reparse point | archive
            _ => 0x0020,     // hardlink / file copy: a plain archive-only file
        }
    }
    #[cfg(not(windows))]
    {
        let _ = redir_type;
        0o100644
    }
}
