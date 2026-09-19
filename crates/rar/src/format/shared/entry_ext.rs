//! Format-side accessors on the public entry type.
//!
//! `ArchiveEntry` itself is format-neutral data and lives in
//! [`crate::engine`]. These two accessors are not: `redirect` reads an RAR5
//! extra record, and `has_mtime` switches on RAR5 file flags and the FILE_TIME
//! record. As inherent impls they stay public API wherever they are written.

use crate::engine::ArchiveEntry;
use crate::model::FileHeader;

/// Everything a redirect (link/copy) member carries: the type follows the
/// RAR5 convention (1 = Unix symlink, 2 = Windows symlink, 3 = Windows
/// junction, 4 = hardlink, 5 = file copy) and `target` is the referenced
/// member name.
///
/// Family-neutral vocabulary: the RAR5 header parser constructs it and the
/// shared extractor consumes it, so it lives here rather than in the RAR5
/// header module.
pub(crate) struct RedirectSpec {
    pub redir_type: u64,
    pub target: String,
}

impl ArchiveEntry {
    /// Redirection record `(type, target)` for link/copy members (RAR5
    /// `EXTRA_FILE_REDIRECT`); `None` for regular members.
    ///
    /// Types follow the RAR5 convention: 1 = Unix symlink, 2 = Windows
    /// symlink, 3 = Windows junction, 4 = hardlink, 5 = file copy.
    pub fn redirect(&self) -> Option<(u64, String)> {
        crate::format::rar5::headers::parse_redirect_record(&self.header.extra_data)
            .map(|spec| (spec.redir_type, spec.target))
    }

    /// Whether the member carries a usable modification time. RAR5 stores
    /// it in the header's `FILE_FLAG_TIME_UNIX` field or in a FILE_TIME
    /// extra record; regular members without either default to the Unix
    /// epoch (like WinRAR's own reader), while link redirects show
    /// `????-??-??`. The legacy formats always carry DOS time, where the
    /// all-zero value means "unknown".
    pub fn has_mtime(&self) -> bool {
        file_header_has_mtime(&self.header)
    }
}

/// Header-level view of [`ArchiveEntry::has_mtime`], for the decode paths
/// that only hold a [`FileHeader`].
pub(crate) fn file_header_has_mtime(header: &FileHeader) -> bool {
    match header.format_version {
        3 | 4 => header.mtime != 0,
        _ => {
            header.file_flags & crate::format::rar5::FILE_FLAG_TIME_UNIX != 0
                || header.mtime_ns.is_some()
                || header.mtime != 0
                || has_file_time_extra(&header.extra_data)
        }
    }
}

/// The redirect record for `entry`, when it is a link/copy member. The
/// shared extractor uses this instead of reaching into the RAR5 header
/// parser itself.
pub(crate) fn redirect_of(entry: &ArchiveEntry) -> Option<RedirectSpec> {
    crate::format::rar5::headers::parse_redirect_record(&entry.header.extra_data)
}

/// Whether the RAR5 extra area carries a FILE_TIME (HTIME) record. A record
/// with all-zero seconds still marks the time as explicitly stored (WinRAR
/// shows 1970 for it), unlike a member with no time information at all
/// (`-ts-`, which WinRAR renders as `????-??-??`).
fn has_file_time_extra(extra: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset < extra.len() {
        let Ok((size, n)) = crate::vint::decode_from_slice(extra, offset) else {
            return false;
        };
        offset += n;
        let Ok(size) = usize::try_from(size) else {
            return false;
        };
        let Some(end) = offset.checked_add(size) else {
            return false;
        };
        if end > extra.len() {
            return false;
        }
        let Ok((rec_type, _)) = crate::vint::decode_from_slice(extra, offset) else {
            return false;
        };
        if rec_type == crate::format::rar5::EXTRA_FILE_TIME {
            return true;
        }
        offset = end;
    }
    false
}
