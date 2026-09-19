//! Multi-volume rewrite: re-split kept members at the volume limit.
//!
//! `VolumeReaders` opens the original volumes lazily; the rewrite walks
//! the members in order, copies kept payloads verbatim and recompresses
//! the affected solid chain, then regenerates `.rev` recovery volumes.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use std::io::{Read, Seek, SeekFrom};

use super::super::RarArchive;
use crate::engine::MemberPlan;
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::FileHeader;
use crate::format::rar5::{COMP_METHOD_STORE, FILE_FLAG_DIRECTORY, FILE_FLAG_TIME_UNIX, OS_UNIX};
use crate::fs::atomic::{parent_dir, read_write_create, temp_suffix};

use super::super::{PendingCommit, volume_base_of, volume_path, volume_path_padded};

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
}

impl crate::format::rar5::payload::ChunkReader for VolumeReaders {
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
        // Grown by the read rather than pre-sized, like the other
        // `ChunkReader` implementations: `len` is a declared size, so it must
        // not drive an allocation on its own, and `take` bounds how much can
        // actually arrive. `try_from` keeps 32-bit targets honest.
        let len = usize::try_from(len)
            .map_err(|_| RarError::Format("chunk size does not fit in usize".into()))?;
        let mut buf = Vec::new();
        f.take(len as u64).read_to_end(&mut buf)?;
        Ok(buf)
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

impl RarArchive {
    /// Re-emit every "STM" stream record owned by kept member `idx` after its
    /// rebuilt block. Payloads are decoded through the original volumes
    /// (decrypting/decompressing when needed) and written as fresh STORE
    /// blocks; an originally encrypted stream is re-encrypted with its own
    /// ENCR record, a plain one stays plain even when a password is set.
    fn rewrite_surviving_streams(
        &mut self,
        readers: &mut VolumeReaders,
        idx: usize,
    ) -> RarResult<()> {
        let records: Vec<crate::archive::StreamRecord> = self
            .read_ctx()
            .streams
            .iter()
            .filter(|stream| stream.owner_index == idx)
            .cloned()
            .collect();
        if records.is_empty() {
            return Ok(());
        }
        let limit = self.read_ctx().extract_options.metadata_limit();
        let max_dict_size = self.read_ctx().extract_options.max_dict_size;
        let password = self.password.clone();
        let streams = crate::format::rar5::extract::read_streams_with(
            &records,
            readers,
            password.as_deref(),
            limit,
            max_dict_size,
        )?;
        for (name, data, was_encrypted) in streams {
            let password = if was_encrypted {
                password.as_deref()
            } else {
                None
            };
            crate::format::rar5::write::stream::write_stream_record(self, &name, data, password)?;
        }
        Ok(())
    }

