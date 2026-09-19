//! Opening and signature verification, dispatched per family.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::detect::RAR5_SIGNATURE;
use crate::detect::{ArchiveFamily, SFX_SCAN_LIMIT};
use crate::engine::Engine;
use crate::engine::discover_volumes;
use crate::error::{RarError, RarResult};

/// Open the archive and scan its catalog (every volume).
pub(crate) fn open_read(cx: &mut dyn Engine) -> RarResult<()> {
    open_common(cx)?;
    match cx.family() {
        ArchiveFamily::Rar13 => crate::format::rar13::extract::open_read_rar13(cx)?,
        ArchiveFamily::Rar15To40 => crate::format::rar4::extract::open_read_rar4(cx)?,
        ArchiveFamily::Rar50Plus => crate::format::rar5::extract::open::open_read_rar5(cx)?,
    }
    cx.reset_catalog_token()?;
    Ok(())
}

/// Open without a full block scan where the family supports a quick
/// catalog; families without one fall back to their full scan.
pub(crate) fn open_read_quick(cx: &mut dyn Engine) -> RarResult<()> {
    open_common(cx)?;
    match cx.family() {
        ArchiveFamily::Rar13 => crate::format::rar13::extract::open_read_rar13(cx)?,
        ArchiveFamily::Rar15To40 => crate::format::rar4::extract::open_read_rar4(cx)?,
        ArchiveFamily::Rar50Plus => crate::format::rar5::extract::open::open_read_quick_rar5(cx)?,
    }
    cx.reset_catalog_token()?;
    Ok(())
}

/// Discover the volume set, open the primary stream and verify the
/// signature (setting the container family and SFX offset).
fn open_common(cx: &mut dyn Engine) -> RarResult<()> {
    let paths = discover_volumes(cx.path());
    cx.set_volume_paths(paths);
    let primary = cx.volume_paths()[0].clone();
    cx.set_stream(Box::new(File::open(&primary)?));
    verify_signature(cx)
}

fn verify_signature(cx: &mut dyn Engine) -> RarResult<()> {
    // The signature must appear at the start for plain archives and
    // after the embedded stub for SFX archives (scan up to 8 MiB,
    // like the reference readers).
    let buf = {
        let stream = cx.stream_mut()?;
        let file_size = stream.seek(SeekFrom::End(0))?;
        stream.seek(SeekFrom::Start(0))?;
        let scan = file_size.min(SFX_SCAN_LIMIT as u64) as usize;
        let mut buf = vec![0u8; scan];
        let n = stream.read(&mut buf)?;
        buf.truncate(n);
        buf
    };
    let (family, sfx_offset) = crate::detect::find_archive_start(&buf, SFX_SCAN_LIMIT)
        .ok_or_else(|| RarError::Format("not a RAR archive (signature not found)".into()))?;
    cx.set_sfx_offset(sfx_offset as u64);
    cx.set_family(family);
    let sig_len = match family {
        ArchiveFamily::Rar50Plus => RAR5_SIGNATURE.len() as u64,
        ArchiveFamily::Rar15To40 => crate::detect::RAR4_SIGNATURE.len() as u64,
        ArchiveFamily::Rar13 => crate::detect::RAR13_SIGNATURE.len() as u64,
    };
    let start = cx.sfx_offset() + sig_len;
    cx.stream_mut()?.seek(SeekFrom::Start(start))?;
    Ok(())
}
