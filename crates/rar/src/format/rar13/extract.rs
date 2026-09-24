//! RAR 1.3/1.4 read path: volume scanning and split-member merge.

use std::fs::File;
use std::io::{Seek, SeekFrom};

use super::{LHD_SPLIT_AFTER, LHD_SPLIT_BEFORE, MHD_SOLID, archive_comment, parse_volume};
use crate::engine::Engine;
use crate::error::{RarError, RarResult};
use crate::format::shared::split::{SplitMerge, SplitMergeError};

/// Map the shared merge error to the RAR 1.3/1.4 texts.
fn map_rar13_split_error(error: SplitMergeError) -> RarError {
    RarError::Format(match error {
        SplitMergeError::ContinuationWithoutStart { .. } => {
            "RAR 1.3: split continuation without a start".into()
        }
        SplitMergeError::Overlapping { .. } | SplitMergeError::Interrupted { .. } => {
            "RAR 1.3: split member is interrupted by a regular entry".into()
        }
        SplitMergeError::MissingFinal { .. } => "RAR 1.3: split member is incomplete".into(),
        SplitMergeError::PackedSizeOverflow { .. } => "RAR 1.3: split packed size overflow".into(),
        SplitMergeError::ChunkCountExceeded { max, .. } => {
            format!("RAR 1.3: split member exceeds the {max}-chunk ceiling")
        }
    })
}

/// Archive comment from the first volume's main-header extension.
pub(crate) fn rar13_archive_comment(cx: &dyn Engine) -> RarResult<Option<Vec<u8>>> {
    let legacy = &cx.read_ctx().legacy;
    archive_comment(legacy.rar13_flags, &legacy.rar13_extra)
}

/// Scan every volume of a RAR 1.3/1.4 set into the entry catalog,
/// merging split members across volumes (old-style `.rar`/`.r00`/`.r01`
/// naming, discovered by `discover_volumes`).
pub(crate) fn open_read_rar13(cx: &mut dyn Engine) -> RarResult<()> {
    cx.clear_catalog();
    cx.read_ctx_mut().streams.clear();

    let mut merge = SplitMerge::default();
    for (vol_idx, path) in cx.volume_paths().to_vec().iter().enumerate() {
        let mut stream = File::open(path)?;
        let file_len = stream.seek(SeekFrom::End(0))?;
        // Only the first volume may carry the SFX stub.
        let offset = if vol_idx == 0 { cx.sfx_offset() } else { 0 };
        let volume = parse_volume(&mut stream, offset, file_len)?;
        if vol_idx == 0 {
            cx.set_archive_solid(volume.flags & MHD_SOLID != 0);
            let legacy = &mut cx.read_ctx_mut().legacy;
            legacy.rar13_flags = volume.flags;
            legacy.rar13_extra = volume.extra.clone();
        }

        for mut entry in volume.entries {
            for chunk in &mut entry.chunks {
                chunk.volume_index = vol_idx;
            }
            let split_before = entry.header.flags & u64::from(LHD_SPLIT_BEFORE) != 0;
            let split_after = entry.header.flags & u64::from(LHD_SPLIT_AFTER) != 0;
            if let Some(done) = merge
                .push(entry, split_before, split_after)
                .map_err(map_rar13_split_error)?
            {
                cx.push_entry(done);
            }
        }
    }
    merge.finish().map_err(map_rar13_split_error)
}