    /// Rewrite a multi-volume archive, omitting deleted members.
    ///
    /// Kept members keep their exact compressed payloads but are re-split
    /// at the volume size limit (the official `rar` CLI refuses to modify
    /// multi-volume archives at all; this matches WinRAR's rebuild
    /// behavior). Solid chains are decoded and recompressed like in the
    /// single-volume path. Trailing QO/RR service records are dropped and
    /// `.rev` recovery volumes are regenerated.
    pub(super) fn rewrite_multivolume(
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
        // Read the archive comment before the stream is redirected to the
        // staged set; it is re-emitted verbatim after the rebuilt main
        // header so a delete does not silently drop it.
        let comment = self.get_comment()?;
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
        let parent = parent_dir(&self.path);
        // Recover a multi-volume commit another process was killed in the
        // middle of before staging this rewrite.
        crate::fs::atomic::recover_interrupted_commit(&parent, &base)?;
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
        self.write_ctx_mut().output.volume_size = Some(volume_size);
        self.volume_paths = vec![volume_path(&parent, &base, 1)];
        self.write_ctx_mut().output.current_volume = 1;
        self.write_ctx_mut().output.bytes_written = 0;
        self.write_ctx_mut().output.pending = Some(PendingCommit::Volumes {
            parent: parent.clone(),
            tmp_base: tmp_base.clone(),
            final_base: base.clone(),
        });
        self.stream = Some(Box::new(read_write_create(&volume_path(
            &parent, &tmp_base, 1,
        ))?));
        self.write_signature()?;
        self.write_archive_header_vol(None)?;
        self.write_ctx_mut().output.bytes_written =
            self.stream.as_mut().unwrap().stream_position()?;
        if let Some(comment) = &comment
            && !comment.is_empty()
        {
            let block = crate::format::rar5::headers::build_comment_block(comment);
            let eoa_size = self.on_disk_header_len(8);
            if self.write_ctx().output.bytes_written + block.len() as u64 + eoa_size > volume_size {
                return Err(RarError::Unsupported(
                    "rewriting a multi-volume archive whose comment does not fit in one volume is not supported"
                        .into(),
                ));
            }
            self.write_block_header(&block)?;
            self.write_ctx_mut().output.bytes_written =
                self.stream.as_mut().unwrap().stream_position()?;
        }

        let mut readers = VolumeReaders::new(&orig_volumes);
        let mut chain_state: Option<super::solid::SolidChainState> = None;
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
            self.check_cancel()?;
            if self.progress.is_some() {
                self.report_progress(processed, total_bytes);
            }
            if !in_chain
                && let Some((s, e)) = chain
                && s == idx
            {
                chain_state = Some(super::solid::SolidChainState::start(
                    crate::format::rar5::extract::decode::member_dict_window(self, idx)?,
                )?);
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
                    let _ = chain_state
                        .as_mut()
                        .unwrap()
                        .decode_member(self, &mut readers, idx)?;
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
                chain_state.as_mut().unwrap().recompress_member(
                    self,
                    &mut readers,
                    idx,
                    &entry_name,
                )?;
                self.rewrite_surviving_streams(&mut readers, idx)?;
                processed += entry.header.unpacked_size;
                continue;
            }

            // Verbatim payload, re-split across the new volumes. Encrypted
            // members come back decrypted; re-encrypt with the original
            // parameters (same salt/IV/key) so the copied ENCR record and
            // MAC'd header CRC stay valid and the stored bytes are exactly
            // the ciphertext that was read. The read side already derived
            // the keys, so no KDF runs here.
            let payload = self.read_member_packed(&mut readers, idx)?;
            processed += payload.data.len() as u64;
            let hdr = &entry.header;
            let stored_payload = match (payload.params.as_ref(), payload.keys.as_ref()) {
                (Some(params), Some(keys)) => {
                    crate::crypto::encrypt_data(&payload.data, &keys.key, &params.iv)
                }
                _ => payload.data,
            };
            crate::format::rar5::write::emit::write_file_entry(
                self,
                &MemberPlan {
                    name: entry_name,
                    unpacked_size: hdr.unpacked_size,
                    file_crc: hdr.crc32_val.unwrap_or(0),
                    method: hdr.comp_method,
                    dict_size_log: hdr.comp_dict_size,
                    dict_size_bytes: hdr.dict_size_bytes,
                    extra_data: hdr.extra_data.clone(),
                    attrs: hdr.attributes,
                    mtime: hdr.mtime,
                    solid: hdr.comp_solid,
                    stored_hash: hdr.hash_value,
                },
                &stored_payload,
            )?;
            self.rewrite_surviving_streams(&mut readers, idx)?;
        }
        if self.progress.is_some() {
            self.report_progress(processed, total_bytes);
        }
        self.write_end_block()?;
        self.stream = None;
        self.write_ctx_mut().output.volume_size = None;
        self.path = saved_path;

