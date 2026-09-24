//! Building the `.rev` recovery volumes of a data-volume set, and the
//! journaled install of a rebuilt set.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::error::{RarError, RarResult};

use super::layout::recovery_name_layout;
use super::map_coder;
use super::repair::{CHUNK, endarc_end, unique_bad_path};
use super::rs8::{MAX_CODEWORD, Rsc8};
use super::trailer::{Format, Meta, TRAILER_LEN};

/// Build `.rev` recovery volumes for an existing RAR 1.5–4.x volume set,
/// matching WinRAR's `rv` output byte-for-byte. Each file is built under a
/// temporary sibling and installed only once the whole set is complete, so
/// a failure leaves existing `.rev` files untouched; the final paths are
/// returned.
pub(crate) fn build_recovery_volumes_for_set(
    volume_paths: &[PathBuf],
    rec_count: usize,
) -> RarResult<Vec<PathBuf>> {
    build_recovery_volumes_for_set_chunked(volume_paths, rec_count, CHUNK)
}

/// [`build_recovery_volumes_for_set`] with an explicit stripe size (tests
/// lower it to exercise multi-stripe runs on small volume sets).
pub(super) fn build_recovery_volumes_for_set_chunked(
    volume_paths: &[PathBuf],
    rec_count: usize,
    chunk: usize,
) -> RarResult<Vec<PathBuf>> {
    let nd = volume_paths.len();
    if nd < 2 {
        return Err(RarError::invalid_option(
            "recovery volumes require a multi-volume archive",
        ));
    }
    if nd + rec_count > MAX_CODEWORD {
        return Err(RarError::invalid_option(format!(
            "legacy recovery volumes support at most {MAX_CODEWORD} data + recovery volumes"
        )));
    }
    let rec_count = rec_count.max(1);

    let (parent, layout, format, sizes) = recovery_name_layout(volume_paths)?;
    let shard_len = *sizes.iter().max().unwrap_or(&0);
    if shard_len < (TRAILER_LEN + 1) as u64 {
        return Err(RarError::format(
            "volumes are too small for recovery volumes",
        ));
    }
    let protected = match format {
        Format::Trailer => shard_len - TRAILER_LEN as u64,
        Format::Legacy => shard_len,
    };

    let coder = Rsc8::new(rec_count).map_err(map_coder)?;
    let mut readers = Vec::with_capacity(nd);
    for path in volume_paths {
        readers.push(fs::File::open(path)?);
    }

    // Create the `.rev` files as temporary siblings and fill them stripe
    // by stripe; the trailer (when the layout has one) is appended after
    // the last stripe. The set installs them as one transaction, so a
    // failure leaves existing `.rev` files untouched and no temps behind.
    let mut set = crate::recovery::parity::ParitySet::new(&parent, &layout.base)?;
    struct RevOutput {
        file: fs::File,
        meta: Meta,
        payload_crc: crc32fast::Hasher,
    }
    let mut outputs = Vec::with_capacity(rec_count);
    for k in 0..rec_count {
        let meta = Meta {
            data_count: nd,
            rec_count,
            recovery_index: k,
        };
        let path = match format {
            Format::Trailer => layout.trailer_rev_path(&parent, k),
            Format::Legacy => layout.legacy_rev_path(&parent, k, &meta),
        };
        let (_tmp, file) = set.stage(&path)?;
        outputs.push(RevOutput {
            file,
            meta,
            payload_crc: crc32fast::Hasher::new(),
        });
    }

    let result = (|| -> RarResult<()> {
        let mut offset = 0u64;
        while offset < protected {
            let want = (protected - offset).min(chunk as u64) as usize;
            let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(nd);
            for (index, reader) in readers.iter_mut().enumerate() {
                let mut chunk = vec![0u8; want];
                if offset < sizes[index] {
                    let to_read = (sizes[index] - offset).min(want as u64) as usize;
                    reader.seek(SeekFrom::Start(offset))?;
                    reader.read_exact(&mut chunk[..to_read])?;
                }
                chunks.push(chunk);
            }
            let mut parity: Vec<Vec<u8>> = vec![vec![0u8; want]; rec_count];
            let mut column = vec![0u8; nd];
            for position in 0..want {
                for (index, chunk) in chunks.iter().enumerate() {
                    column[index] = chunk[position];
                }
                let encoded = coder.encode(&column);
                for (index, byte) in encoded.into_iter().enumerate() {
                    parity[index][position] = byte;
                }
            }
            for (output, bytes) in outputs.iter_mut().zip(&parity) {
                output.payload_crc.update(bytes);
                output.file.write_all(bytes)?;
            }
            offset += want as u64;
        }

        // Trailer layout: the seven trailer bytes carry the counts and a
        // CRC over the payload plus the first three trailer bytes.
        if format == Format::Trailer {
            for output in outputs.iter_mut() {
                let head = [
                    (output.meta.data_count - 1) as u8,
                    (output.meta.rec_count - 1) as u8,
                    output.meta.recovery_index as u8,
                ];
                let mut hasher =
                    std::mem::replace(&mut output.payload_crc, crc32fast::Hasher::new());
                hasher.update(&head);
                output.file.write_all(&head)?;
                output.file.write_all(&hasher.finalize().to_le_bytes())?;
            }
        }
        Ok(())
    })();
    result?;

    // The whole parity set is built: close the staged files and install the
    // set as one transaction (ParitySet refuses a non-file final and sweeps
    // the temps on failure), so a failure cannot leave a half-replaced
    // parity set.
    for output in outputs {
        drop(output.file);
    }
    set.commit()
}

