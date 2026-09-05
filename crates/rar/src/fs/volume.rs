//! Volume naming policy: base extraction, part-number path builders and
//! the `.partN.rar` name parser shared by archive discovery, creation and
//! the recovery-volume machinery.

use std::path::{Path, PathBuf};

/// Extract the volume base and the zero-padding width of the part number
/// from a name like `archive.part3.rar` → `("archive", 1)` or
/// `archive.part03.rar` → `("archive", 2)`. WinRAR pads the number to the
/// digit count of the total volume count (part01..part15), so both forms
/// must be discoverable.
pub(crate) fn extract_volume_base(name: &str) -> Option<(String, usize)> {
    // Case-insensitive match for .partN.rar
    let lower = name.to_lowercase();
    if let Some(idx) = lower.find(".part") {
        let after = &lower[idx + 5..];
        if let Some(rar_idx) = after.find(".rar") {
            let num_str = &after[..rar_idx];
            if num_str.chars().all(|c| c.is_ascii_digit()) && !num_str.is_empty() {
                return Some((name[..idx].to_string(), num_str.len()));
            }
        }
    }
    None
}

/// Volume base of an archive path, stripping `.partN.rar` or `.rar`
/// suffixes (used by the recovery-volume machinery).
pub(crate) fn volume_base_of(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive");
    if let Some((base, _)) = extract_volume_base(name) {
        return base;
    }
    if let Some(stem) = name.strip_suffix(".rar") {
        return stem.to_string();
    }
    if let Some(stem) = name.strip_suffix(".RAR") {
        return stem.to_string();
    }
    name.to_string()
}

/// Zero-padding width of the part number in a volume name
/// (`archive.part03.rar` → 2, `archive.part3.rar` → 1). Used to name
/// `.rev` files identically to their volume set.
pub(crate) fn volume_part_width(path: &Path) -> usize {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(extract_volume_base)
        .map(|(_, w)| w)
        .unwrap_or(1)
}

pub(crate) fn volume_path(parent: &Path, base: &str, part_num: usize) -> PathBuf {
    parent.join(format!("{base}.part{part_num}.rar"))
}

/// Legacy RAR 1.5–3.x volume name: the first volume is `{base}.rar`, then
/// `{base}.r00`, `{base}.r01`, … `.r99`, then `{base}.s00`, … — one extension
/// letter per hundred volumes, matching WinRAR's RAR4 multi-volume naming.
pub(crate) fn volume_path_rar4(parent: &Path, base: &str, part_num: usize) -> PathBuf {
    if part_num <= 1 {
        return parent.join(format!("{base}.rar"));
    }
    let idx = part_num - 2;
    let letter = b'r' + (idx / 100) as u8;
    let num = idx % 100;
    parent.join(format!("{base}.{}{:02}", letter as char, num))
}

/// Legacy volume base from `x.rar` / `x.r00` / `x.s37` (case-insensitive),
/// the inverse of [`volume_path_rar4`].
pub(crate) fn legacy_volume_base(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    if let Some(base) = lower.strip_suffix(".rar") {
        return Some(name[..base.len()].to_string());
    }
    let bytes = lower.as_bytes();
    if bytes.len() >= 5
        && bytes[bytes.len() - 4] == b'.'
        && bytes[bytes.len() - 3].is_ascii_lowercase()
        && bytes[bytes.len() - 3] >= b'r'
        && bytes[bytes.len() - 3] <= b'z'
        && bytes[bytes.len() - 2].is_ascii_digit()
        && bytes[bytes.len() - 1].is_ascii_digit()
    {
        let end = bytes.len() - 4;
        return Some(name[..end].to_string());
    }
    None
}

/// Volume path with the part number zero-padded to `width` digits
/// (`part01.rar` for width 2), matching WinRAR's naming for sets of 10
/// or more volumes.
pub(crate) fn volume_path_padded(
    parent: &Path,
    base: &str,
    part_num: usize,
    width: usize,
) -> PathBuf {
    parent.join(format!("{base}.part{part_num:0width$}.rar"))
}
