//! RAR 1.3/1.4 read path: volume scanning and split-member merge.

use std::fs::File;
use std::io::{Seek, SeekFrom};

use super::{LHD_SPLIT_AFTER, LHD_SPLIT_BEFORE, MHD_SOLID, archive_comment, parse_volume};
use crate::archive::{ArchiveEntry, RarArchive};
use crate::error::{RarError, RarResult};

impl RarArchive {
    /// Archive comment from the first volume's main-header extension.
    pub(crate) fn rar13_archive_comment(&self) -> RarResult<Option<Vec<u8>>> {
        let legacy = &self.read_ctx().legacy;
        archive_comment(legacy.rar13_flags, &legacy.rar13_extra)
    }

    /// Scan every volume of a RAR 1.3/1.4 set into the entry catalog,
    /// merging split members across volumes (old-style `.rar`/`.r00`/`.r01`
    /// naming, discovered by `discover_volumes`).
    pub(crate) fn open_read_rar13(&mut self) -> RarResult<()> {
        self.entries.clear();
        self.read_ctx_mut().streams.clear();

        let mut pending: Option<ArchiveEntry> = None;
        for (vol_idx, path) in self.volume_paths.clone().iter().enumerate() {
            let mut stream = File::open(path)?;
            let file_len = stream.seek(SeekFrom::End(0))?;
            // Only the first volume may carry the SFX stub.
            let offset = if vol_idx == 0 { self.sfx_offset } else { 0 };
            let volume = parse_volume(&mut stream, offset, file_len)?;
            if vol_idx == 0 {
                self.archive_solid = volume.flags & MHD_SOLID != 0;
                let legacy = &mut self.read_ctx_mut().legacy;
                legacy.rar13_flags = volume.flags;
                legacy.rar13_extra = volume.extra.clone();
            }

            for mut entry in volume.entries {
                for chunk in &mut entry.chunks {
                    chunk.volume_index = vol_idx;
                }
                let split_before = entry.header.flags & u64::from(LHD_SPLIT_BEFORE) != 0;
                let split_after = entry.header.flags & u64::from(LHD_SPLIT_AFTER) != 0;
                if split_before {
                    let Some(current) = pending.as_mut() else {
                        return Err(RarError::Format(
                            "RAR 1.3: split continuation without a start".into(),
                        ));
                    };
                    current.header.packed_size += entry.header.packed_size;
                    current.chunks.extend(entry.chunks);
                    if !split_after {
                        let mut finished = pending.take().expect("pending split member");
                        // The final fragment carries the whole-member size and
                        // checksum.
                        finished.header.unpacked_size = entry.header.unpacked_size;
                        finished.header.crc32_val = entry.header.crc32_val;
                        self.entries.push(finished);
                    }
                } else {
                    if pending.is_some() {
                        return Err(RarError::Format(
                            "RAR 1.3: split member is interrupted by a regular entry".into(),
                        ));
                    }
                    if split_after {
                        pending = Some(entry);
                    } else {
                        self.entries.push(entry);
                    }
                }
            }
        }
        if pending.is_some() {
            return Err(RarError::Format(
                "RAR 1.3: split member is incomplete".into(),
            ));
        }
        Ok(())
    }
}
