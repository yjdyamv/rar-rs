//! The commit journal's on-disk protocol: the versioned, tab-separated record
//! format, field escaping (raw bytes on Unix, so a non-UTF-8 name round-trips
//! exactly) and the hidden backup/park sibling names.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::error::RarResult;

/// Hidden sibling used to park a destination file during [`commit_files`].
pub(super) fn backup_sibling_path(dest: &Path, suffix: &str) -> PathBuf {
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "entry".to_string());
    dest.with_file_name(format!(".{file_name}.rar5bak-{suffix}"))
}

/// Move `src` onto `dest`, replacing an existing destination. Only the
/// rollback path uses this, where discarding the newer bytes is the point,
/// so a plain rename plus a remove-retry is correct.
pub(super) fn restore_file(src: &Path, dest: &Path) -> io::Result<()> {
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(_) if dest.exists() => {
            fs::remove_file(dest)?;
            fs::rename(src, dest)
        }
        Err(error) => Err(error),
    }
}

/// Journal file recording an in-flight multi-file commit, next to the archive
/// being replaced. One per base name: writes to the same archive are
/// serialized by the caller.
pub(super) fn journal_path(parent: &Path, base: &str) -> PathBuf {
    parent.join(format!(".{base}.rar5commit.journal"))
}

/// Marker written after every staged file is installed, before the backups
/// are dropped. Its presence tells recovery the new set won.
pub(super) fn commit_done_path(parent: &Path, base: &str) -> PathBuf {
    parent.join(format!(".{base}.rar5commit.done"))
}

/// Journal format for the escaped layout with explicit `park` records.
/// Versioned so a foreign or truncated journal is never misread as a commit
/// plan, and so a reader that does not know a record kind keeps the journal
/// instead of guessing.
pub(super) const COMMIT_JOURNAL_VERSION: &str = "rar5commit v3";
/// Escaped layout without `park` records, written by earlier builds; still
/// parsed so an in-flight journal survives an upgrade.
pub(super) const COMMIT_JOURNAL_VERSION_V2: &str = "rar5commit v2";
/// Header of the unescaped format written by earlier builds; still parsed
/// (fields taken verbatim) so an in-flight journal survives an upgrade.
pub(super) const COMMIT_JOURNAL_VERSION_V1: &str = "rar5commit v1";

/// Escape one journal field so the tab-separated, one-record-per-line format
/// stays unambiguous: `\`, `\t` and `\n` get a backslash form, and every
/// other control character is written as `\r` or `\xNN`. Control characters
/// are legal on Unix, so they are escaped rather than rejected (rejecting
/// them here failed a whole multi-volume commit after staging).
#[cfg(not(unix))]
pub(super) fn escape_journal_field(name: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                out.push_str("\\x");
                out.push(HEX[((c as u32 >> 4) & 0xF) as usize] as char);
                out.push(HEX[(c as u32 & 0xF) as usize] as char);
            }
            c => out.push(c),
        }
    }
    out
}

/// Inverse of [`escape_journal_field`]; `None` marks a malformed field (an
/// unknown escape, a truncated `\xNN`, or a raw control character).
#[cfg(not(unix))]
pub(super) fn unescape_journal_field(field: &str) -> Option<String> {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next()? {
                '\\' => out.push('\\'),
                't' => out.push('\t'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                'x' => {
                    let hi = chars.next()?.to_digit(16)?;
                    let lo = chars.next()?.to_digit(16)?;
                    out.push(char::from_u32(hi * 16 + lo)?);
                }
                _ => return None,
            },
            c if c.is_control() => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

/// Escape one file name into a journal field. On Unix the raw `OsStr` bytes
/// are escaped (`\xNN` for every byte >= 0x80 or control byte), so a name
/// that is not valid UTF-8 round-trips exactly; elsewhere the lossy spelling
/// is escaped as before.
#[cfg(unix)]
fn escape_journal_name(name: &OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;
    escape_journal_bytes(name.as_bytes())
}

#[cfg(not(unix))]
fn escape_journal_name(name: &OsStr) -> String {
    escape_journal_field(&name.to_string_lossy())
}

/// Inverse of [`escape_journal_name`], rebuilding the raw name.
#[cfg(unix)]
pub(super) fn unescape_journal_name(field: &str) -> Option<OsString> {
    use std::os::unix::ffi::OsStringExt;
    unescape_journal_bytes(field.as_bytes()).map(OsString::from_vec)
}

#[cfg(not(unix))]
pub(super) fn unescape_journal_name(field: &str) -> Option<OsString> {
    unescape_journal_field(field).map(OsString::from)
}

/// Byte-level field escaping: `\`, `\t`, `\n` and `\r` keep their backslash
/// forms, printable ASCII stays literal, and every other byte (a control
/// byte or one above 0x7F) becomes `\xNN`. The escape is one byte per input
/// byte, so a non-UTF-8 name is not rewritten to U+FFFD like
/// `to_string_lossy` would.
#[cfg(unix)]
fn escape_journal_bytes(name: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(name.len());
    for &byte in name {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b if b.is_ascii_graphic() || b == b' ' => out.push(b as char),
            b => {
                out.push_str("\\x");
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0xF) as usize] as char);
            }
        }
    }
    out
}

