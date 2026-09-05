//! Surgical archive rewrite: delete, rename, comment and recovery-record
//! mutation share one plan/execute pipeline. Methods on [RarArchive]
//! in a sibling impl block (see `crate::archive::mod`).

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use super::*;
use crate::codec::{DecoderState, lzss_huff as compression};
use crate::crypto::parse_archive_encrypt_header;
use crate::io_util::{read_write_create, replace_file, temp_sibling_path, temp_suffix};
use crate::rar50::headers::*;
use crate::rar50::vint;

use super::{PendingCommit, volume_base_of, volume_path};

/// One step of a surgical archive rewrite.
enum RewriteOp {
    /// Copy one block verbatim: `header_bytes` followed by `len` bytes of
    /// data starting at `src_data` in the original archive. `qo_header`
    /// holds the header bytes for the rebuilt quick-open record (copied
    /// file blocks only, plaintext archives only).
    CopyBlock {
        header_bytes: Vec<u8>,
        src_data: u64,
        len: u64,
        qo_header: Option<Vec<u8>>,
    },
    /// Decode (and recompress when kept) one member of the affected solid
    /// chain.
    Recompress { idx: usize, is_deleted: bool },
}

/// The result of planning a rewrite: the blocks to emit, in order.
struct RewritePlan {
    ops: Vec<RewriteOp>,
    /// Verbatim bytes of the archive encryption header (if any), written
    /// before the rebuilt main header.
    encrypt_header: Option<Vec<u8>>,
    /// Parsed main header block (plaintext).
    main_meta: BlockMeta,
    /// Recovery percentage from the dropped RR record; the record is
    /// rebuilt over the rewritten archive.
    rr_percent: Option<u8>,
    /// New archive comment (CMT service block), written right after the
    /// main header.
    comment: Option<Vec<u8>>,
}

/// Result of one combined structural edit: how many members were
/// deleted and how many were renamed (explicit rename pairs only;
/// directory-expanded descendants are not counted).
pub(crate) struct EditSummary {
    pub deleted: usize,
    pub renamed: usize,
}

/// Lazily opened readers for every volume of the original archive.
struct VolumeReaders {
    files: Vec<Option<File>>,
    paths: Vec<PathBuf>,
}

impl VolumeReaders {
    fn new(paths: &[PathBuf]) -> Self {
        VolumeReaders {
            files: (0..paths.len()).map(|_| None).collect(),
            paths: paths.to_vec(),
        }
    }

    fn read_chunk(&mut self, vol: usize, offset: u64, len: u64) -> RarResult<Vec<u8>> {
        let file = self
            .files
            .get_mut(vol)
            .ok_or_else(|| RarError::Format(format!("chunk references missing volume {vol}")))?;
        if file.is_none() {
            *file = Some(File::open(&self.paths[vol])?);
        }
        let f = file.as_mut().unwrap();
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len as usize];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }
}

impl crate::rar50::payload::ChunkReader for VolumeReaders {
    fn read_chunk(&mut self, vol: usize, offset: u64, len: u64) -> RarResult<Vec<u8>> {
        VolumeReaders::read_chunk(self, vol, offset, len)
    }
}

/// Parse the recovery parameters out of an existing `.rev` file header:
/// `(rec_count, data_count)`.
fn rev_params_from_file(path: &Path) -> RarResult<(u32, u32)> {
    let data = std::fs::read(path)?;
    if data.len() < 8 + 4 + 4 + 1 + 2 + 2 + 2 + 4
        || &data[..8] != crate::recovery::rev50::REV5_SIGNATURE
    {
        return Err(RarError::Format(format!(
            "{}: not a RAR5 recovery volume",
            path.display()
        )));
    }
    let mut off = 8 + 4 + 4; // signature + header CRC + header size
    if data[off] != 1 {
        return Err(RarError::Format(format!(
            "{}: unsupported recovery volume version",
            path.display()
        )));
    }
    off += 1;
    let data_count = u16::from_le_bytes(data[off..off + 2].try_into().unwrap()) as u32;
    off += 2;
    let rec_count = u16::from_le_bytes(data[off..off + 2].try_into().unwrap()) as u32;
    Ok((rec_count, data_count))
}

/// Read-ahead copy job: `len` bytes from `src` in the original archive.
#[cfg(feature = "parallel")]
#[derive(Clone, Copy)]
struct CopyJob {
    src: u64,
    len: u64,
}

/// Bounded producer thread that prefetches verbatim block data ahead of
/// the writer, overlapping source reads with destination writes (and, for
/// solid chains, with the CPU-bound recompression).
#[cfg(feature = "parallel")]
struct CopyPipeline {
    rx: std::sync::mpsc::Receiver<Result<Vec<u8>, RarError>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "parallel")]
impl CopyPipeline {
    const CHUNK: usize = 4 * 1024 * 1024;
    const QUEUE: usize = 4;

