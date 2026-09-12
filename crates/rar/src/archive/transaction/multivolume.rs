//! Multi-volume rewrite: re-split kept members at the volume limit.
//!
//! `VolumeReaders` opens the original volumes lazily; the rewrite walks
//! the members in order, copies kept payloads verbatim and recompresses
//! the affected solid chain, then regenerates `.rev` recovery volumes.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use std::io::{Read, Seek, SeekFrom};

use super::super::{DecryptedPayload, RarArchive};
use crate::codec::{DecoderState, lzss_huff as compression};
use crate::error::{RarError, RarResult};
use crate::format::rar5::headers::FileHeader;
use crate::format::rar5::{COMP_METHOD_STORE, FILE_FLAG_DIRECTORY, FILE_FLAG_TIME_UNIX, OS_UNIX};
use crate::fs::atomic::{read_write_create, temp_suffix};

use super::super::{PendingCommit, volume_base_of, volume_path};

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

impl crate::format::rar5::payload::ChunkReader for VolumeReaders {
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

impl RarArchive {
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
            self.check_cancel()?;
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
        self.write_ctx_mut().output.volume_size = None;
        self.path = saved_path;

        // Move the new volumes (and regenerated `.rev` recovery volumes)
        // into place as one journaled commit. `commit_files` parks every
        // replaced or retired file first, so a failure or a process kill
        // mid-swap leaves either the complete new set or the untouched old
        // one — never a mix.
        let mut staged_paths: Vec<PathBuf> = Vec::new();
        let result = (|| -> RarResult<()> {
            let mut install: Vec<(PathBuf, PathBuf)> = Vec::new();
            let mut final_volumes: Vec<PathBuf> = Vec::new();
            for n in 1..=orig_volumes.len() {
                let tmp = volume_path(&parent, &tmp_base, n);
                let exists = fs::metadata(&tmp).map(|m| m.len() > 0).unwrap_or(false);
                if !exists {
                    let _ = fs::remove_file(&tmp);
                    continue;
                }
                let final_path = volume_path(&parent, &base, n);
                staged_paths.push(tmp.clone());
                install.push((tmp, final_path.clone()));
                final_volumes.push(final_path);
            }

            // Regenerate the `.rev` set from the staged volumes so the
            // recovery files commit together with the data volumes instead
            // of after them (a failure used to leave the data set committed
            // with stale or missing recovery files).
            let rev = parent.join(format!("{base}.part1.rev"));
            if rev.exists() {
                let (rec_count, _data_count) = rev_params_from_file(&rev)?;
                let staged_volumes: Vec<PathBuf> =
                    install.iter().map(|(staged, _)| staged.clone()).collect();
                let rec_count = (rec_count as usize).min(staged_volumes.len());
                let written = crate::recovery::rev50::build_recovery_volumes_for_set(
                    &staged_volumes,
                    rec_count,
                )?;
                for (k, staged_rev) in written.into_iter().enumerate() {
                    staged_paths.push(staged_rev.clone());
                    let final_rev = parent.join(format!("{base}.part{}.rev", k + 1));
                    install.push((staged_rev, final_rev));
                }
            }

            let keep: Vec<PathBuf> = install.iter().map(|(_, f)| f.clone()).collect();
            let retire = crate::fs::volume::stale_volume_paths(&parent, &base, false, &keep);
            crate::fs::atomic::commit_files(&parent, &base, &install, &retire)?;
            self.volume_paths = final_volumes;
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
            if let Ok(entries) = fs::read_dir(&parent) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with(&tmp_base) && name.ends_with(".rev") {
                        let _ = fs::remove_file(entry.path());
                    }
                }
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
        crate::format::rar5::payload::read_packed(
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
        crate::format::rar5::payload::decode_member(
            &self.entries[idx].header,
            &payload,
            Some(state),
            &mut raw_data,
        )?;
        let crc = crc32fast::hash(&raw_data);
        let blake = self.entries[idx]
            .header
            .hash_value
            .map(|_| crate::format::rar5::blake2sp::hash(&raw_data));
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
        let plain_blake = hdr
            .hash_value
            .map(|_| crate::format::rar5::blake2sp::hash(&data));
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
}