/// Finalize and install a set of rebuilt volumes as one journaled commit.
///
/// `outputs` holds `(volume index, staged path, write handle)` for each
/// rebuilt volume; a damaged original is parked as `*.bad` through the set's
/// journaled park, kept there on success and restored by rollback or by
/// `recover_interrupted_commit` after a kill. The park is recorded in the
/// commit journal before any rename, so a process kill between the park and
/// the install cannot strand the volume at `*.bad`. Staged files are removed
/// on drop when the commit never ran.
pub(super) fn commit_rebuilt_volumes(
    data_paths: &[PathBuf],
    damaged: &[usize],
    outputs: Vec<(usize, PathBuf, fs::File)>,
    last_index: usize,
    shard_len: u64,
) -> RarResult<Vec<PathBuf>> {
    let mut set = match crate::fs::atomic::StagedSet::new(
        &crate::fs::atomic::parent_dir(&data_paths[0]),
        &crate::fs::volume::volume_base_of(&data_paths[0]),
    ) {
        Ok(set) => set,
        Err(error) => {
            for (_, tmp, _) in &outputs {
                let _ = fs::remove_file(tmp);
            }
            return Err(error);
        }
    };
    // Adopt every staged rebuild before finalizing any of them: a failure at
    // any volume then removes them all on drop.
    for (index, tmp, _) in &outputs {
        set.track(tmp.clone(), &data_paths[*index]);
    }
    let mut rebuilt = Vec::with_capacity(outputs.len());
    for (index, tmp, file) in outputs {
        file.sync_all().map_err(RarError::Io)?;
        drop(file);
        // Truncate at the `ENDARC` block when everything after it is
        // zero padding (only the last volume of a set can be short).
        if index == last_index {
            let mut probe = fs::File::open(&tmp)?;
            if let Some(end) = endarc_end(&mut probe)?
                && end > 0
                && end < shard_len
            {
                let file = fs::File::options().write(true).open(&tmp)?;
                file.set_len(end).map_err(RarError::Io)?;
            }
        }
        let final_path = data_paths[index].clone();
        if damaged.contains(&index) && final_path.exists() {
            set.park(&final_path, unique_bad_path(&final_path));
        }
        rebuilt.push(final_path);
    }
    set.commit()?;
    Ok(rebuilt)
}
