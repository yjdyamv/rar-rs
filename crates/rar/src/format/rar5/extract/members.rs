//! Whole-archive and single-member extraction.
//!
//! `extract_all_with_options` picks the parallel path for large archives
//! when the memory budget allows, otherwise streams member by member;
//! `extract_entry` is the shared per-member body.

#[cfg(feature = "parallel")]
use super::capped_dict_bytes;
#[cfg(feature = "parallel")]
use super::verify::verify_integrity_for;

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(feature = "parallel")]
use crate::archive::DecryptedPayload;
use crate::archive::{ArchiveEntry, MAX_DICT_SIZE_LOG, RarArchive};
use crate::error::{RarError, RarResult};
#[cfg(feature = "parallel")]
use crate::format::rar5::COMP_METHOD_STORE;
use crate::format::rar5::headers::parse_redirect_record;
use crate::fs::atomic::{replace_file, temp_sibling_path};
use crate::fs::safe_path::sanitize_archive_path;
#[cfg(feature = "parallel")]
use crate::model::FileHeader;
#[cfg(feature = "parallel")]
use crate::parallel::extraction_pool;

/// Memory budget (packed + unpacked) for the optional parallel extraction
/// path; larger archives stream sequentially to stay bounded.
#[cfg(feature = "parallel")]
const PARALLEL_BUFFER_LIMIT: u64 = 256 * 1024 * 1024;
/// Parallel extraction only engages for at least this many members...
#[cfg(feature = "parallel")]
const PARALLEL_MIN_MEMBERS: usize = 4;
/// ...and at least this much total unpacked data (Rayon overhead amortized).
#[cfg(feature = "parallel")]
const PARALLEL_MIN_UNPACKED: u64 = 64 * 1024 * 1024;

/// Destination resolution outcome for one member; the serial and parallel
/// extraction paths share it so `-e`, `-o-` and `-or` behave identically.
///
/// Callers that need to know where a member *would* land without extracting
/// it (for example a CLI reporting how many files an extraction writes) get
/// one through [`crate::ArchiveReader::resolve_destination`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    /// Extract the member to this path.
    Extract(PathBuf),
    /// `-o-`: the destination already exists and must be left untouched.
    Skip(PathBuf),
}

impl Destination {
    /// The resolved destination path, whether it is written or skipped.
    pub fn path(&self) -> &Path {
        match self {
            Destination::Extract(path) | Destination::Skip(path) => path,
        }
    }

    /// Whether the skip-existing policy leaves the member untouched.
    pub fn is_skipped(&self) -> bool {
        matches!(self, Destination::Skip(_))
    }

    /// Consume the outcome and return the resolved path.
    pub fn into_path(self) -> PathBuf {
        match self {
            Destination::Extract(path) | Destination::Skip(path) => path,
        }
    }
}

