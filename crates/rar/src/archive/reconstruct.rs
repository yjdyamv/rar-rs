//! Rebuild an archive that carries no recovery record.
//!
//! WinRAR's `rar r` does not give up when an archive has no `.rev` volumes and
//! no inline recovery record: it reconstructs a new archive from the members
//! it can still read. This module implements that fallback by *decoding and
//! verifying* every member and keeping only the ones that pass — the same
//! promise the official tool makes with its `Found <name>` output, rather than
//! copying blocks that merely have a valid header CRC.
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
