//! Rebuild an archive that carries no recovery record.
//!
//! WinRAR's `rar r` does not give up when an archive has no `.rev` volumes and
//! no inline recovery record: it reconstructs a new archive from the members
//! it can still read. This module implements that fallback by *decoding and
//! verifying* every member and keeping only the ones that pass — the same
//! promise the official tool makes with its `Found <name>` output, rather than
//! copying blocks that merely have a valid header CRC.
//!
//! For a RAR5 source, a member that passes is then copied **verbatim**, block
//! and all, instead of being decoded, re-stored and re-headered: that is what
//! WinRAR does, and it is the difference between a rebuilt archive that keeps
//! the members' compression, timestamps and attributes and one that keeps none
//! of them. Legacy sources still re-store, deliberately (it makes a legacy
//! member carry the official `-m0` `unp_ver`), and so does a RAR5 rebuild that
//! cannot be verified.
//!
//! It lives in the `archive` layer (not `recovery`) because it orchestrates
//! the reader and writer roles, and `recovery` must not depend on `archive`.

use std::path::Path;

use super::{
    ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, OpenOptions, WriterOptions,
};
use crate::error::RarResult;
use crate::options::ExtractOptions;
use crate::version::ArchiveVersion;

/// What a reconstruct run recovered and dropped.
#[derive(Debug, Default)]
pub struct ReconstructReport {
    recovered: Vec<String>,
    dropped: Vec<String>,
    damaged: bool,
    legacy: bool,
}

impl ReconstructReport {
    /// Names of the members that decoded and verified, in archive order.
    pub fn recovered(&self) -> &[String] {
        &self.recovered
    }

    /// Names of the members that failed to decode or verify.
    pub fn dropped(&self) -> &[String] {
        &self.dropped
    }

    /// Whether the scan had to resync past a corrupt header, losing members
    /// that never reached the catalog. `dropped` cannot name those.
    pub fn skipped_damage(&self) -> bool {
        self.damaged
    }

    /// Whether the source was a legacy (RAR 1.5–4.x) archive, whose rebuilt
    /// container is RAR4.
    pub fn legacy(&self) -> bool {
        self.legacy
    }
}

/// Rebuild `src` into `dst`, keeping only the members that decode and verify,
/// like WinRAR's `rar r` when no recovery record is present.
///
/// The rebuilt container keeps the source family: a legacy (RAR 1.3–4.x)
/// source is rebuilt as a RAR4 container (written at `v29`), everything else
/// as RAR5 (`v50`). Members are stored uncompressed (STORE), so a legacy
/// member carries the official `-m0` `unp_ver` 20 rather than the container's
/// 29; per-member timestamps and attributes are not preserved. One member is
/// held in memory at a time; the per-member and total read caps are lifted so
/// a large member is not refused mid-recovery.
///
/// An archive whose *headers* cannot be parsed at all fails to open and is
/// reported as an error: only payload damage is salvaged here.
pub fn reconstruct_archive_path(
    src: &Path,
    dst: &Path,
    password: Option<&str>,
) -> RarResult<ReconstructReport> {
    let mut open = OpenOptions::new();
    if let Some(password) = password {
        open = open.password(password);
    }
    // A damaged *header* fails the strict scan; retry tolerantly so the
    // members around the damage are still salvaged (WinRAR's `rar r`). If the
    // salvage scan cannot run either (a legacy family, or an unreadable
    // archive start), the original strict error is reported.
    let mut reader = match ArchiveReader::open_with(src, open) {
        Ok(reader) => reader,
        Err(strict) => ArchiveReader::open_salvage(src, password).map_err(|_| strict)?,
    };
    // A salvage scan silently loses members whose header was corrupt; carry
    // that up so the caller can report the damage.
    let damaged = reader.salvage_damaged();

    let mut version = ArchiveVersion::V50;
    for entry in reader.entries() {
        if entry.version().is_legacy() || entry.version().is_rar13() {
            version = ArchiveVersion::V29;
            break;
        }
    }

    // A RAR5 source rebuilds by copying the intact members' blocks verbatim.
    // `None` — and any error from the block walk, which is stricter than the
    // salvage scan — means the rebuild could not be vouched for, so the
    // decode-and-store path below runs instead.
    if !version.is_legacy() && !version.is_rar13() {
        match rebuild_verbatim(src, dst, password, &mut reader) {
            Ok(Some(mut rebuilt)) => {
                rebuilt.damaged = damaged;
                return Ok(rebuilt);
            }
            Ok(None) | Err(_) => {
                // A partial rebuild must not confuse the fallback below.
                let _ = std::fs::remove_file(dst);
            }
        }
    }

    let members: Vec<_> = reader
        .entries()
        .filter(|entry| !entry.is_dir())
        .map(|entry| (entry.id(), entry.name().to_string()))
        .collect();

    let mut writer =
        ArchiveWriter::create_with(dst, WriterOptions::default().compression(version))?;
    let store = EntryWriteOptions::new().compression_level(CompressionLevel::STORE);
    // Recovery must not refuse a member just because it is large.
    let read_options = ExtractOptions {
        max_unpacked_bytes: None,
        max_total_unpacked_bytes: None,
        ..ExtractOptions::default()
    };

    let mut report = ReconstructReport::default();
    for (id, name) in members {
        match reader.read_entry_with_options(id, read_options.clone()) {
            Ok(data) => {
                writer.add_bytes(&name, &data, store)?;
                report.recovered.push(name);
            }
            Err(_) => report.dropped.push(name),
        }
    }
    writer.finish()?;
    report.damaged = damaged;
    report.legacy = version == ArchiveVersion::V29;
    Ok(report)
}