impl RarArchive {
    /// Extract all archive contents with explicit options.
    pub fn extract_all_with_options(
        &mut self,
        dest_dir: impl AsRef<Path>,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<()> {
        let dest = dest_dir.as_ref();
        fs::create_dir_all(dest)?;
        self.read_ctx_mut().extract_options = opts;
        // A quick-open catalog carries no "STM" service records: replace it
        // with the scanned catalog before extraction restores streams.
        self.ensure_full_catalog()?;

        #[cfg(feature = "parallel")]
        {
            if self.extract_all_parallel(dest, opts)? {
                return Ok(());
            }
        }

        let mut total_unpacked = 0u64;
        let entries: Vec<_> = self.entries.clone();
        for (index, entry) in entries.iter().enumerate() {
            self.check_cancel()?;
            total_unpacked = total_unpacked
                .checked_add(entry.header.unpacked_size)
                .ok_or_else(|| RarError::LimitExceeded {
                    limit: opts.max_total_unpacked_bytes.unwrap_or(u64::MAX),
                    context: "total unpacked size overflow".into(),
                })?;
            if let Some(limit) = opts.max_total_unpacked_bytes
                && total_unpacked > limit
            {
                return Err(RarError::LimitExceeded {
                    limit,
                    context: format!(
                        "total unpacked size {total_unpacked} exceeds limit while extracting {}",
                        entry.name()
                    ),
                });
            }
            self.extract_entry(index, entry, dest)?;
        }
        Ok(())
    }

    /// Parallel extraction for eligible archives (optional `parallel`
    /// feature).
    ///
    /// Eligible: at least [`PARALLEL_MIN_MEMBERS`] members, no solid chains,
    /// no split/multi-volume members, no progress callback, and total packed
    /// + unpacked sizes within a bounded memory budget. Packed payloads are
    ///   read sequentially, then decoded and integrity-checked with Rayon
    ///   workers (archive order preserved by replaying writes sequentially
    ///   afterwards). Ineligible archives fall back to the sequential path
    ///   unchanged. The codec's decode is memory-bandwidth-bound, so the
    ///   parallel path mainly helps on machines where decompression is
    ///   CPU-bound; it engages only for member counts and sizes where Rayon
    ///   overhead is amortized.
    #[cfg(feature = "parallel")]
    fn extract_all_parallel(
        &mut self,
        dest: &Path,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<bool> {
        use rayon::prelude::*;

        if self.progress.is_some() || self.entries.len() < PARALLEL_MIN_MEMBERS {
            return Ok(false);
        }
        for (i, e) in self.entries.iter().enumerate() {
            if self.is_solid_chain_member(i) || e.chunks.len() != 1 {
                return Ok(false);
            }
        }
        let mut total_packed = 0u64;
        let mut total_unpacked = 0u64;
        for e in &self.entries {
            total_packed = total_packed.saturating_add(e.header.packed_size);
            total_unpacked = total_unpacked.saturating_add(e.header.unpacked_size);
            if total_packed > PARALLEL_BUFFER_LIMIT || total_unpacked > PARALLEL_BUFFER_LIMIT {
                return Ok(false);
            }
        }
        if total_unpacked < PARALLEL_MIN_UNPACKED {
            return Ok(false);
        }
        if let Some(limit) = opts.max_total_unpacked_bytes
            && total_unpacked > limit
        {
            return Err(RarError::LimitExceeded {
                limit,
                context: "total unpacked size exceeds limit".into(),
            });
        }

        // Phase 1: read + decrypt all payloads sequentially.
        let mut payloads: Vec<(usize, DecryptedPayload)> = Vec::with_capacity(self.entries.len());
        for i in 0..self.entries.len() {
            payloads.push((i, self.read_packed_data(i)?));
        }
        let headers: Vec<FileHeader> = self.entries.iter().map(|e| e.header.clone()).collect();

        struct DecodedMember {
            idx: usize,
            data: Vec<u8>,
        }

        // Phase 2: decode + integrity-check in parallel.
        let results: Vec<RarResult<DecodedMember>> = extraction_pool().install(|| {
            payloads
                .into_par_iter()
                .map(|(i, payload)| {
                    let hdr = &headers[i];
                    if hdr.comp_dict_size > MAX_DICT_SIZE_LOG {
                        return Err(RarError::LimitExceeded {
                            limit: MAX_DICT_SIZE_LOG as u64,
                            context: format!(
                                "{}: dictionary size log {} exceeds supported maximum {}",
                                hdr.name, hdr.comp_dict_size, MAX_DICT_SIZE_LOG
                            ),
                        });
                    }
                    // The RAR7 byte dictionary bypasses the 4-bit log: enforce
                    // the extraction cap here too.
                    let _ = capped_dict_bytes(hdr, opts.max_dict_size)?;
                    if let Some(limit) = opts.max_unpacked_bytes
                        && hdr.unpacked_size > limit
                    {
                        return Err(RarError::LimitExceeded {
                            limit,
                            context: format!(
                                "{}: unpacked size {} exceeds limit",
                                hdr.name, hdr.unpacked_size
                            ),
                        });
                    }

                    let data = if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
                        Vec::new()
                    } else if hdr.comp_method == COMP_METHOD_STORE {
                        // Mirror the serial `payload::decode_member` bound: a
                        // crafted STORE member whose packed area exceeds the
                        // declared unpacked size must fail before its excess
                        // bytes can be written.
                        let declared = usize::try_from(hdr.unpacked_size).map_err(|_| {
                            RarError::LimitExceeded {
                                limit: hdr.unpacked_size,
                                context: format!(
                                    "{}: unpacked size overflows host address space",
                                    hdr.name
                                ),
                            }
                        })?;
                        let mut data = payload.data;
                        if data.len() > declared {
                            let actual = data.len();
                            data.truncate(declared);
                            return Err(RarError::Format(format!(
                                "member {}: stored payload has {} bytes, header declares {}",
                                hdr.name, actual, hdr.unpacked_size
                            )));
                        }
                        data
                    } else {
                        crate::codec::decode_raw(
                            &payload.data,
                            hdr.unpacked_size,
                            crate::codec::DecodeOptions {
                                dict_size_log: hdr.comp_dict_size,
                                dict_size_bytes: hdr.dict_size_bytes,
                                variant: crate::version::ArchiveVersion::from_v70(
                                    hdr.dict_size_bytes.is_some(),
                                ),
                                state: None,
                            },
                        )?
                    };

                    let crc = crc32fast::hash(&data);
                    let blake = if hdr.hash_value.is_some() {
                        Some(crate::format::rar5::blake2sp::hash(&data))
                    } else {
                        None
                    };
                    verify_integrity_for(
                        hdr,
                        crc,
                        blake,
                        payload.params.as_ref(),
                        payload.keys.as_ref(),
                    )?;
                    Ok(DecodedMember { idx: i, data })
                })
                .collect()
        });

        // Phase 3: replay writes sequentially in archive order.
        for result in results {
            let member = result?;
            let entry = &self.entries[member.idx];
            let dest_path = match self.resolve_dest_path(entry, dest)? {
                Destination::Extract(path) => path,
                Destination::Skip(_) => continue,
            };
            if entry.is_dir() {
                fs::create_dir_all(&dest_path)?;
                // Flat extraction resolves directories to the destination
                // root itself; the archived mode must not be applied to the
                // caller's directory.
                if dest_path.as_path() != dest {
                    self.apply_member_attributes(&entry.header, &dest_path);
                }
                continue;
            }
            if let Some(redir) = parse_redirect_record(&entry.header.extra_data) {
                if !self.read_ctx().extract_options.skip_links {
                    self.extract_redirection(dest, &dest_path, &redir)?;
                }
                continue;
            }
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let tmp_path = temp_sibling_path(&dest_path);
            let write_result = (|| -> RarResult<()> {
                let mut file = File::create(&tmp_path)?;
                file.write_all(&member.data)?;
                file.flush()?;
                Ok(())
            })();
            match write_result {
                Ok(()) => replace_file(&tmp_path, &dest_path)?,
                Err(e) => {
                    let _ = fs::remove_file(&tmp_path);
                    return Err(e);
                }
            }
            if crate::archive::file_header_has_mtime(&entry.header) {
                self.apply_member_times(&entry.header, &dest_path);
            }
            self.extract_member_streams(member.idx, &dest_path)?;
            self.propagate_member_mark_of_the_web(&dest_path);
            self.apply_member_attributes(&self.entries[member.idx].header, &dest_path);
        }
        Ok(true)
    }
    /// Extract a single entry with explicit options.
    pub fn extract_with_options(
        &mut self,
        name: &str,
        dest_dir: impl AsRef<Path>,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<PathBuf> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.name() == name)
            .ok_or_else(|| RarError::MemberNotFound {
                name: name.to_string(),
            })?;
        self.extract_at_index_with_options(idx, dest_dir, opts)
    }

