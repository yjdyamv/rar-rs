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
    // A signature is not an archive: a random executable can contain the 8-byte
    // magic, and slicing there would write a bogus `.rar`. Validate that a real
    // main header follows before writing (WinRAR reports "Bad archive" here and
    // leaves no output).
    let payload = &input[sfx_offset..];
    if !looks_like_archive(payload) {
        return Err(format!(
            "{archive_path} is not an SFX archive (no valid RAR archive after the module)"
        )
        .into());
    }
    let base = [".exe", ".EXE", ".sfx", ".SFX"]
        .into_iter()
        .find_map(|suffix| archive_path.strip_suffix(suffix))
        .map(|b| b.to_string())
        .unwrap_or_else(|| format!("{archive_path}.plain"));
    let out_path = format!("{base}.rar");
    std::fs::write(&out_path, payload).map_err(|e| format!("write: {e}"))?;
    info!("Removed SFX module: {out_path}");
    Ok(())
}

/// Convert an archive to SFX (like `rar s`).
pub(crate) fn cmd_sfx(args: &SfxArgs, assume_yes: bool) -> CliResult<()> {
    let archive_path = &args.archive;
    let input = std::fs::read(archive_path).map_err(|e| format!("read: {e}"))?;
    if input.starts_with(rar_rs::detect::RAR13_SIGNATURE) {
        return Err(
            "SFX is not supported for RAR 1.3/1.4 archives (official tools only accept the DOS-era stub)"
                .into(),
        );
    }
    // `s` converts one standalone archive. A volume set needs `a -sfx`, which
    // puts the module on the first volume and keeps the `.partN.rar` chain;
    // wrapping only volume 1 here would leave a half-converted set on disk.
    if rar_rs::discover_volumes(std::path::Path::new(archive_path)).len() > 1 {
        return Err(format!(
            "{archive_path} is part of a volume set; create a self-extracting set with `a -sfx` instead of `s`"
        )
        .into());
    }

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
    let out_path = format!("{base}.{}", sfx_extension());
    // Never clobber an existing executable -- and `<base>.exe` is very likely
    // someone's program -- without `-y`, matching WinRAR's overwrite query.
    if !assume_yes && std::path::Path::new(&out_path).exists() {
        return Err(format!("{out_path} already exists (use -y to overwrite)").into());
    }
    // Official `rar s` re-serializes the embedded archive (shortest member size
    // fields, rebuilt quick-open catalog, no inline recovery record, recomputed
    // locator), so mirror it rather than prepending the module verbatim.
    let payload = rar_rs::wire::reserialize_for_sfx(&input);
    let mut out = Vec::with_capacity(module_bytes.len() + payload.len());
    out.extend_from_slice(&module_bytes);
    out.extend_from_slice(&payload);
    std::fs::write(&out_path, &out).map_err(|e| format!("write: {e}"))?;
    info!("Created {out_path}");
    Ok(())
}

/// Whether `payload` begins with a valid RAR main header (not merely the
/// signature), so `s-` refuses a non-SFX executable that happens to contain
/// the magic bytes. RAR5 is checked by parsing (and CRC-verifying) the first
/// block; RAR 1.5-4.x is checked by the main header's type and size field.
fn looks_like_archive(payload: &[u8]) -> bool {
    use std::io::{Cursor, Seek, SeekFrom};
    const RAR5: [u8; 8] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x01, 0x00];
    const RAR4: [u8; 7] = [0x52, 0x61, 0x72, 0x21, 0x1A, 0x07, 0x00];
    if payload.starts_with(&RAR5) {
        let mut cursor = Cursor::new(payload);
        if cursor.seek(SeekFrom::Start(8)).is_err() {
            return false;
        }
        // Block type 1 = main header, 4 = plaintext encryption header (the
        // first block of a header-encrypted archive). `read_block` checks the
        // header CRC, so random bytes after the signature are rejected.
        return matches!(
            rar_rs::wire::read_block(&mut cursor, None),
            Ok(Some(block)) if block.block_type == 1 || block.block_type == 4
        );
    }
    if payload.starts_with(&RAR4) {
        // Main header: HEAD_CRC(2) HEAD_TYPE(1)=0x73 HEAD_FLAGS(2) HEAD_SIZE(2).
        if payload.len() < 15 || payload[9] != 0x73 {
            return false;
        }
        let head_size = u16::from_le_bytes([payload[12], payload[13]]) as usize;
        return head_size >= 13 && head_size <= payload.len() - 7;
    }
    // RAR 1.3/1.4 (`RE~^`): `s` refuses to create these, but tolerate an
    // existing one rather than mangling it.
    payload.starts_with(rar_rs::detect::RAR13_SIGNATURE)
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
    tmp.push(format!(".sfxtmp-{}", std::process::id()));
    let tmp = std::path::PathBuf::from(tmp);
    let staged = std::fs::write(&tmp, &out).and_then(|()| std::fs::rename(&tmp, archive));
    if staged.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    staged.map_err(|e| format!("write: {e}"))?;
    Ok(())
}

/// The extension WinRAR gives an SFX archive on this platform: `.exe` on
/// Windows, `.sfx` elsewhere (official `rar s` on Linux writes `name.sfx`).
pub(crate) fn sfx_extension() -> &'static str {
    if cfg!(windows) { "exe" } else { "sfx" }
}

/// Locate an SFX module. Our own console stub (`rar-sfx[.exe]` next to the
/// running `rar`, or `$RAR_SFX_STUB`) is preferred so SFX creation needs no
/// WinRAR install; the official modules are a fallback (Windows console
/// `WinCon.SFX` before the GUI `Default.SFX`; Linux `default.sfx`).
pub(crate) fn find_sfx_module() -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(explicit) = std::env::var("RAR_SFX_STUB") {
        candidates.push(explicit);
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let name = if cfg!(windows) {
            "rar-sfx.exe"
        } else {
            "rar-sfx"
        };
        candidates.push(dir.join(name).to_string_lossy().into_owned());
    }
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(format!("{home}/default.sfx"));
    }
    candidates.push("/usr/lib/default.sfx".to_string());
    candidates.push("/usr/local/lib/default.sfx".to_string());
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RegCloseKey, RegOpenKeyW, RegQueryValueExW,
        };
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
                    candidates.push(format!("{dir}\\WinCon.SFX"));
                    candidates.push(format!("{dir}\\Default.SFX"));
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
            candidates.push(format!("{pf}\\WinRAR\\WinCon.SFX"));
            candidates.push(format!("{pf}\\WinRAR\\Default.SFX"));
        }
    }
    candidates
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
}
