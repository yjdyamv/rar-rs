//! SFX conversion and the `default.sfx` module lookup.

use crate::args::{ArchiveArgs, SfxArgs};
use crate::error::CliResult;
use crate::info;
/// Convert an archive to or from SFX (like `rar s` / `rar s-`).
pub(crate) fn cmd_sfx_strip(args: &ArchiveArgs) -> CliResult<()> {
    let archive_path = &args.archive;
    let input = std::fs::read(archive_path).map_err(|e| format!("read: {e}"))?;
    let sfx_offset = rar_rs::sfx_offset_of(&input)
        .ok_or_else(|| format!("{archive_path} is not an SFX archive"))?;
    let base = archive_path
        .strip_suffix(".sfx")
        .or_else(|| archive_path.strip_suffix(".SFX"))
        .map(|b| b.to_string())
        .unwrap_or_else(|| format!("{archive_path}.plain"));
    let out_path = format!("{base}.rar");
    std::fs::write(&out_path, &input[sfx_offset..]).map_err(|e| format!("write: {e}"))?;
    info!("Removed SFX module: {out_path}");
    Ok(())
}

/// Convert an archive to SFX (like `rar s`).
pub(crate) fn cmd_sfx(args: &SfxArgs) -> CliResult<()> {
    let archive_path = &args.archive;
    let input = std::fs::read(archive_path).map_err(|e| format!("read: {e}"))?;

    // Creation: prepend the SFX module.
    let module_path = match &args.module {
        Some(m) => m.clone(),
        None => find_sfx_module()
            .ok_or_else(|| "default.sfx not found (use -sfx<module>)".to_string())?,
    };
    let module_bytes = std::fs::read(&module_path).map_err(|e| format!("read module: {e}"))?;
    let base = std::path::Path::new(archive_path)
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_string());
    let out_path = format!("{base}.sfx");
    let mut out = Vec::with_capacity(module_bytes.len() + input.len());
    out.extend_from_slice(&module_bytes);
    out.extend_from_slice(&input);
    std::fs::write(&out_path, &out).map_err(|e| format!("write: {e}"))?;
    info!("Created {out_path}");
    Ok(())
}

/// Prepend the SFX module to an existing archive *in place* (create-time
/// `-sfx[name]`). Idempotent for archives that already carry a module.
pub(crate) fn prepend_module_in_place(
    archive: &std::path::Path,
    module: Option<&str>,
) -> CliResult<()> {
    let input = std::fs::read(archive).map_err(|e| format!("read: {e}"))?;
    let module_path = match module {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => find_sfx_module()
            .ok_or_else(|| "default.sfx not found (use -sfx<module>)".to_string())?,
    };
    let module_bytes = std::fs::read(&module_path).map_err(|e| format!("read module: {e}"))?;
    let payload = &input[rar_rs::sfx_offset_of(&input).unwrap_or(0)..];
    let mut out = Vec::with_capacity(module_bytes.len() + payload.len());
    out.extend_from_slice(&module_bytes);
    out.extend_from_slice(payload);
    let mut tmp = archive.as_os_str().to_owned();
    tmp.push(".sfxtmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, &out).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, archive).map_err(|e| format!("replace: {e}"))?;
    Ok(())
}

/// Locate a `default.sfx` module: `$HOME`, `/usr/lib`, `/usr/local/lib`,
/// or the installed WinRAR directory (Windows: `%ProgramFiles%\WinRAR`,
/// `%ProgramFiles(x86)%\WinRAR`, or the registry-installed path).
pub(crate) fn find_sfx_module() -> Option<String> {
    #[cfg_attr(not(windows), allow(unused_mut))] // mut only for the Windows registry candidates
    let mut candidates: Vec<Option<String>> = vec![
        std::env::var("HOME")
            .ok()
            .map(|h| format!("{h}/default.sfx")),
        Some("/usr/lib/default.sfx".to_string()),
        Some("/usr/local/lib/default.sfx".to_string()),
    ];
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RegCloseKey, RegOpenKeyW, RegQueryValueExW,
        };
        let mut reg_paths: Vec<String> = Vec::new();
        for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
            let mut hkey = std::ptr::null_mut();
            let key: Vec<u16> = "Software\\WinRAR"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let status = unsafe { RegOpenKeyW(root, key.as_ptr(), &mut hkey) };
            if status == 0 {
                let mut buf = [0u16; 1024];
                let mut size = (buf.len() * 2) as u32;
                let value: Vec<u16> = "exe32".encode_utf16().chain(std::iter::once(0)).collect();
                let status = unsafe {
                    RegQueryValueExW(
                        hkey,
                        value.as_ptr(),
                        std::ptr::null(),
                        std::ptr::null_mut(),
                        buf.as_mut_ptr() as *mut u8,
                        &mut size,
                    )
                };
                if status == 0 {
                    let len = (size / 2) as usize;
                    let dir = String::from_utf16_lossy(&buf[..len.min(buf.len())]);
                    reg_paths.push(format!("{dir}\\Default.SFX"));
                    reg_paths.push(format!("{dir}\\WinCon.SFX"));
                }
                unsafe { RegCloseKey(hkey) };
            }
        }
        for pf in [
            std::env::var("ProgramFiles").ok(),
            std::env::var("ProgramFiles(x86)").ok(),
        ]
        .into_iter()
        .flatten()
        {
            reg_paths.push(format!("{pf}\\WinRAR\\Default.SFX"));
            reg_paths.push(format!("{pf}\\WinRAR\\WinCon.SFX"));
        }
        candidates.extend(reg_paths.into_iter().map(Some));
    }
    candidates
        .into_iter()
        .flatten()
        .find(|p| std::path::Path::new(p).exists())
}