        // Move the new volumes (and regenerated `.rev` recovery volumes)
        // into place as one journaled commit. `commit_files` parks every
        // replaced or retired file first, so a failure or a process kill
        // mid-swap leaves either the complete new set or the untouched old
        // one — never a mix.
        let mut staged_paths: Vec<PathBuf> = Vec::new();
        let result = (|| -> RarResult<()> {
            // The staged set — not the original volume count — is
            // authoritative: a rewrite whose members grew (longer names,
            // additional volumes) can produce more volumes than the archive
            // had, and every one of them must be installed.
            let mut staged: Vec<(usize, PathBuf)> = Vec::new();
            for entry in fs::read_dir(&parent)? {
                let entry = entry?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Some(rest) = name.strip_prefix(&tmp_base) else {
                    continue;
                };
                let Some(rest) = rest.strip_prefix(".part") else {
                    continue;
                };
                let Some(num) = rest.strip_suffix(".rar") else {
                    continue;
                };
                let Ok(n) = num.parse::<usize>() else {
                    continue;
                };
                let path = entry.path();
                if !fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false) {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                staged_paths.push(path.clone());
                staged.push((n, path));
            }
            staged.sort_by_key(|(n, _)| *n);
            if staged.len() > 65535 {
                return Err(RarError::Format(format!(
                    "volume set of {} parts exceeds the RAR5 limit of 65535",
                    staged.len()
                )));
            }
            // WinRAR zero-pads the part number to the digit count of the
            // volume count (part01..part15); the installed set and the
            // regenerated `.rev` files share that canonical naming.
            let width = staged.len().to_string().len().max(1);
            let mut install: Vec<(PathBuf, PathBuf)> = Vec::new();
            let mut final_volumes: Vec<PathBuf> = Vec::new();
            for (n, tmp) in &staged {
                let final_path = volume_path_padded(&parent, &base, *n, width);
                install.push((tmp.clone(), final_path.clone()));
                final_volumes.push(final_path);
            }

            // Regenerate the `.rev` set from the staged volumes so the
            // recovery files commit together with the data volumes instead
            // of after them (a failure used to leave the data set committed
            // with stale or missing recovery files). WinRAR names `.rev`
            // files with the volume set's own padding (`part01.rev` for a
            // padded set), so probe the whole family like
            // `rev50::rebuild_missing_volumes` does.
            let orig_width = crate::fs::volume::volume_part_width(&orig_volumes[0]).max(1);
            let mut rev_probe: Option<PathBuf> = None;
            for w in [orig_width, 1, 2, 3, 4] {
                let probe = parent.join(format!("{base}.part{:0w$}.rev", 1, w = w));
                if probe.exists() {
                    rev_probe = Some(probe);
                    break;
                }
            }
            if let Some(rev) = rev_probe {
                let (rec_count, _data_count) = rev_params_from_file(&rev)?;
                let staged_data: Vec<PathBuf> =
                    install.iter().map(|(staged, _)| staged.clone()).collect();
                let rec_count = (rec_count as usize).min(staged_data.len());
                let written = crate::recovery::rev50::build_recovery_volumes_for_set(
                    &staged_data,
                    rec_count,
                )?;
                for (k, staged_rev) in written.into_iter().enumerate() {
                    staged_paths.push(staged_rev.clone());
                    let final_rev =
                        parent.join(format!("{base}.part{:0width$}.rev", k + 1, width = width));
                    install.push((staged_rev, final_rev));
                }
            }

            let keep: Vec<PathBuf> = install.iter().map(|(_, f)| f.clone()).collect();
            let retire = crate::fs::volume::stale_volume_paths(
                &parent,
                &base,
                false,
                &keep,
                &crate::recovery::rev3::rev_name_belongs_to_set,
            );
            crate::fs::atomic::commit_files(&parent, &base, &install, &retire)?;
            self.volume_paths = final_volumes;
            // Canonical padding can differ from the original name (an
            // unpadded set that grew past nine volumes); reopen from the
            // installed first volume so the catalog reload rediscovers the
            // new set from `self.path`.
            if let Some(first) = self.volume_paths.first() {
                self.path = first.clone();
            }
            Ok(())
        })();
        // `commit_files` installed the staged files (or restored them to
        // their staged names on rollback), so the drop guard must not touch
        // them again.
        self.write_ctx_mut().output.pending = None;
        if result.is_err() {
            for path in &staged_paths {
                let _ = fs::remove_file(path);
            }
            // A `.rev` generation failure may leave staged rev siblings that
            // never made it into `staged_paths`; sweep them by their unique
            // staged base.
            Self::remove_staged_recovery_files(&parent, &tmp_base);
        }
        result
    }
}