    /// Extract an entry selected by its archive-order catalog index.
    pub(crate) fn extract_at_index_with_options(
        &mut self,
        idx: usize,
        dest_dir: impl AsRef<Path>,
        opts: crate::options::ExtractOptions,
    ) -> RarResult<PathBuf> {
        if idx >= self.entries.len() {
            return Err(RarError::InvalidState(
                "entry index is outside the current catalog".into(),
            ));
        }
        // Capture the member's payload position before the catalog can be
        // rebuilt. A quick-open catalog can order entries differently from
        // the full scan, so the index alone is not stable across the rescan
        // while the payload offset identifies the same member in both.
        let data_offset = self.entries[idx].chunks.first().map(|c| c.data_offset);
        let rebuilt = self.read_ctx().quick_open_catalog;
        let dest = dest_dir.as_ref();
        fs::create_dir_all(dest)?;
        self.read_ctx_mut().extract_options = opts;
        // A quick-open catalog carries no "STM" service records: replace it
        // with the scanned catalog before extraction restores streams.
        self.ensure_full_catalog()?;
        let idx = if rebuilt {
            data_offset
                .and_then(|offset| {
                    self.entries.iter().position(|entry| {
                        entry
                            .chunks
                            .first()
                            .is_some_and(|chunk| chunk.data_offset == offset)
                    })
                })
                .ok_or(RarError::StaleEntryId)?
        } else {
            idx
        };
        if idx >= self.entries.len() {
            return Err(RarError::InvalidState(
                "entry index is outside the scanned catalog".into(),
            ));
        }
        self.validate_entry_limits(idx)?;
        self.extract_entry(idx, &self.entries[idx].clone(), dest)
    }

