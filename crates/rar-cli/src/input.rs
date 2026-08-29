//! Solid-order input list (rarfiles.lst) shared by the `rar` and`n//! `unrar` binaries.

#[allow(dead_code)] // used by the `rar` binary only
pub fn read_rarfiles_lst() -> Vec<Option<String>> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    // Next to the executable, like the official rar/winrar lookup
    // (Windows and Unix alike; the test writes it next to the binary).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("rarfiles.lst"));
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            candidates.push(
                std::path::PathBuf::from(appdata)
                    .join("WinRAR")
                    .join("rarfiles.lst"),
            );
        }
    }
    #[cfg(unix)]
    {
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(std::path::PathBuf::from(home).join("rarfiles.lst"));
        }
        candidates.push(std::path::PathBuf::from("/etc/rarfiles.lst"));
    }
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.split(';').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            out.push(if line == "$default" {
                None
            } else {
                Some(line.to_string())
            });
        }
        if !out.is_empty() {
            return out;
        }
    }
    Vec::new()
}