    fn start(src_path: &Path, jobs: &[CopyJob]) -> Self {
        let src_path = src_path.to_path_buf();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, RarError>>(Self::QUEUE);
        let jobs = jobs.to_vec();
        let handle = std::thread::spawn(move || {
            let mut f = match File::open(src_path) {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(Err(e.into()));
                    return;
                }
            };
            for job in jobs {
                if let Err(e) = f.seek(SeekFrom::Start(job.src)) {
                    let _ = tx.send(Err(e.into()));
                    return;
                }
                let mut left = job.len;
                while left > 0 {
                    let want = left.min(Self::CHUNK as u64) as usize;
                    let mut buf = vec![0u8; want];
                    if let Err(e) = f.read_exact(&mut buf) {
                        let _ = tx.send(Err(e.into()));
                        return;
                    }
                    if tx.send(Ok(buf)).is_err() {
                        return; // consumer aborted
                    }
                    left -= want as u64;
                }
            }
        });
        CopyPipeline {
            rx,
            handle: Some(handle),
        }
    }

    /// Next prefetched buffer, in job order.
    fn take(&self) -> RarResult<Option<Vec<u8>>> {
        match self.rx.recv() {
            Ok(Ok(buf)) => Ok(Some(buf)),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(None),
        }
    }

    fn finish(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl RarArchive {
    // ── Public API: deletion ───────────────────────────────────────────────

    /// Delete members from a RAR5 archive without rebuilding the whole
    /// archive.
    ///
    /// The archive is rewritten surgically: every block before the first
    /// deleted member is copied verbatim, and for non-solid archives every
    /// kept member after the deletion point is copied verbatim too (file
    /// header and compressed payload included) — only the main header
    /// locator, the quick-open record and the end block are re-emitted.
    /// This matches the official `rar d`, which never recompresses
    /// non-solid archives.
    ///
    /// Solid archives: when the first deleted member belongs to a solid
    /// chain, the members after it reference the removed data, so the whole
    /// chain is decoded and recompressed from its start; everything before
    /// the chain start is still copied verbatim (the official `rar d`
    /// recompresses the entire archive in this case).
    ///
    /// Multi-volume archives: kept members keep their exact compressed
    /// payloads but are re-split at the volume size limit, and `.rev`
    /// recovery volumes are regenerated (the official `rar` CLI refuses to
    /// modify multi-volume archives at all).
    ///
    /// Other behaviors:
    /// - inline recovery records are rebuilt over the rewritten archive,
    ///   so `rar r` can still repair it (the official `rar d` drops them);
    /// - the quick-open record is rebuilt from the kept members;
    /// - when every member is deleted, the archive is erased;
    /// - with the `parallel` feature, verbatim block data is prefetched by
    ///   a background thread, overlapping reads with writes and with the
    ///   solid-chain recompression.
    ///
    /// [`RarArchive::delete`] with rewrite progress reporting: `progress`
    /// receives `(written_bytes, total_bytes)`, monotonic, covering the
    /// whole rewrite (verbatim copies plus any solid-chain recompression)
    /// and reaching `total` on success.
    pub fn delete_with_progress(
        &mut self,
        names: &[&str],
        progress: Option<Box<dyn FnMut(u64, u64) + Send>>,
    ) -> RarResult<usize> {
        let saved = self.progress.take();
        self.progress_member = 0;
        if let Some(cb) = progress {
            // The total is left unset: the rewrite loop reports the exact
            // work byte count (kept payloads + recompressed input), which
            // the tracker adopts as its denominator.
            let tracker = crate::write_progress::ProgressTracker::new(Some(cb));
            self.progress = Some(std::sync::Arc::new(std::sync::Mutex::new(tracker)));
        }
        let result = self.delete(names);
        self.progress = saved;
        result
    }

    /// Delete members without rebuilding the archive (like `rar d`);
    /// returns the number of deleted members.
    ///
    /// Kept members keep their exact compressed bytes (never recompressed)
    /// and the archive is rewritten surgically; the solid chain that loses
    /// a member is decoded and recompressed from its start. Multi-volume
    /// archives are re-split at the volume size limit and `.rev` recovery
    /// volumes are regenerated. Fails when any requested name is not
    /// present, and with [`RarError::Unsupported`] for locked archives.
    pub fn delete(&mut self, names: &[&str]) -> RarResult<usize> {
        if self.mode != Mode::Read {
            return Err(RarError::Format(
                "delete requires an archive opened for reading".into(),
            ));
        }
        // Resolve every name to a distinct catalog index before any rewrite
        // starts: a missing name fails the whole call and leaves the archive
        // untouched, and repeated names delete successive duplicate members
        // (the legacy contract).
        let mut selected: Vec<usize> = Vec::with_capacity(names.len());
        for name in names {
            let idx = self
                .entries
                .iter()
                .enumerate()
                .find(|(i, e)| e.name() == *name && !selected.contains(i))
                .map(|(i, _)| i)
                .ok_or_else(|| RarError::MemberNotFound {
                    name: name.to_string(),
                })?;
            selected.push(idx);
        }
        self.delete_indexes(&selected)
    }

    /// Delete the members at the given catalog indexes (like `rar d`);
    /// returns the number deleted. Name-based [`Self::delete`] and the
    /// typed [`ArchiveEditor`] resolve names or IDs to indexes and delegate
    /// to [`Self::edit_plan`].
    pub(crate) fn delete_indexes(&mut self, indexes: &[usize]) -> RarResult<usize> {
        if indexes.is_empty() {
            return Err(RarError::Format("no members to delete".into()));
        }
        Ok(self.edit_plan(indexes, &[], None, None)?.deleted)
    }

    /// Rewrite a multi-volume archive, omitting deleted members.
    ///
    /// Kept members keep their exact compressed payloads but are re-split
    /// at the volume size limit (the official `rar` CLI refuses to modify
    /// multi-volume archives at all; this matches WinRAR's rebuild
    /// behavior). Solid chains are decoded and recompressed like in the
    /// single-volume path. Trailing QO/RR service records are dropped and
    /// `.rev` recovery volumes are regenerated.
    fn rewrite_multivolume(
        &mut self,
        deleted: &[bool],
        chain: Option<(usize, usize)>,
        rename_map: Option<&std::collections::HashMap<usize, String>>,
    ) -> RarResult<()> {
        if self.header_encryption {
            return Err(RarError::Unsupported(
                "deleting from header-encrypted multi-volume archives is not supported".into(),
            ));
        }
        let orig_volumes = self.volume_paths.clone();
        let mut vol_sizes = Vec::with_capacity(orig_volumes.len());
        for vol in &orig_volumes {
            vol_sizes.push(fs::metadata(vol)?.len());
        }
        // Every volume except the last is exactly the size limit.
        let volume_size = *vol_sizes[..vol_sizes.len() - 1]
            .iter()
            .min()
            .unwrap_or(&vol_sizes[0]);
        if volume_size == 0 {
            return Err(RarError::Format(
                "cannot rewrite: volume size is zero".into(),
            ));
        }

        let base = volume_base_of(&self.path);
        let parent = self.path.parent().unwrap_or(Path::new(".")).to_path_buf();
        // Write to a temporary volume base and rename over the originals
        // only after every volume succeeded (a failure never destroys the
        // original set).
        let tmp_base = format!(".{base}.rar5tmp-{}", temp_suffix());
        let tmp_base_path = parent.join(&tmp_base);

        // Write the new volume set. Swapping `self.path` makes the
        // streamed payload spill file land next to the temporary volumes;
        // volume naming itself comes from the staged `pending` set.
        let saved_path = self.path.clone();
        self.path = tmp_base_path;
        self.write_ctx_mut().volume_size = Some(volume_size);
        self.volume_paths = vec![volume_path(&parent, &base, 1)];
        self.write_ctx_mut().current_volume = 1;
        self.write_ctx_mut().volume_bytes_written = 0;
        self.write_ctx_mut().pending = Some(PendingCommit::Volumes {
            parent: parent.clone(),
            tmp_base: tmp_base.clone(),
            final_base: base.clone(),
        });
        self.stream = Some(Box::new(read_write_create(&volume_path(
            &parent, &tmp_base, 1,
        ))?));
        self.write_signature()?;
        self.write_archive_header_vol(None)?;
        self.write_ctx_mut().volume_bytes_written =
            self.stream.as_mut().unwrap().stream_position()?;

        let mut readers = VolumeReaders::new(&orig_volumes);
        let (mut dec, mut enc, mut enc_active) = (None, None, false);
        let mut in_chain = false;
        let mut chain_end = usize::MAX;
        let total_bytes: u64 = self
            .entries
            .iter()
            .enumerate()
            .filter(|(i, _)| !deleted[*i])
            .map(|(_, e)| e.header.packed_size)
            .sum();
        let mut processed = 0u64;
        for (idx, entry) in self.entries.clone().iter().enumerate() {
            if self.progress.is_some() {
                self.report_progress(processed, total_bytes);
            }
            if !in_chain
                && let Some((s, e)) = chain
                && s == idx
            {
                let dict_log = self.entries[idx].header.comp_dict_size;
                let dict_size =
                    (128usize * 1024)
                        .checked_shl(dict_log as u32)
                        .ok_or_else(|| {
                            RarError::Format("dictionary size overflows host address space".into())
                        })?;
                dec = Some(DecoderState::new(dict_size));
                enc = Some(crate::codec::EncoderState::default());
                enc_active = false;
                in_chain = true;
                chain_end = e;
            }
            let is_chain = in_chain && idx <= chain_end;
            if is_chain && idx == chain_end {
                in_chain = false;
            }

            if deleted[idx] {
                if is_chain && !entry.is_dir() && entry.header.comp_method != COMP_METHOD_STORE {
                    // Advance the chain window.
                    let _ =
                        self.decode_chain_member_volumes(&mut readers, idx, dec.as_mut().unwrap())?;
                }
                continue;
            }
            let entry_name = rename_map
                .and_then(|m| m.get(&idx))
                .cloned()
                .unwrap_or_else(|| entry.header.name.clone());
            if entry.is_dir() {
                let fh = FileHeader {
                    name: entry_name.clone(),
                    attributes: entry.header.attributes,
                    mtime: entry.header.mtime,
                    host_os: OS_UNIX,
                    file_flags: FILE_FLAG_TIME_UNIX | FILE_FLAG_DIRECTORY,
                    is_directory: true,
                    ..Default::default()
                };
                let hdr_bytes = fh.to_bytes();
                self.write_block_header(&hdr_bytes)?;
                continue;
            }
            if is_chain && entry.header.comp_method != COMP_METHOD_STORE {
                self.recompress_chain_member_volumes_named(
                    &mut readers,
                    idx,
                    &entry_name,
                    dec.as_mut().unwrap(),
                    enc.as_mut().unwrap(),
                    &mut enc_active,
                )?;
                processed += entry.header.unpacked_size;
                continue;
            }

            // Verbatim payload, re-split across the new volumes.
            let payload = self.read_packed_volumes(&mut readers, idx)?;
            processed += payload.data.len() as u64;
            let hdr = &entry.header;
            self.write_file_entry(
                &entry_name,
                hdr.unpacked_size,
                &payload.data,
                hdr.crc32_val.unwrap_or(0),
                hdr.comp_method,
                hdr.comp_dict_size,
                hdr.dict_size_bytes,
                &hdr.extra_data,
                hdr.attributes,
                hdr.mtime,
                hdr.comp_solid,
                hdr.hash_value,
            )?;
        }
        if self.progress.is_some() {
            self.report_progress(processed, total_bytes);
        }
        self.write_end_block()?;
        self.stream = None;
        self.write_ctx_mut().volume_size = None;
        self.path = saved_path;

        // Move the new volumes into place, drop stale ones, and regenerate
        // `.rev` recovery volumes when the original set had any.
        let result = (|| -> RarResult<()> {
            let mut final_volumes: Vec<PathBuf> = Vec::new();
            for n in 1..=orig_volumes.len() {
                let tmp = volume_path(&parent, &tmp_base, n);
                let final_path = volume_path(&parent, &base, n);
                let exists = fs::metadata(&tmp).map(|m| m.len() > 0).unwrap_or(false);
                if !exists {
                    let _ = fs::remove_file(&tmp);
                    continue;
                }
                replace_file(&tmp, &final_path)?;
                final_volumes.push(final_path);
            }
            // Drop any stale volumes beyond the new set.
            for n in final_volumes.len() + 1..=orig_volumes.len() {
                let _ = fs::remove_file(volume_path(&parent, &base, n));
            }
            self.volume_paths = final_volumes;

            let rev = parent.join(format!("{base}.part1.rev"));
            if rev.exists() {
                let (rec_count, _data_count) = rev_params_from_file(&rev)?;
                // Drop every stale `.rev` file, then regenerate the set.
                let mut n = 1u32;
                loop {
                    let old_rev = parent.join(format!("{base}.part{n}.rev"));
                    if old_rev.exists() {
                        let _ = fs::remove_file(old_rev);
                        n += 1;
                    } else {
                        break;
                    }
                }
                self.recovery_volumes_count = Some(rec_count.min(self.volume_paths.len() as u32));
                self.recovery_volumes_percent = None;
                self.write_recovery_volumes()?;
            }
            Ok(())
        })();
        // The staged volumes were renamed/cleaned above (or on error below),
        // so the drop guard must not touch them again.
        self.write_ctx_mut().pending = None;
        if result.is_err() {
            for n in 1..=orig_volumes.len() {
                let _ = fs::remove_file(volume_path(&parent, &tmp_base, n));
            }
        }
        result
    }

    /// Read the full packed (and decrypted, when applicable) payload of a
    /// multi-volume member across its chunks on the original volumes.
    fn read_packed_volumes(
        &mut self,
        readers: &mut VolumeReaders,
        idx: usize,
    ) -> RarResult<DecryptedPayload> {
        let entry = &self.entries[idx];
        let hdr = &entry.header;
        crate::rar50::payload::read_packed(
            readers,
            hdr,
            &entry.chunks,
            &hdr.name,
            self.password.as_deref(),
            self.max_packed_bytes(),
            || Ok(()),
        )
    }

    /// Decode a multi-volume chain member with a shared decoder state,
    /// verifying its integrity.
    fn decode_chain_member_volumes(
        &mut self,
        readers: &mut VolumeReaders,
        idx: usize,
        state: &mut DecoderState,
    ) -> RarResult<Vec<u8>> {
        let hdr = &self.entries[idx].header;
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(Vec::new());
        }
        let payload = self.read_packed_volumes(readers, idx)?;
        let mut raw_data = Vec::new();
        crate::rar50::payload::decode_member(
            &self.entries[idx].header,
            &payload,
            Some(state),
            &mut raw_data,
        )?;
        let crc = crc32fast::hash(&raw_data);
        let blake = self.entries[idx]
            .header
            .hash_value
            .map(|_| crate::rar50::blake2sp::hash(&raw_data));
        self.verify_integrity(
            idx,
            crc,
            blake,
            payload.params.as_ref(),
            payload.keys.as_ref(),
        )?;
        Ok(raw_data)
    }

    /// Decode and recompress one member of the affected solid chain in a
    /// multi-volume archive. `name` overrides the entry name (rename).
    fn recompress_chain_member_volumes_named(
        &mut self,
        readers: &mut VolumeReaders,
        idx: usize,
        name: &str,
        dec: &mut DecoderState,
        enc: &mut crate::codec::EncoderState,
        enc_active: &mut bool,
    ) -> RarResult<()> {
        let entry = self.entries[idx].clone();
        let hdr = &entry.header;
        let data = self.decode_chain_member_volumes(readers, idx, dec)?;

        let plain_crc = crc32fast::hash(&data);
        let plain_blake = hdr.hash_value.map(|_| crate::rar50::blake2sp::hash(&data));
        let variant = crate::version::ArchiveVersion::from_v70(hdr.dict_size_bytes.is_some());
        let packed = compression::encode_chunked(
            &data,
            compression::EncodeOptions {
                chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                state: Some(enc),
                is_final: true,
                variant,
                ..compression::EncodeOptions::new(hdr.comp_method, hdr.comp_dict_size)
            },
        )?;

        let (method, dsl, dict_bytes, payload) = if packed.len() >= data.len() {
            enc.reset();
            *enc_active = false;
            (COMP_METHOD_STORE, 0u8, None, data.clone())
        } else {
            *enc_active = true;
            (
                hdr.comp_method,
                hdr.comp_dict_size,
                hdr.dict_size_bytes,
                packed,
            )
        };
        let (header_crc, extra_data, stored_hash, encr_params) =
            RarArchive::payload_extra_and_crc(self.password.as_deref(), plain_crc, plain_blake)?;
        let payload = RarArchive::encrypt_payload_with(
            self.password.as_deref(),
            encr_params.as_ref(),
            &payload,
        )?;
        self.write_file_entry(
            name,
            data.len() as u64,
            &payload,
            header_crc,
            method,
            dsl,
            dict_bytes,
            &extra_data,
            hdr.attributes,
            hdr.mtime,
            *enc_active,
            stored_hash,
        )?;
        Ok(())
    }

    /// Rename members in the archive (like `rar rn`).
    ///
    /// Each `(old, new)` pair renames the first member whose name equals
    /// `old`; renaming a directory also renames the matching prefix of
    /// every member beneath it. Kept members keep their exact compressed
    /// payloads — only the file headers are re-emitted (the quick-open
    /// record is rebuilt and, unlike the official `rar rn`, the recovery
    /// record is rebuilt so `rar r` can still repair the archive).
    ///
    /// Multi-volume archives are re-split like [`Self::delete`] (the
    /// official `rar rn` patches only the first chunk header in place).
    ///
    /// Returns the number of renamed members. Fails with
    /// [`RarError::Format`] when any source name is not present, and with
    /// [`RarError::Unsupported`] for locked archives.
    pub fn rename(&mut self, renames: &[(&str, &str)]) -> RarResult<usize> {
        if self.mode != Mode::Read {
            return Err(RarError::Format(
                "rename requires an archive opened for reading".into(),
            ));
        }
        self.ensure_write_ctx();
        if self.main_header_is_locked()? {
            return Err(RarError::ArchiveLocked);
        }

        // Resolve the pairs sequentially, honoring earlier renames in the
        // same call and expanding directory renames to their descendants.
        let mut map: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
        let mut count = 0usize;
        for (old, new) in renames {
            let old_norm = old.trim_end_matches('/').to_string();
            let idx = self
                .entries
                .iter()
                .enumerate()
                .find(|(i, e)| {
                    let name = map
                        .get(i)
                        .map(|n| n.as_str())
                        .unwrap_or(e.name())
                        .trim_end_matches('/');
                    name == old_norm
                })
                .map(|(i, _)| i)
                .ok_or_else(|| RarError::MemberNotFound {
                    name: old.to_string(),
                })?;
            let is_dir = self.entries[idx].is_dir();
            let new_norm = new.trim_end_matches('/').to_string();
            if is_dir {
                map.insert(idx, format!("{new_norm}/"));
                let prefix = format!("{old_norm}/");
                for (i, e) in self.entries.iter().enumerate() {
                    if i == idx || map.contains_key(&i) {
                        continue;
                    }
                    if let Some(rest) = e.name().strip_prefix(&prefix) {
                        map.insert(i, format!("{new_norm}/{rest}"));
                    }
                }
            } else {
                map.insert(idx, new_norm.clone());
            }
            count += 1;
        }

        self.rewrite_edit(vec![false; self.entries.len()], None, &map, None, None)?;
        Ok(count)
    }

    /// Apply a combined structural edit — deletes, renames and optional
    /// comment/recovery-record changes — in one atomic rewrite (a staged
    /// sibling file for single-volume archives, a re-split staged volume
    /// set for multi-volume ones) and reload the catalog. Returns the
    /// number of deleted and renamed members.
    ///
    /// Validation happens before any rewrite: out-of-range indexes fail
    /// with [`RarError::StaleEntryId`], a member cannot be both deleted and
    /// renamed, and renaming a member of a solid chain that also loses a
    /// member is refused with [`RarError::Unsupported`] (the recompressed
    /// chain would not carry the new name — split the edit instead).
    /// `comment = Some(_)` replaces the archive comment (empty bytes remove
    /// it); `None` keeps it. `force_rr` overrides the inline recovery
    /// record percentage (like `rar rr`); without it the record is rebuilt
    /// at the original percentage. Comment and recovery changes require a
    /// single-volume archive, like the legacy methods. Deleting every
    /// member without renames erases the archive like `rar d`. Locked
    /// archives fail with [`RarError::ArchiveLocked`].
    pub(crate) fn edit_plan(
        &mut self,
        delete_indexes: &[usize],
        renames: &[(usize, String)],
        force_rr: Option<u8>,
        comment: Option<&[u8]>,
    ) -> RarResult<EditSummary> {
        if self.mode != Mode::Read {
            return Err(RarError::Format(
                "edit requires an archive opened for reading".into(),
            ));
        }
        if force_rr.is_some_and(|percent| percent > 100) {
            return Err(RarError::InvalidOption(
                "recovery percent must be in 0..=100".into(),
            ));
        }
        if self.volume_paths.len() > 1 && (force_rr.is_some() || comment.is_some()) {
            return Err(RarError::Unsupported(
                "comment and recovery-record changes are not supported for multi-volume archives"
                    .into(),
            ));
        }
        self.ensure_write_ctx();
        if (!renames.is_empty() || force_rr.is_some() || comment.is_some())
            && self.main_header_is_locked()?
        {
            return Err(RarError::ArchiveLocked);
        }

        // Delete mask: duplicates are fine (a member can only be deleted
        // once); out-of-range indexes are callers' bugs, so surface them.
        let mut deleted = vec![false; self.entries.len()];
        let mut deleted_count = 0usize;
        for &idx in delete_indexes {
            if idx >= deleted.len() {
                return Err(RarError::StaleEntryId);
            }
            if !deleted[idx] {
                deleted[idx] = true;
                deleted_count += 1;
            }
        }
        for (idx, _) in renames {
            if *idx >= self.entries.len() {
                return Err(RarError::StaleEntryId);
            }
            if deleted[*idx] {
                return Err(RarError::InvalidOption(
                    "cannot rename a member that the same edit deletes".into(),
                ));
            }
        }

        let (map, renamed_count) = self.build_rename_map(renames)?;
        if deleted_count == 0 && renamed_count == 0 && force_rr.is_none() && comment.is_none() {
            return Err(RarError::Format("no members to edit".into()));
        }

        if deleted_count == self.entries.len() {
            // Matching `rar d`: deleting every member erases the archive
            // (every volume for multi-volume archives). Renames are empty
            // here — the disjointness check above leaves no target — and
            // comment/recovery changes would be silently dropped, so they
            // are refused too.
            if force_rr.is_some() || comment.is_some() {
                return Err(RarError::InvalidOption(
                    "cannot combine comment or recovery-record changes with deleting every member"
                        .into(),
                ));
            }
            if self.main_header_is_locked()? {
                return Err(RarError::ArchiveLocked);
            }
            for vol in &self.volume_paths {
                let _ = fs::remove_file(vol);
            }
            self.stream = None;
            self.entries.clear();
            self.read_ctx_mut().solid_state = None;
            self.read_ctx_mut().solid_decoded_through = -1;
            return Ok(EditSummary {
                deleted: deleted_count,
                renamed: 0,
            });
        }

        let chain = if deleted_count > 0 {
            let first_deleted = deleted.iter().position(|d| *d).unwrap();
            self.chain_range_around(first_deleted)
        } else {
            None
        };
        // The engine applies renames to verbatim copies only; a kept member
        // of a recompressed chain would silently keep its old name. Refuse
        // that combination up front. Deleted members are irrelevant (they
        // are not emitted), so only kept members in the chain matter.
        if let Some((start, end)) = chain {
            for idx in map.keys() {
                if !deleted[*idx] && (start..=end).contains(idx) {
                    return Err(RarError::Unsupported(
                        "renaming a member of a solid chain that also loses a member is not supported; split the edit into separate transactions"
                            .into(),
                    ));
                }
            }
        }

        self.rewrite_edit(deleted, chain, &map, force_rr, comment)?;
        Ok(EditSummary {
            deleted: deleted_count,
            renamed: renamed_count,
        })
    }

    /// Build the rename map (index -> new name) for resolved rename pairs,
    /// expanding directory renames to their descendants with the same rules
    /// as the name-based path. Returns the map and the number of explicit
    /// rename pairs.
    fn build_rename_map(
        &self,
        renames: &[(usize, String)],
    ) -> RarResult<(std::collections::HashMap<usize, String>, usize)> {
        let mut map: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
        let mut count = 0usize;
        for (idx, new) in renames {
            if *idx >= self.entries.len() {
                return Err(RarError::StaleEntryId);
            }
            let old_norm = map
                .get(idx)
                .map(|n| n.as_str())
                .unwrap_or(self.entries[*idx].name())
                .trim_end_matches('/')
                .to_string();
            let is_dir = self.entries[*idx].is_dir();
            let new_norm = new.trim_end_matches('/').to_string();
            if is_dir {
                map.insert(*idx, format!("{new_norm}/"));
                let prefix = format!("{old_norm}/");
                for (i, e) in self.entries.iter().enumerate() {
                    if i == *idx || map.contains_key(&i) {
                        continue;
                    }
                    if let Some(rest) = e.name().strip_prefix(&prefix) {
                        map.insert(i, format!("{new_norm}/{rest}"));
                    }
                }
            } else {
                map.insert(*idx, new_norm.clone());
            }
            count += 1;
        }
        Ok((map, count))
    }

    /// Run one staged edit rewrite (delete mask + rename map) against the
    /// original archive and reload the catalog. Single-volume archives are
    /// rewritten through a sibling file that replaces the original only on
    /// success; multi-volume archives are re-split at their volume size and
    /// their .rev recovery volumes regenerated.
    fn rewrite_edit(
        &mut self,
        deleted: Vec<bool>,
        chain: Option<(usize, usize)>,
        map: &std::collections::HashMap<usize, String>,
        force_rr: Option<u8>,
        comment: Option<&[u8]>,
    ) -> RarResult<()> {
        let rename_map = (!map.is_empty()).then_some(map);
        // Comment and recovery-record changes are validated out of the
        // multi-volume path above (rewrite_multivolume cannot carry them).
        debug_assert!(
            self.volume_paths.len() <= 1 || (force_rr.is_none() && comment.is_none()),
            "comment/recovery edits must be single-volume"
        );
        if self.volume_paths.len() > 1 {
            self.rewrite_multivolume(&deleted, chain, rename_map)?;
        } else {
            let src_path = self.path.clone();
            let tmp_path = temp_sibling_path(&src_path);
            let mut reader = File::open(&src_path)?;
            self.stream = Some(Box::new(read_write_create(&tmp_path)?));
            self.write_ctx_mut().quick_open_entries.clear();
            // Rewriting rediscovers header encryption from the file itself.
            self.header_encryption = false;
            self.archive_encr = None;

            let result = self.rewrite_blocks(
                &mut reader,
                &deleted,
                chain,
                force_rr,
                rename_map,
                comment,
                &src_path,
                &tmp_path,
            );
            self.stream = None;
            match result {
                Ok(()) => replace_file(&tmp_path, &src_path)?,
                Err(e) => {
                    let _ = fs::remove_file(&tmp_path);
                    return Err(e);
                }
            }
        }

        self.mode = Mode::Read;
        self.read_ctx_mut().solid_state = None;
        self.read_ctx_mut().solid_decoded_through = -1;
        self.open_read()?;
        Ok(())
    }

    /// Read the archive comment (the "CMT" service block), if any.
    ///
    /// Header-encrypted archives store the comment encrypted; reading it
    /// requires the password and is not supported yet.
    pub fn get_comment(&mut self) -> RarResult<Option<Vec<u8>>> {
        let mut reader = File::open(&self.path)?;
        reader.seek(SeekFrom::Start(self.sfx_offset + 8))?;
        while let Some(meta) =
            crate::rar50::headers::read_block(&mut reader, self.archive_block_key()?.as_ref())?
        {
            match meta.block_type {
                BLOCK_TYPE_END_ARCHIVE => break,
                BLOCK_TYPE_SERVICE_HEADER
                    if self.service_block_name(&meta)?.as_deref() == Some("CMT") =>
                {
                    let mut data = vec![0u8; meta.raw.data_size as usize];
                    reader.seek(SeekFrom::Start(meta.data_offset))?;
                    reader.read_exact(&mut data)?;
                    return Ok(Some(data));
                }
                _ => {}
            }
            // Advance past the data area.
            reader.seek(SeekFrom::Start(meta.data_end))?;
        }
        Ok(None)
    }

    /// Set the archive comment (like `rar c`); an empty comment removes
    /// the existing one.
    ///
    /// The comment is stored in a "CMT" service block right after the main
    /// header; the quick-open and recovery records are rebuilt over the
    /// rewritten archive. Multi-volume archives are not supported.
    pub fn set_comment(&mut self, comment: &[u8]) -> RarResult<()> {
        if self.mode != Mode::Read {
            return Err(RarError::Format(
                "set_comment requires an archive opened for reading".into(),
            ));
        }
        self.ensure_write_ctx();
        if self.volume_paths.len() > 1 {
            return Err(RarError::Unsupported(
                "archive comments are not supported for multi-volume archives".into(),
            ));
        }
        if self.main_header_is_locked()? {
            return Err(RarError::ArchiveLocked);
        }
        let src_path = self.path.clone();
        let tmp_path = temp_sibling_path(&src_path);
        let mut reader = File::open(&src_path)?;
        self.stream = Some(Box::new(read_write_create(&tmp_path)?));
        self.write_ctx_mut().quick_open_entries.clear();
        self.header_encryption = false;
        self.archive_encr = None;

        let deleted = vec![false; self.entries.len()];
        // `Some(empty)` removes the existing comment: the plan drops CMT
        // blocks and nothing new is written.
        let result = self.rewrite_blocks(
            &mut reader,
            &deleted,
            None,
            None,
            None,
            Some(comment),
            &src_path,
            &tmp_path,
        );
        self.stream = None;
        match result {
            Ok(()) => replace_file(&tmp_path, &src_path)?,
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(e);
            }
        }
        self.mode = Mode::Read;
        self.open_read()?;
        Ok(())
    }

    /// Parse the main archive header and report whether the archive is
    /// locked. Runs before any destructive step of [`Self::delete`] so the
    /// erase-everything path is covered too.
    pub(crate) fn main_header_is_locked(&mut self) -> RarResult<bool> {
        let mut reader = File::open(&self.path)?;
        reader.seek(SeekFrom::Start(self.sfx_offset + 8))?;
        self.header_encryption = false;
        self.archive_encr = None;
        let first =
            crate::rar50::headers::read_block(&mut reader, self.archive_block_key()?.as_ref())?
                .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
        let main = match first.block_type {
            BLOCK_TYPE_ENCRYPT_HEADER => {
                let params = parse_archive_encrypt_header(&first.raw)?;
                self.handle_archive_encrypt_header(params)?;
                crate::rar50::headers::read_block(&mut reader, self.archive_block_key()?.as_ref())?
                    .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?
            }
            BLOCK_TYPE_ARCHIVE_HEADER => first,
            _ => {
                return Err(RarError::Format(
                    "archive is missing the main header".into(),
                ));
            }
        };
        let ah = ArchiveHeader::from_raw(&main.raw)?;
        Ok(ah.flags & ARCHIVE_FLAG_LOCKED != 0)
    }

    /// Index range `[s, e]` of the solid chain affected by deleting member
    /// `idx`, when one exists.
    ///
    /// A member joins its predecessor's window when its header carries the
    /// solid flag, so the chain extends backwards while the entry at the
    /// boundary is solid and forwards while the next entry is solid (the
    /// first member of a chain is not flagged solid, matching the writer).
    /// Deleting the last member of a chain leaves the earlier members
    /// decodable (their windows are untouched), so no chain needs to be
    /// recompressed in that case.
    fn chain_range_around(&self, idx: usize) -> Option<(usize, usize)> {
        let mut s = idx;
        while s > 0 && self.entries[s].header.comp_solid {
            s -= 1;
        }
        let mut e = idx;
        while e + 1 < self.entries.len() && self.entries[e + 1].header.comp_solid {
            e += 1;
        }
        (idx < e).then_some((s, e))
    }

    /// Rewrite the archive file, omitting deleted members.
    ///
    /// `reader` reads the original archive; the rewritten bytes go to
    /// `self.stream` (the replacement file). With the `parallel` feature,
    /// verbatim block data is prefetched by a background thread and the
    /// affected solid chain is recompressed while the tail is already
    /// being read. Inline recovery records are rebuilt when the original
    /// had one (the percentage is carried over).
    #[allow(clippy::too_many_arguments)] // mirrors the delete() state machine
    pub(crate) fn rewrite_blocks(
        &mut self,
        reader: &mut File,
        deleted: &[bool],
        chain: Option<(usize, usize)>,
        force_rr: Option<u8>,
        rename_map: Option<&std::collections::HashMap<usize, String>>,
        comment: Option<&[u8]>,
        src_path: &Path,
        tmp_path: &Path,
    ) -> RarResult<()> {
        let plan = self.plan_rewrite(reader, deleted, chain, force_rr, rename_map, comment)?;
        self.execute_rewrite(&plan, src_path, tmp_path)?;
        Ok(())
    }

    /// Walk the archive and build the rewrite plan: verbatim copies for
    /// every kept block, recompression ops for the affected solid chain,
    /// and the dropped QO/RR service records (the RR percentage is parsed
    /// so the record can be rebuilt).
    fn plan_rewrite(
        &mut self,
        reader: &mut File,
        deleted: &[bool],
        chain: Option<(usize, usize)>,
        force_rr: Option<u8>,
        rename_map: Option<&std::collections::HashMap<usize, String>>,
        comment: Option<&[u8]>,
    ) -> RarResult<RewritePlan> {
        // Signature (after any embedded SFX stub).
        let mut sig = [0u8; 8];
        reader.seek(SeekFrom::Start(self.sfx_offset))?;
        reader.read_exact(&mut sig)?;

        // Leading blocks: optional archive encryption header (plaintext),
        // then the main archive header (rebuilt so the locator stays
        // consistent with the rewritten archive).
        let mut encrypt_header = None;
        let first = crate::rar50::headers::read_block(reader, self.archive_block_key()?.as_ref())?
            .ok_or_else(|| RarError::Format("archive is missing the main header".into()))?;
        let main_meta = match first.block_type {
            BLOCK_TYPE_ENCRYPT_HEADER => {
                let params = parse_archive_encrypt_header(&first.raw)?;
                self.handle_archive_encrypt_header(params)?;
                encrypt_header = Some(first.header_bytes);
                let main =
                    crate::rar50::headers::read_block(reader, self.archive_block_key()?.as_ref())?
                        .ok_or_else(|| {
                            RarError::Format("archive is missing the main header".into())
                        })?;
                if main.block_type != BLOCK_TYPE_ARCHIVE_HEADER {
                    return Err(RarError::Format(
                        "archive is missing the main header".into(),
                    ));
                }
                main
            }
            BLOCK_TYPE_ARCHIVE_HEADER => first,
            _ => {
                return Err(RarError::Format(
                    "archive is missing the main header".into(),
                ));
            }
        };

        // Decide quick-open capture and recovery rebuild from the main
        // header (write_main_header re-derives them from the same data).
        let ah = ArchiveHeader::from_raw(&main_meta.raw)?;
        let (had_qo, _had_rr, _) = split_main_extra(&ah.extra_data)?;
        let capture_qo = had_qo && !self.header_encryption;

        let mut ops = Vec::new();
        let mut entry_idx = 0usize;
        let mut chain_active = false;
        let mut chain_end = usize::MAX;
        let mut rr_percent = force_rr;
        // Service blocks flagged as dependent on the previous block (e.g.
        // NTFS streams/ACLs) belong to their file: drop them when that file
        // was deleted.
        let mut prev_file_deleted = false;

        while let Some(meta) =
            crate::rar50::headers::read_block(reader, self.archive_block_key()?.as_ref())?
        {
            match meta.block_type {
                BLOCK_TYPE_END_ARCHIVE => break,
                BLOCK_TYPE_FILE_HEADER => {
                    let idx = entry_idx;
                    entry_idx += 1;
                    if !chain_active
                        && let Some((s, e)) = chain
                        && s == idx
                    {
                        chain_active = true;
                        chain_end = e;
                    }
                    if chain_active && idx <= chain_end {
                        let entry = &self.entries[idx];
                        if entry.is_dir() || entry.header.comp_method == COMP_METHOD_STORE {
                            // Directories and STORE members never
                            // participate in the LZ window.
                            if !deleted[idx] {
                                ops.push(RewriteOp::CopyBlock {
                                    qo_header: None,
                                    header_bytes: meta.header_bytes,
                                    src_data: meta.data_offset,
                                    len: meta.raw.data_size,
                                });
                            }
                        } else {
                            ops.push(RewriteOp::Recompress {
                                idx,
                                is_deleted: deleted[idx],
                            });
                        }
                        if idx == chain_end {
                            chain_active = false;
                        }
                        prev_file_deleted = deleted[idx];
                    } else if deleted[idx] {
                        prev_file_deleted = true;
                    } else {
                        let header_bytes = match rename_map.and_then(|m| m.get(&idx)) {
                            Some(new_name) => {
                                let mut fh = self.entries[idx].header.clone();
                                fh.name = new_name.clone();
                                fh.to_bytes()
                            }
                            None => meta.header_bytes,
                        };
                        ops.push(RewriteOp::CopyBlock {
                            qo_header: if capture_qo {
                                Some(header_bytes.clone())
                            } else {
                                None
                            },
                            header_bytes,
                            src_data: meta.data_offset,
                            len: meta.raw.data_size,
                        });
                        prev_file_deleted = false;
                    }
                }
                BLOCK_TYPE_SERVICE_HEADER => {
                    let name = self.service_block_name(&meta)?;
                    if name.as_deref() == Some("RR") && rr_percent.is_none() {
                        rr_percent = self.rr_percent_from_block(&meta);
                    }
                    let drops = name.as_deref() == Some("QO")
                        || name.as_deref() == Some("RR")
                        || (comment.is_some() && name.as_deref() == Some("CMT"))
                        || (meta.flags & BLOCK_FLAG_DEPENDS_PREV != 0 && prev_file_deleted);
                    if !drops {
                        ops.push(RewriteOp::CopyBlock {
                            qo_header: None,
                            header_bytes: meta.header_bytes,
                            src_data: meta.data_offset,
                            len: meta.raw.data_size,
                        });
                    }
                }
                _ => ops.push(RewriteOp::CopyBlock {
                    qo_header: None,
                    header_bytes: meta.header_bytes,
                    src_data: meta.data_offset,
                    len: meta.raw.data_size,
                }),
            }
            // Advance past the data area (headers are read separately).
            reader.seek(SeekFrom::Start(meta.data_end))?;
        }

        Ok(RewritePlan {
            ops,
            encrypt_header,
            main_meta,
            rr_percent: rr_percent.filter(|p| *p <= 100),
            comment: comment.map(|c| c.to_vec()),
        })
    }

    /// Write the plan to `self.stream`: signature, archive encryption
    /// header, rebuilt main header, then every op in order, and finally
    /// the quick-open record, the main header locator patch, the recovery
    /// record and the end block.
    fn execute_rewrite(
        &mut self,
        plan: &RewritePlan,
        src_path: &Path,
        tmp_path: &Path,
    ) -> RarResult<()> {
        let out = self.stream.as_mut().unwrap();
        if self.sfx_offset > 0 {
            // Preserve the embedded SFX stub of the original archive.
            let mut stub = File::open(src_path)?;
            stub.seek(SeekFrom::Start(0))?;
            let mut limited = stub.take(self.sfx_offset + RAR5_SIGNATURE.len() as u64);
            io::copy(&mut limited, out)?;
        } else {
            out.write_all(RAR5_SIGNATURE)?;
        }
        if let Some(ref enc) = plan.encrypt_header {
            out.write_all(enc)?;
        }
        let (main_start, qo_field_pos, rr_field_pos, main_hdr) =
            self.write_main_header(&plan.main_meta, plan.rr_percent)?;
        if let Some(ref comment) = plan.comment
            && !comment.is_empty()
        {
            let block = build_comment_block(comment);
            self.write_block_header(&block)?;
        }

        // Prefetch every verbatim block with a background reader when the
        // total volume justifies the thread (parallel feature).
        #[cfg(feature = "parallel")]
        let mut pipeline: Option<CopyPipeline> = None;
        #[cfg(not(feature = "parallel"))]
        let pipeline: Option<()> = None;
        #[cfg(feature = "parallel")]
        {
            const PARALLEL_MIN_COPY: u64 = 32 * 1024 * 1024;
            let total_copy: u64 = plan
                .ops
                .iter()
                .map(|op| match op {
                    RewriteOp::CopyBlock { len, .. } => *len,
                    RewriteOp::Recompress { .. } => 0,
                })
                .sum();
            if total_copy >= PARALLEL_MIN_COPY && plan.ops.len() >= 4 {
                let jobs: Vec<CopyJob> = plan
                    .ops
                    .iter()
                    .filter_map(|op| match op {
                        RewriteOp::CopyBlock { src_data, len, .. } => Some(CopyJob {
                            src: *src_data,
                            len: *len,
                        }),
                        RewriteOp::Recompress { .. } => None,
                    })
                    .collect();
                pipeline = Some(CopyPipeline::start(src_path, &jobs));
            }
        }
        let mut reader = File::open(src_path)?;
        let mut dec = None;
        let mut enc = None;
        let mut enc_active = false;
        // Rewrite progress: `(processed_input_bytes, total_bytes)` where
        // total is the rewrite work (kept payload bytes + recompressed
        // input) and `processed` counts input bytes consumed, so the
        // fraction reaches 100% even when deletion shrinks the output.
        let total_bytes: u64 = plan
            .ops
            .iter()
            .map(|op| match op {
                RewriteOp::CopyBlock { len, .. } => *len,
                RewriteOp::Recompress { idx, .. } => self.entries[*idx].header.unpacked_size,
            })
            .sum();
        let mut processed = 0u64;

        for op in &plan.ops {
            self.check_cancel()?;
            if self.progress.is_some() {
                self.report_progress(processed, total_bytes);
            }
            match op {
                RewriteOp::CopyBlock {
                    header_bytes,
                    src_data,
                    len,
                    qo_header,
                } => {
                    let out_pos = self.stream.as_mut().unwrap().stream_position()?;
                    if let Some(qh) = qo_header {
                        self.write_ctx_mut()
                            .quick_open_entries
                            .push((out_pos, qh.clone()));
                    }
                    self.stream.as_mut().unwrap().write_all(header_bytes)?;
                    processed += len;
                    #[cfg_attr(not(feature = "parallel"), allow(unused_mut))]
                    let mut left = *len;
                    #[cfg(feature = "parallel")]
                    if let Some(pipe) = &pipeline {
                        while left > 0 {
                            let buf = pipe.take()?.ok_or_else(|| {
                                RarError::Io(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "copy pipeline ended early",
                                ))
                            })?;
                            self.stream.as_mut().unwrap().write_all(&buf)?;
                            left -= buf.len() as u64;
                        }
                    }
                    #[cfg(not(feature = "parallel"))]
                    let _ = &pipeline;
                    if left > 0 {
                        reader.seek(SeekFrom::Start(*src_data))?;
                        let mut limited = (&mut reader).take(left);
                        io::copy(&mut limited, self.stream.as_mut().unwrap())?;
                    }
                }
                RewriteOp::Recompress { idx, is_deleted } => {
                    if dec.is_none() {
                        let dict_log = self.entries[*idx].header.comp_dict_size;
                        let dict_size =
                            (128usize * 1024)
                                .checked_shl(dict_log as u32)
                                .ok_or_else(|| {
                                    RarError::Format(
                                        "dictionary size overflows host address space".into(),
                                    )
                                })?;
                        dec = Some(DecoderState::new(dict_size));
                        enc = Some(crate::codec::EncoderState::default());
                        enc_active = false;
                    }
                    self.recompress_chain_member(
                        &mut reader,
                        *idx,
                        *is_deleted,
                        dec.as_mut().unwrap(),
                        enc.as_mut().unwrap(),
                        &mut enc_active,
                    )?;
                    processed += self.entries[*idx].header.unpacked_size;
                }
            }
        }
        #[cfg(feature = "parallel")]
        if let Some(mut pipe) = pipeline {
            pipe.finish();
        }
        #[cfg(not(feature = "parallel"))]
        let _ = pipeline;

        if self.progress.is_some() {
            self.report_progress(processed, total_bytes);
        }

        // Quick-open record (rebuilt from the kept headers), locator patch
        // (with the recovery offset), recovery record and end block.
        let qo_pos = if self.write_ctx().quick_open {
            Some(self.write_quick_open_record()?)
        } else {
            None
        };
        let rr_pos = if self.recovery_percent.is_some() {
            Some(self.stream.as_mut().unwrap().stream_position()?)
        } else {
            None
        };
        if qo_pos.is_some() || rr_pos.is_some() {
            self.patch_main_header(
                qo_pos,
                rr_pos,
                main_start,
                qo_field_pos,
                rr_field_pos,
                &main_hdr,
            )?;
        }
        if rr_pos.is_some() {
            self.write_recovery_record_from(tmp_path)?;
        }
        self.write_end_block()?;
        Ok(())
    }

    /// Decode and recompress one member of the affected solid chain.
    ///
    /// Kept members are decoded with the shared decoder window and
    /// recompressed with a shared encoder window; deleted members are only
    /// decoded (to advance the window) and their blocks are not written.
    fn recompress_chain_member(
        &mut self,
        reader: &mut File,
        idx: usize,
        is_deleted: bool,
        dec: &mut DecoderState,
        enc: &mut crate::codec::EncoderState,
        enc_active: &mut bool,
    ) -> RarResult<()> {
        if is_deleted {
            let _ = self.decode_chain_member(reader, idx, dec)?;
            return Ok(());
        }
        let entry = self.entries[idx].clone();
        let hdr = &entry.header;
        let data = self.decode_chain_member(reader, idx, dec)?;

        let plain_crc = crc32fast::hash(&data);
        let plain_blake = hdr.hash_value.map(|_| crate::rar50::blake2sp::hash(&data));
        let variant = crate::version::ArchiveVersion::from_v70(hdr.dict_size_bytes.is_some());
        let packed = compression::encode_chunked(
            &data,
            compression::EncodeOptions {
                chunk_size: crate::codec::DEFAULT_CHUNK_SIZE,
                state: Some(enc),
                is_final: true,
                variant,
                ..compression::EncodeOptions::new(hdr.comp_method, hdr.comp_dict_size)
            },
        )?;

        let (method, dsl, dict_bytes, payload) = if packed.len() >= data.len() {
            // Compression is a net loss: STORE resets the chain, matching
            // the sequential add_file path.
            enc.reset();
            *enc_active = false;
            (COMP_METHOD_STORE, 0u8, None, data.clone())
        } else {
            *enc_active = true;
            (
                hdr.comp_method,
                hdr.comp_dict_size,
                hdr.dict_size_bytes,
                packed,
            )
        };
        let (header_crc, extra_data, stored_hash, encr_params) =
            RarArchive::payload_extra_and_crc(self.password.as_deref(), plain_crc, plain_blake)?;
        let payload = RarArchive::encrypt_payload_with(
            self.password.as_deref(),
            encr_params.as_ref(),
            &payload,
        )?;
        self.write_file_entry(
            &hdr.name,
            data.len() as u64,
            &payload,
            header_crc,
            method,
            dsl,
            dict_bytes,
            &extra_data,
            hdr.attributes,
            hdr.mtime,
            *enc_active,
            stored_hash,
        )?;
        Ok(())
    }

    /// Decode member `idx` with a shared decoder state, verifying its
    /// integrity. Reads directly from `reader` (the original archive) since
    /// `self.stream` is the replacement file during deletion.
    fn decode_chain_member(
        &mut self,
        reader: &mut File,
        idx: usize,
        state: &mut DecoderState,
    ) -> RarResult<Vec<u8>> {
        let hdr = &self.entries[idx].header;
        if hdr.packed_size == 0 && hdr.unpacked_size == 0 {
            return Ok(Vec::new());
        }
        let payload = self.read_packed_single(reader, idx)?;
        let mut raw_data = Vec::new();
        crate::rar50::payload::decode_member(
            &self.entries[idx].header,
            &payload,
            Some(state),
            &mut raw_data,
        )?;
        let crc = crc32fast::hash(&raw_data);
        let blake = self.entries[idx]
            .header
            .hash_value
            .map(|_| crate::rar50::blake2sp::hash(&raw_data));
        self.verify_integrity(
            idx,
            crc,
            blake,
            payload.params.as_ref(),
            payload.keys.as_ref(),
        )?;
        Ok(raw_data)
    }

    /// Read the packed (and decrypted, when applicable) payload of a
    /// single-volume member directly from `reader`.
    fn read_packed_single(&mut self, reader: &mut File, idx: usize) -> RarResult<DecryptedPayload> {
        let entry = &self.entries[idx];
        let hdr = &entry.header;
        let mut rr = crate::rar50::payload::SingleFileReader { reader };
        crate::rar50::payload::read_packed(
            &mut rr,
            hdr,
            &entry.chunks,
            &hdr.name,
            self.password.as_deref(),
            self.max_packed_bytes(),
            || Ok(()),
        )
    }

    /// Rebuild the main archive header for the rewritten archive: original
    /// flags, original extra records with the locator replaced by a fresh
    /// quick-open / recovery locator. Returns the header position, the
    /// plaintext offsets of the preallocated quick-open and recovery offset
    /// fields (patched at the end), and the full header bytes.
    #[allow(clippy::type_complexity)] // the header position + locator fields
    fn write_main_header(
        &mut self,
        meta: &BlockMeta,
        rr_percent: Option<u8>,
    ) -> RarResult<(u64, Option<usize>, Option<usize>, Vec<u8>)> {
        let ah = ArchiveHeader::from_raw(&meta.raw)?;
        if ah.flags & ARCHIVE_FLAG_LOCKED != 0 {
            return Err(RarError::ArchiveLocked);
        }
        let (had_qo, _had_rr, mut extra) = split_main_extra(&ah.extra_data)?;
        self.write_ctx_mut().quick_open = had_qo && !self.header_encryption;
        // The recovery record is rebuilt when the original archive had one
        // (or when the caller forces it, e.g. the `rr` command).
        self.recovery_percent = rr_percent;

        let mut arch_flags = ah.flags & !ARCHIVE_FLAG_RECOVERY;
        if self.recovery_percent.is_some() {
            arch_flags |= ARCHIVE_FLAG_RECOVERY;
        }

        let quick_open = self.write_ctx().quick_open;
        let recovery = self.recovery_percent.is_some();
        let (locator, qo_field_pos, rr_field_pos) =
            crate::rar50::headers::locator::build_locator_body(quick_open, recovery);
        // When neither QO nor RR is active, locator_flags == 0 and
        // the body is exactly 1 byte (the flags vint).  Omit the record
        // in that case, preserving the original conditional emit.
        if locator.len() > 1 {
            extra.extend(crate::rar50::headers::locator::frame_locator_record(
                &locator,
            ));
        }

        let mut block_flags = 0u64;
        if !extra.is_empty() {
            block_flags |= BLOCK_FLAG_EXTRA_DATA;
        }

        let mut body = Vec::new();
        body.extend(vint::encode(BLOCK_TYPE_ARCHIVE_HEADER));
        body.extend(vint::encode(block_flags));
        if block_flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            body.extend(vint::encode(extra.len() as u64));
        }
        body.extend(vint::encode(arch_flags));
        body.extend(&extra);

        let size_bytes = vint::encode(body.len() as u64);
        let mut header_content = Vec::with_capacity(size_bytes.len() + body.len());
        header_content.extend(&size_bytes);
        header_content.extend(&body);
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&header_content);
        let crc = hasher.finalize();
        let mut hdr = Vec::with_capacity(4 + header_content.len());
        hdr.extend(crc.to_le_bytes());
        hdr.extend(header_content);

        // Plaintext-relative offset of the locator offset fields inside the
        // locator body (see write_archive_header_with_locators).
        let field_base = 4usize
            + size_bytes.len()
            + vint::encoded_size(BLOCK_TYPE_ARCHIVE_HEADER)
            + vint::encoded_size(block_flags)
            + vint::encoded_size(extra.len() as u64)
            + vint::encoded_size(arch_flags)
            + vint::encoded_size(locator.len() as u64)
            + vint::encoded_size(crate::rar50::headers::locator::LOCATOR_TYPE);
        let qo_field_pos = qo_field_pos.map(|p| field_base + p);
        let rr_field_pos = rr_field_pos.map(|p| field_base + p);

        let main_start = self.stream.as_mut().unwrap().stream_position()?;
        self.write_block_header(&hdr)?;
        Ok((main_start, qo_field_pos, rr_field_pos, hdr))
    }

    /// Patch the rewritten main header with the real quick-open and/or
    /// recovery-record offsets and rewrite it in place (the offset fields
    /// were preallocated as fixed 5-byte vints, so the header length never
    /// changes).
    fn patch_main_header(
        &mut self,
        qo_offset: Option<u64>,
        rr_offset: Option<u64>,
        main_start: u64,
        qo_field_pos: Option<usize>,
        rr_field_pos: Option<usize>,
        main_hdr: &[u8],
    ) -> RarResult<()> {
        let base = self.sfx_offset + RAR5_SIGNATURE.len() as u64;
        let mut hdr = main_hdr.to_vec();
        crate::rar50::headers::locator::patch_locator_fields(
            &mut hdr,
            qo_offset,
            rr_offset,
            qo_field_pos,
            rr_field_pos,
            base,
        )?;
        self.stream
            .as_mut()
            .unwrap()
            .seek(SeekFrom::Start(main_start))?;
        self.write_block_header(&hdr)?;
        self.stream.as_mut().unwrap().seek(SeekFrom::End(0))?;
        Ok(())
    }

    /// Recovery percentage carried by a dropped "RR" service block
    /// (service data record type 0x07, single byte).
    pub(crate) fn rr_percent_from_block(&self, meta: &BlockMeta) -> Option<u8> {
        let data = &meta.raw.header_data;
        let mut offset = 0usize;
        let (_, n) = vint::decode_from_slice(data, offset).ok()?;
        offset += n;
        let (flags, n) = vint::decode_from_slice(data, offset).ok()?;
        offset += n;
        let mut extra_size = 0usize;
        if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            let (v, n) = vint::decode_from_slice(data, offset).ok()?;
            extra_size = v as usize;
            offset += n;
        }
        if flags & BLOCK_FLAG_DATA_AREA != 0 {
            let (_, n) = vint::decode_from_slice(data, offset).ok()?;
            offset += n;
        }
        // file flags, unpacked size, attributes, compression info, host OS
        for _ in 0..5 {
            let (_, n) = vint::decode_from_slice(data, offset).ok()?;
            offset += n;
        }
        let (name_len, n) = vint::decode_from_slice(data, offset).ok()?;
        offset += n + name_len as usize;
        if offset + extra_size > data.len() {
            return None;
        }
        let extra = &data[offset..offset + extra_size];
        let mut e = 0usize;
        let (rec_size, n) = vint::decode_from_slice(extra, e).ok()?;
        e += n;
        let rec_start = e;
        let (rec_type, n) = vint::decode_from_slice(extra, e).ok()?;
        let _ = n;
        if rec_type != 0x07 || rec_size == 0 {
            return None;
        }
        let data_end = rec_start + rec_size as usize;
        if data_end > extra.len() {
            return None;
        }
        Some(extra[data_end - 1])
    }

    /// Name of a service block (type 3), if parseable.
    pub(crate) fn service_block_name(&self, meta: &BlockMeta) -> RarResult<Option<String>> {
        let data = &meta.raw.header_data;
        let mut offset = 0usize;
        let (_, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("service block type: {e}")))?;
        offset += n;
        let (flags, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("service block flags: {e}")))?;
        offset += n;
        if flags & BLOCK_FLAG_EXTRA_DATA != 0 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("service block extra size: {e}")))?;
            offset += n;
        }
        if flags & BLOCK_FLAG_DATA_AREA != 0 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("service block data size: {e}")))?;
            offset += n;
        }
        // file flags, unpacked size, attributes, then fixed time/CRC32
        // fields, then compression info and host OS.
        let (file_flags, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("service block file flags: {e}")))?;
        offset += n;
        for _ in 0..2 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("service block field: {e}")))?;
            offset += n;
        }
        if file_flags & FILE_FLAG_TIME_UNIX != 0 {
            offset += 4;
        }
        if file_flags & FILE_FLAG_CRC32 != 0 {
            offset += 4;
        }
        for _ in 0..2 {
            let (_, n) = vint::decode_from_slice(data, offset)
                .map_err(|e| RarError::Format(format!("service block field: {e}")))?;
            offset += n;
        }
        let (name_len, n) = vint::decode_from_slice(data, offset)
            .map_err(|e| RarError::Format(format!("service block name: {e}")))?;
        offset += n;
        let end = (offset + name_len as usize).min(data.len());
        Ok(Some(
            String::from_utf8_lossy(&data[offset..end]).into_owned(),
        ))
    }
}