    /// Validate per-entry header limits against the current extract options.
    pub(super) fn validate_entry_limits(&self, idx: usize) -> RarResult<()> {
        let hdr = &self.entries[idx].header;
        if hdr.comp_dict_size > MAX_DICT_SIZE_LOG {
            return Err(RarError::LimitExceeded {
                limit: MAX_DICT_SIZE_LOG as u64,
                context: format!(
                    "{}: dictionary size log {} exceeds supported maximum {}",
                    hdr.name, hdr.comp_dict_size, MAX_DICT_SIZE_LOG
                ),
            });
        }
        if let Some(limit) = self.read_ctx().extract_options.max_unpacked_bytes
            && hdr.unpacked_size > limit
        {
            return Err(RarError::LimitExceeded {
                limit,
                context: format!(
                    "{}: unpacked size {} exceeds limit",
                    hdr.name, hdr.unpacked_size
                ),
            });
        }
        Ok(())
    }

    /// Resolve the destination path for one member, applying the shared
    /// extraction policies: flat extraction (`-e`), `-o-` (skip existing)
    /// and `-or` (auto rename).
    ///
    /// Flat extraction (`rar e` / `unrar e`) lands members in the
    /// destination directory under their basename. The safe-path policy
    /// always applies — the full member name is sanitized (which rejects
    /// `..`/absolute/drive names) before its basename is used, so
    /// traversal-shaped names cannot escape the destination. A directory
    /// in flat mode resolves to the destination directory itself.
    ///
    /// `Skip` means the destination exists and `skip_existing` is set, so
    /// the caller must leave it untouched. Both the serial (`extract_entry`)
    /// and parallel (phase 3) paths call this, so they stay identical.
    fn resolve_dest_path(&self, entry: &ArchiveEntry, dest_dir: &Path) -> RarResult<Destination> {
        self.resolve_dest_path_with(entry, dest_dir, &self.read_ctx().extract_options)
    }

