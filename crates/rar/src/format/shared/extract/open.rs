//! Opening and signature verification, dispatched per family.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::archive::RarArchive;
use crate::detect::{ArchiveFamily, SFX_SCAN_LIMIT};
use crate::engine::discover_volumes;
use crate::error::{RarError, RarResult};
use crate::format::rar5::RAR5_SIGNATURE;
use crate::format::shared::stream_mut;

impl RarArchive {
    /// Open the archive and scan its catalog (every volume).
    pub(crate) fn open_read(&mut self) -> RarResult<()> {
        self.open_common()?;
        match self.family {
            ArchiveFamily::Rar13 => crate::format::rar13::extract::open_read_rar13(self)?,
            ArchiveFamily::Rar15To40 => self.open_read_rar4()?,
            ArchiveFamily::Rar50Plus => self.open_read_rar5()?,
        }
        self.reset_catalog_token()?;
        Ok(())
    }

    /// Open without a full block scan where the family supports a quick
    /// catalog; families without one fall back to their full scan.
    pub(crate) fn open_read_quick(&mut self) -> RarResult<()> {
        self.open_common()?;
        match self.family {
            ArchiveFamily::Rar13 => crate::format::rar13::extract::open_read_rar13(self)?,
            ArchiveFamily::Rar15To40 => self.open_read_rar4()?,
            ArchiveFamily::Rar50Plus => self.open_read_quick_rar5()?,
        }
        self.reset_catalog_token()?;
        Ok(())
    }

    /// Discover the volume set, open the primary stream and verify the
    /// signature (setting the container family and SFX offset).
    fn open_common(&mut self) -> RarResult<()> {
        self.volume_paths = discover_volumes(&self.path);
        let f = File::open(&self.volume_paths[0])?;
        self.stream = Some(Box::new(f));
        self.verify_signature()
    }

    fn verify_signature(&mut self) -> RarResult<()> {
        // The signature must appear at the start for plain archives and
        // after the embedded stub for SFX archives (scan up to 8 MiB,
        // like the reference readers).
        let stream = stream_mut(&mut self.stream)?;
        let file_size = stream.seek(SeekFrom::End(0))?;
        stream.seek(SeekFrom::Start(0))?;
        let scan = file_size.min(SFX_SCAN_LIMIT as u64) as usize;
        let mut buf = vec![0u8; scan];
        let n = stream.read(&mut buf)?;
        buf.truncate(n);
        let (family, sfx_offset) = crate::detect::find_archive_start(&buf, SFX_SCAN_LIMIT)
            .ok_or_else(|| RarError::Format("not a RAR archive (signature not found)".into()))?;
        self.sfx_offset = sfx_offset as u64;
        self.family = family;
        let sig_len = match family {
            ArchiveFamily::Rar50Plus => RAR5_SIGNATURE.len() as u64,
            ArchiveFamily::Rar15To40 => crate::detect::RAR4_SIGNATURE.len() as u64,
            ArchiveFamily::Rar13 => crate::detect::RAR13_SIGNATURE.len() as u64,
        };
        stream.seek(SeekFrom::Start(self.sfx_offset + sig_len))?;
        Ok(())
    }
}