/// The RAR5 archive signature, written once at the start of a plain archive.
const RAR5_SIGNATURE: &[u8] = b"Rar!\x1a\x07\x01\x00";

/// Try to rebuild `dst` from `src` by copying each intact member's blocks
/// verbatim, the way WinRAR's `rar r` does.
///
/// The members are decoded and verified first — the module's promise — and only
/// then copied as bytes, so a member is never trusted on its header CRC alone.
/// Returns `None` when it cannot vouch for the result: a source whose header
/// encryption the walk cannot read, a walk that does not account for every
/// member that decoded, or a rebuilt archive that fails to verify. The caller
/// then falls back to decoding and re-storing.
fn rebuild_verbatim(
    src: &Path,
    dst: &Path,
    password: Option<&str>,
    reader: &mut ArchiveReader,
) -> RarResult<Option<ReconstructReport>> {
    use std::io::{Read, Seek, SeekFrom, Write};

    use crate::format::rar5::headers::{
        ArchiveHeader, BlockCursor, EndOfArchiveHeader, FileHeader, build_main_header,
        split_main_extra,
    };
    use crate::format::rar5::{
        ARCHIVE_FLAG_RECOVERY, ARCHIVE_FLAG_VOLUME, ARCHIVE_FLAG_VOLUME_NUM,
        BLOCK_TYPE_ARCHIVE_HEADER, BLOCK_TYPE_END_ARCHIVE, BLOCK_TYPE_FILE_HEADER,
    };

    let read_options = ExtractOptions {
        max_unpacked_bytes: None,
        max_total_unpacked_bytes: None,
        ..ExtractOptions::default()
    };

    // Which members still decode, in archive order: the same answer the
    // decode-and-store path below would reach.
    let mut report = ReconstructReport::default();
    let mut intact: Vec<String> = Vec::new();
    let members: Vec<_> = reader
        .entries()
        .filter(|entry| !entry.is_dir())
        .map(|entry| (entry.id(), entry.name().to_string()))
        .collect();
    for (id, name) in members {
        match reader.read_entry_with_options(id, read_options.clone()) {
            Ok(_) => {
                intact.push(name.clone());
                report.recovered.push(name);
            }
            Err(_) => report.dropped.push(name),
        }
    }

    // Walk the source's blocks: the main header, and each file block's byte
    // span. A RAR5 file block is `[header][data]` contiguously and stores no
    // absolute offset, so a span copies as one range with no fixups. The cursor
    // starts at the first block, past the 8-byte `Rar!\x1a\x07\x01` signature.
    let mut file = std::fs::File::open(src).map_err(crate::error::RarError::Io)?;
    let len = file.metadata().map_err(crate::error::RarError::Io)?.len();
    file.seek(SeekFrom::Start(8))
        .map_err(crate::error::RarError::Io)?;
    let mut cursor = BlockCursor::new(len, None);
    let mut main: Option<ArchiveHeader> = None;
    let mut spans: Vec<(String, u64, u64)> = Vec::new();
    while let Some(meta) = cursor.next(&mut file)? {
        match meta.block_type {
            BLOCK_TYPE_ARCHIVE_HEADER => main = Some(ArchiveHeader::from_raw(&meta.raw)?),
            BLOCK_TYPE_FILE_HEADER => {
                let header = FileHeader::from_raw(&meta.raw, meta.block_start)?;
                if !header.is_directory {
                    spans.push((header.name, meta.block_start, meta.data_end));
                }
            }
            BLOCK_TYPE_END_ARCHIVE => break,
            _ => {}
        }
    }
    let Some(main) = main else { return Ok(None) };

    // Keep the spans whose member decoded. Every member that decoded must be
    // accounted for, or the block walk and the catalog disagree about the
    // archive and nothing may be copied.
    let mut waiting = intact;
    let mut kept: Vec<(u64, u64)> = Vec::new();
    for (name, start, end) in spans {
        if let Some(position) = waiting.iter().position(|candidate| *candidate == name) {
            waiting.remove(position);
            kept.push((start, end));
        }
    }
    if !waiting.is_empty() {
        return Ok(None);
    }

    // Emit: a rebuilt main header (no quick-open, no recovery record), the
    // copied blocks in order, then the end-of-archive block.
    let (_, _, extra) = split_main_extra(&main.extra_data)?;
    let flags =
        main.flags & !(ARCHIVE_FLAG_VOLUME | ARCHIVE_FLAG_VOLUME_NUM | ARCHIVE_FLAG_RECOVERY);
    let (header, _, _) = build_main_header(flags, &extra, false, false, None, 8);
    {
        let mut out = std::fs::File::create(dst).map_err(crate::error::RarError::Io)?;
        out.write_all(RAR5_SIGNATURE).map_err(crate::error::RarError::Io)?;
        out.write_all(&header).map_err(crate::error::RarError::Io)?;
        let mut buffer = vec![0u8; 1 << 20];
        for (start, end) in &kept {
            let mut left = end - start;
            file.seek(SeekFrom::Start(*start))
                .map_err(crate::error::RarError::Io)?;
            while left > 0 {
                let want = left.min(buffer.len() as u64) as usize;
                file.read_exact(&mut buffer[..want])
                    .map_err(crate::error::RarError::Io)?;
                out.write_all(&buffer[..want])
                    .map_err(crate::error::RarError::Io)?;
                left -= want as u64;
            }
        }
        out.write_all(&EndOfArchiveHeader { flags: 0 }.to_bytes())
            .map_err(crate::error::RarError::Io)?;
        out.sync_all().map_err(crate::error::RarError::Io)?;
    }

    // Verify by decoding every kept member out of the rebuilt archive. A
    // rebuild that cannot be verified is thrown away, so a wrong guess here
    // degrades to the decode-and-store path instead of salvaging bad data.
    let mut open = OpenOptions::new();
    if let Some(password) = password {
        open = open.password(password);
    }
    let verified = ArchiveReader::open_with(dst, open).and_then(|mut check| {
        for name in &report.recovered {
            let id = check.unique_entry(name)?;
            check.read_entry_with_options(id, read_options.clone())?;
        }
        Ok(())
    });
    if verified.is_err() {
        let _ = std::fs::remove_file(dst);
        return Ok(None);
    }
    report.legacy = false;
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rebuild copies the intact members' blocks instead of decoding and
    /// re-storing them, so the result keeps the members' compression: an
    /// archive whose packed form is far smaller than its members comes back at
    /// roughly its own size, not at the members' unpacked total.
    #[test]
    fn rebuild_keeps_the_members_compression() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.rar");
        let dst = dir.path().join("rebuilt.rar");
        let data: Vec<u8> = b"the quick brown fox jumps over the lazy dog\n".repeat(20_000);
        {
            let mut writer = ArchiveWriter::create(&src).unwrap();
            writer
                .add_bytes("text.txt", &data, EntryWriteOptions::new())
                .unwrap();
            writer.finish().unwrap();
        }
        let packed = std::fs::metadata(&src).unwrap().len();
        assert!(packed < data.len() as u64 / 4, "the fixture must compress");

        let report = reconstruct_archive_path(&src, &dst, None).unwrap();
        assert_eq!(report.recovered(), ["text.txt"]);
        assert!(report.dropped().is_empty());

        let rebuilt = std::fs::metadata(&dst).unwrap().len();
        assert!(
            rebuilt < data.len() as u64 / 4,
            "the rebuild re-stored the member: {rebuilt} bytes for {} unpacked",
            data.len()
        );

        let mut reader = ArchiveReader::open(&dst).unwrap();
        let id = reader.unique_entry("text.txt").unwrap();
        assert_eq!(reader.read_entry(id).unwrap(), data);
    }
}