/// Inverse of [`escape_journal_bytes`]: one output byte per literal byte and
/// per `\xNN`. `None` marks a malformed field (an unknown escape, a
/// truncated `\xNN`, or a raw control byte).
#[cfg(unix)]
fn unescape_journal_bytes(field: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        let byte = field[index];
        index += 1;
        match byte {
            b'\\' => {
                let escape = *field.get(index)?;
                index += 1;
                match escape {
                    b'\\' => out.push(b'\\'),
                    b't' => out.push(b'\t'),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b'x' => {
                        let hi = hex_digit(*field.get(index)?)?;
                        let lo = hex_digit(*field.get(index + 1)?)?;
                        index += 2;
                        out.push(hi * 16 + lo);
                    }
                    _ => return None,
                }
            }
            b if b.is_ascii_control() => return None,
            b => out.push(b),
        }
    }
    Some(out)
}

#[cfg(unix)]
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A journal record may only name a plain sibling of the journal's
/// directory. Reject separators, `.`/`..`, NUL and Windows drive-relative
/// names (`C:evil`), all of which would make `parent.join(name)` leave
/// `parent` — a planted journal must never become a file primitive outside
/// the archive's own directory. A backslash is a legal filename character on
/// Unix (v2 escapes it, so records stay unambiguous) and only Windows treats
/// it as a separator. On Unix the check runs on the raw bytes, so a
/// non-UTF-8 name is validated without a lossy round trip.
pub(super) fn plain_journal_name(name: &OsStr) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = name.as_bytes();
        let plain_bytes = !bytes.is_empty()
            && bytes != b"."
            && bytes != b".."
            && !bytes.contains(&0)
            && !bytes.contains(&b'/');
        let mut components = Path::new(name).components();
        plain_bytes
            && matches!(
                (components.next(), components.next()),
                (Some(Component::Normal(part)), None) if part == name
            )
    }
    #[cfg(not(unix))]
    {
        let Some(name) = name.to_str() else {
            return false;
        };
        if name.is_empty() || name == "." || name == ".." || name.contains('\0') {
            return false;
        }
        if name.contains('/') {
            return false;
        }
        #[cfg(windows)]
        if name.contains('\\') {
            return false;
        }
        let mut components = Path::new(name).components();
        matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(part)), None) if part == OsStr::new(name)
        )
    }
}

/// Serialize the commit plan: one record per file — `backup` (hidden, dropped
/// on success), `park` (kept on success) or `install` — with names relative to
/// `parent` and escaped. Written before anything moves, via a sibling rename
/// so a torn write cannot leave a half journal for recovery to misread.
pub(super) fn write_commit_journal(
    parent: &Path,
    base: &str,
    backups: &[(PathBuf, PathBuf)],
    install: &[(PathBuf, PathBuf)],
    parks: &[(PathBuf, PathBuf)],
) -> RarResult<()> {
    let mut text = String::from(COMMIT_JOURNAL_VERSION);
    text.push('\n');
    let mut push = |kind: &str, from: &Path, to: &Path| {
        if let (Some(from), Some(to)) = (from.file_name(), to.file_name()) {
            let from = escape_journal_name(from);
            let to = escape_journal_name(to);
            text.push_str(kind);
            text.push('\t');
            text.push_str(&from);
            text.push('\t');
            text.push_str(&to);
            text.push('\n');
        }
    };
    for (backup, final_path) in backups {
        push("backup", backup, final_path);
    }
    for (parked, final_path) in parks {
        push("park", parked, final_path);
    }
    for (staged, final_path) in install {
        push("install", staged, final_path);
    }
    let path = journal_path(parent, base);
    let tmp = path.with_extension("journal.tmp");
    fs::write(&tmp, text.as_bytes())?;
    fs::rename(&tmp, &path)?;
    Ok(())
}