    /// [`Self::resolve_dest_path`] with explicit options, for callers that
    /// resolve members outside the extraction loop (the CLI's written-file
    /// count, via [`crate::ArchiveReader::resolve_destination`]).
    pub(crate) fn resolve_dest_path_with(
        &self,
        entry: &ArchiveEntry,
        dest_dir: &Path,
        options: &crate::options::ExtractOptions,
    ) -> RarResult<Destination> {
        let dest_path = if options.flat_paths {
            if entry.is_dir() {
                return Ok(Destination::Extract(dest_dir.to_path_buf()));
            }
            let safe_name = sanitize_archive_path(&entry.header.name)?;
            let base = safe_name.rsplit('/').next().unwrap_or(&safe_name);
            dest_dir.join(base)
        } else {
            self.safe_dest_path_with(dest_dir, &entry.header.name, options.safe_paths)?
        };

        // `-o-` (skip existing): members whose destination already exists
        // are left untouched.
        if options.skip_existing && dest_path.exists() {
            return Ok(Destination::Skip(dest_path));
        }

        // `-or` (auto rename): when the destination exists, insert `(N)`
        // before the extension (like WinRAR: a.txt -> a(1).txt).
        let mut dest_path = dest_path;
        if options.auto_rename && !entry.is_dir() {
            let mut n = 1;
            while dest_path.exists() {
                let file_name = dest_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let (stem, ext) = match file_name.rfind('.') {
                    Some(dot) if dot > 0 => (&file_name[..dot], &file_name[dot..]),
                    _ => (file_name.as_str(), ""),
                };
                dest_path = dest_path.with_file_name(format!("{stem}({n}){ext}"));
                n += 1;
            }
        }
        Ok(Destination::Extract(dest_path))
    }

    /// Extract one entry. File contents are decoded to a temporary file and
    /// renamed over the destination only after integrity checks pass, so a
    /// failure never leaves partial or corrupt output behind.
    fn extract_entry(
        &mut self,
        idx: usize,
        entry: &ArchiveEntry,
        dest_dir: &Path,
    ) -> RarResult<PathBuf> {
        self.validate_entry_limits(idx)?;

        let dest_path = match self.resolve_dest_path(entry, dest_dir)? {
            Destination::Extract(path) => path,
            Destination::Skip(path) => return Ok(path),
        };

        if entry.is_dir() {
            fs::create_dir_all(&dest_path)?;
            // Flat extraction resolves directories to the destination root
            // itself; the archived mode must not be applied to the caller's
            // directory.
            if dest_path.as_path() != dest_dir {
                self.apply_member_attributes(&entry.header, &dest_path);
            }
            return Ok(dest_path);
        }

        // RAR5 redirect records (symlinks, hardlinks, file copies): the
        // entry carries no data, only the target reference. `-ol-` skips
        // them entirely.
        if let Some(redir) = parse_redirect_record(&entry.header.extra_data) {
            if self.read_ctx().extract_options.skip_links {
                return Ok(dest_path);
            }
            return self.extract_redirection(dest_dir, &dest_path, &redir);
        }

        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = temp_sibling_path(&dest_path);
        let result = (|| -> RarResult<u64> {
            let mut file = File::create(&tmp_path)?;
            let written = if self.rar4 || self.rar13 {
                self.decode_rar4_to(idx, &mut file)?
            } else if self.is_solid_chain_member(idx) {
                self.decode_solid_through_to(idx, &mut file)?
            } else {
                self.decode_file_to(idx, &mut file, None)?
            };
            file.flush()?;
            Ok(written)
        })();

        match result {
            Ok(_) => {
                replace_file(&tmp_path, &dest_path)?;
            }
            Err(e) => {
                if self.read_ctx().extract_options.keep_broken {
                    // `-kb`: keep the partially extracted file.
                    let _ = replace_file(&tmp_path, &dest_path);
                } else {
                    let _ = fs::remove_file(&tmp_path);
                }
                return Err(e);
            }
        }

        // Restore mtime (best-effort), including the nanosecond fraction
        // from the FILE_TIME extra record when present.
        if crate::archive::file_header_has_mtime(&entry.header) {
            self.apply_member_times(&entry.header, &dest_path);
        }
        // Restore NTFS alternate data streams attached to this member
        // (no-op on non-Windows, like the reference extractor).
        self.extract_member_streams(idx, &dest_path)?;
        self.propagate_member_mark_of_the_web(&dest_path);
        self.apply_member_attributes(&entry.header, &dest_path);

        Ok(dest_path)
    }
}
