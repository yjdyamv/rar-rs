//! `@listfile` handling shared by the `rar` and `unrar` binaries.
//!
//! A parameter starting with `@` names a plain text file whose lines are
//! file/member names; `@` alone reads the list from stdin. `//` starts a
//! comment, and `-@` disables list processing (`-@+` re-enables it), like
//! WinRAR. `-sc<charset>l` is accepted but decoding is UTF-8 with a
//! Latin-1 fallback; UTF-16 lists are detected by their byte order mark.

use std::io::Read;

/// Whether `@listfile` processing is enabled (`-@` disables it, `-@+`
/// re-enables it; the last switch wins).
pub fn enabled(spec: Option<&str>) -> bool {
    !matches!(spec, Some(""))
}

/// Expand `@listfile` arguments into their lines; every other argument is
/// passed through unchanged.
pub fn expand(specs: &[String], list_files: Option<&str>) -> Result<Vec<String>, String> {
    if !enabled(list_files) {
        return Ok(specs.to_vec());
    }
    let mut out = Vec::new();
    for spec in specs {
        let Some(path) = spec.strip_prefix('@') else {
            out.push(spec.clone());
            continue;
        };
        let bytes = if path.is_empty() {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .map_err(|error| format!("read stdin: {error}"))?;
            buf
        } else {
            std::fs::read(path).map_err(|error| format!("read list file {path}: {error}"))?
        };
        for line in decode(&bytes).lines() {
            let name = line.split("//").next().unwrap_or_default().trim_end();
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
    }
    Ok(out)
}

/// Decode a list file: UTF-16 by BOM, otherwise UTF-8 with a byte-preserving
/// Latin-1 fallback for legacy single-byte encodings.
fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_be_bytes(*pair))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|&byte| byte as char).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, enabled, expand};

    #[test]
    fn listfiles_expand_with_comments_and_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("files.lst");
        std::fs::write(
            &list,
            b"a.txt\r\nb.txt // keep b\r\n// whole line comment\r\n\r\nc dir/\r\n",
        )
        .unwrap();
        let expanded = expand(
            &[format!("@{}", list.display()), "plain.txt".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(expanded, ["a.txt", "b.txt", "c dir/", "plain.txt"]);

        // `-@` disables list processing entirely.
        let raw = expand(&[format!("@{}", list.display())], Some("")).unwrap();
        assert_eq!(raw.len(), 1);
        assert!(raw[0].starts_with('@'));
        assert!(!enabled(Some("")));
        assert!(enabled(Some("+")));
    }

    #[test]
    fn utf16_bom_is_detected() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "a.txt\r\nb.txt".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode(&bytes), "a.txt\r\nb.txt");
        // Latin-1 fallback preserves non-UTF-8 bytes.
        assert_eq!(decode(&[0xE9, b'.', b't']), "é.t");
    }
}
