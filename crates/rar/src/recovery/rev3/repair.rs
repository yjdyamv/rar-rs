//! Rebuilding missing/damaged data volumes from the recovery set (`rc`):
//! the damaged-volume scan, the RS recovery and the staged install.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{RarError, RarResult};
use crate::format::rar4::ENDARC_HEAD;

use super::build::commit_rebuilt_volumes;
use super::layout::{RecoverySet, collect_recovery_volumes, identify, resolve_data_slots};
use super::map_coder;
use super::rs8::Rsc8;
use super::trailer::{Format, Meta, TRAILER_LEN};

/// Streaming chunk for parity building and reconstruction.
pub(super) const CHUNK: usize = 1024 * 1024;

/// Rebuild missing and damaged volumes of a RAR 1.5–4.x set from its
/// `.rev` files (like WinRAR's `rc`). `path` may be any existing data
/// volume or any `.rev` of the set.
pub(crate) fn rebuild_missing_volumes(
    path: &Path,
    cancel: Option<&AtomicBool>,
    progress: Option<&mut dyn FnMut(u64, u64)>,
) -> RarResult<Vec<PathBuf>> {
    rebuild_missing_volumes_chunked(path, cancel, progress, CHUNK)
}

/// [`rebuild_missing_volumes`] with an explicit stripe size (tests lower it
/// to exercise multi-stripe runs on small volume sets).
pub(super) fn rebuild_missing_volumes_chunked(
    path: &Path,
    cancel: Option<&AtomicBool>,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
    chunk: usize,
) -> RarResult<Vec<PathBuf>> {
    let check_cancel = |cancel: Option<&AtomicBool>| -> RarResult<()> {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(RarError::Cancelled);
        }
        Ok(())
    };
    check_cancel(cancel)?;

    let (parent, layout, _) = identify(path)?;
    let RecoverySet {
        layout: set_layout,
        meta,
        format,
        payloads: revs,
    } = collect_recovery_volumes(&parent, &layout.base)?;
    if revs.is_empty() {
        return Err(RarError::Format("no recovery volumes found".into()));
    }
    let shard_len = revs.iter().map(|source| source.len).max().unwrap_or(0);
    if shard_len == 0 {
        return Err(RarError::Format("empty recovery volume".into()));
    }
    let protected = match format {
        Format::Trailer => shard_len.saturating_sub(TRAILER_LEN as u64),
        Format::Legacy => shard_len,
    };

    // Open every recovery volume once; the parity bytes are seeked per
    // stripe. Trailer-format footers read back as zeros.
    let zero_from = (format == Format::Trailer).then_some(protected);
    let mut rev_readers: Vec<(usize, fs::File, u64)> = Vec::with_capacity(revs.len());
    for source in &revs {
        rev_readers.push((source.index, fs::File::open(&source.path)?, source.len));
    }

    let (data_paths, missing) = resolve_data_slots(&parent, &set_layout, meta.data_count);
    let sizes: Vec<u64> = data_paths
        .iter()
        .map(|path| fs::metadata(path).map(|m| m.len()).unwrap_or(0))
        .collect();

    let missing_recovery: Vec<usize> = (0..meta.rec_count)
        .filter(|index| !revs.iter().any(|source| source.index == *index))
        .collect();
    let mut erasures: Vec<usize> = missing.clone();
    erasures.extend(missing_recovery.iter().map(|k| meta.data_count + k));
    erasures.sort_unstable();
    if erasures.len() > meta.rec_count {
        return Err(RarError::Format(format!(
            "{} volume(s) missing but only {} recovery volume(s) available",
            erasures.len(),
            meta.rec_count
        )));
    }

    let coder = Rsc8::new(meta.rec_count).map_err(map_coder)?;
    let codeword_len = meta.data_count + meta.rec_count;

    // Damage pass (WinRAR's "calculating checksums"): every protected
    // offset is checked; a whole corrupted volume shows up as a constant
    // set of error positions, located once and then treated as an erasure.
    let mut damaged: Vec<usize> = Vec::new();
    while let Some(positions) = locate_damage(
        &data_paths,
        &sizes,
        &missing,
        &mut rev_readers,
        &meta,
        &coder,
        protected,
        &erasures,
        cancel,
        chunk,
        zero_from,
    )? {
        let mut added = false;
        for position in positions {
            if !erasures.contains(&position) {
                // Damaged volumes are corrected as erasures (data and
                // recovery symbols alike).
                erasures.push(position);
                added = true;
            }
            if position < meta.data_count && !damaged.contains(&position) {
                damaged.push(position);
            }
        }
        erasures.sort_unstable();
        erasures.dedup();
        damaged.sort_unstable();
        if !added {
            return Err(RarError::Format(
                "recovery volumes cannot repair this damage".into(),
            ));
        }
        if erasures.len() > meta.rec_count {
            return Err(RarError::Format(
                "too many damaged or missing volumes for the recovery data".into(),
            ));
        }
    }

    let mut rebuild_indices: Vec<usize> = missing.clone();
    rebuild_indices.extend(damaged.iter().copied());
    rebuild_indices.sort_unstable();
    rebuild_indices.dedup();
    if rebuild_indices.is_empty() {
        return Ok(Vec::new());
    }

    // Stream the protected range, correcting every offset. Rebuilt volumes
    // go to temporary siblings first; the last volume is truncated at its
    // `ENDARC` block afterwards.
    let mut outputs: Vec<(usize, PathBuf, fs::File)> = Vec::new();
    for &index in &rebuild_indices {
        let final_path = data_paths[index].clone();
        let tmp = crate::fs::atomic::temp_sibling_path(&final_path);
        let file = crate::fs::atomic::read_write_create(&tmp)?;
        outputs.push((index, tmp, file));
    }

    let result = (|| -> RarResult<()> {
        let mut offset = 0u64;
        while offset < shard_len {
            check_cancel(cancel)?;
            if let Some(report) = progress.as_deref_mut() {
                report(offset, shard_len);
            }
            let want = (shard_len - offset).min(chunk as u64) as usize;
            let columns = load_chunk(
                &data_paths,
                &sizes,
                &missing,
                &mut rev_readers,
                meta.rec_count,
                offset,
                want,
                zero_from,
            )?;
            let mut codeword = vec![0u8; codeword_len];
            let mut rebuilt: Vec<Vec<u8>> = vec![Vec::with_capacity(want); rebuild_indices.len()];
            for position in 0..want {
                columns.codeword(position, &meta, &mut codeword);
                coder
                    .correct_erasures(&mut codeword, &erasures)
                    .map_err(map_coder)?;
                for (slot, &index) in rebuild_indices.iter().enumerate() {
                    let absolute = offset + position as u64;
                    let byte = if format == Format::Trailer && absolute >= protected {
                        // The trailer layout does not protect the tail; a
                        // rebuilt volume gets zeros there, like WinRAR.
                        0
                    } else {
                        codeword[index]
                    };
                    rebuilt[slot].push(byte);
                }
            }
            for (slot, (_, _, file)) in outputs.iter_mut().enumerate() {
                file.write_all(&rebuilt[slot]).map_err(RarError::Io)?;
            }
            offset += want as u64;
            if let Some(report) = progress.as_deref_mut() {
                report(offset.min(shard_len), shard_len);
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        for (_, tmp, _) in &outputs {
            let _ = fs::remove_file(tmp);
        }
        return Err(error);
    }

    // Finalize the staged rebuilds (sync + ENDARC truncate), park every
    // damaged original as `*.bad`, then install the whole set as one
    // journaled commit: either every rebuilt volume lands or none does. A
    // failure restores the parks, so a damaged original is never lost.
    commit_rebuilt_volumes(
        &data_paths,
        &damaged,
        outputs,
        meta.data_count - 1,
        shard_len,
    )
}

/// One chunk of every volume, loaded once per streaming step.
pub(super) struct ChunkColumns {
    /// `None` for a missing data volume; other chunks are zero-padded past
    /// their volume's length.
    data: Vec<Option<Vec<u8>>>,
    /// One zero-padded chunk per recovery volume (`None` when absent).
    revs: Vec<Option<Vec<u8>>>,
}

impl ChunkColumns {
    fn codeword(&self, position: usize, meta: &Meta, codeword: &mut [u8]) {
        codeword.fill(0);
        for (index, chunk) in self.data.iter().enumerate() {
            if let Some(chunk) = chunk {
                codeword[index] = chunk[position];
            }
        }
        for (k, chunk) in self.revs.iter().enumerate() {
            if let Some(chunk) = chunk {
                codeword[meta.data_count + k] = chunk[position];
            }
        }
    }
}

/// Fill `buf` with the parity bytes at `offset..offset + buf.len()` of a
/// `.rev` file. Bytes past the file (or past `zero_from`, the trailer
/// region of a trailer-format file) read as zero, matching the previous
/// in-memory zero-padded payloads.
pub(super) fn read_rev_range(
    file: &mut fs::File,
    len: u64,
    zero_from: Option<u64>,
    offset: u64,
    buf: &mut [u8],
) -> RarResult<()> {
    buf.fill(0);
    let end = offset.saturating_add(buf.len() as u64);
    let readable_end = end.min(len).min(zero_from.unwrap_or(u64::MAX));
    if readable_end > offset {
        let n = (readable_end - offset) as usize;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut buf[..n])?;
    }
    Ok(())
}

/// Load one chunk of every volume for a streaming pass.
#[allow(clippy::too_many_arguments)]
pub(super) fn load_chunk(
    slot_paths: &[PathBuf],
    sizes: &[u64],
    missing: &[usize],
    revs: &mut [(usize, fs::File, u64)],
    rec_count: usize,
    offset: u64,
    want: usize,
    zero_from: Option<u64>,
) -> RarResult<ChunkColumns> {
    let mut data = Vec::with_capacity(slot_paths.len());
    for (index, path) in slot_paths.iter().enumerate() {
        if missing.contains(&index) {
            data.push(None);
            continue;
        }
        let mut chunk = vec![0u8; want];
        if offset < sizes[index] {
            let to_read = (sizes[index] - offset).min(want as u64) as usize;
            let mut file = fs::File::open(path)?;
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut chunk[..to_read])?;
        }
        data.push(Some(chunk));
    }
    let mut rev_chunks: Vec<Option<Vec<u8>>> = vec![None; rec_count];
    for (k, file, len) in revs.iter_mut() {
        let mut chunk = vec![0u8; want];
        read_rev_range(file, *len, zero_from, offset, &mut chunk)?;
        rev_chunks[*k] = Some(chunk);
    }
    Ok(ChunkColumns {
        data,
        revs: rev_chunks,
    })
}

/// Scan every protected offset and return the error positions of the first
/// nonzero syndrome (whole corrupted volumes), or `None` when the set is
/// consistent with its parity.
#[allow(clippy::too_many_arguments)]
pub(super) fn locate_damage(
    slot_paths: &[PathBuf],
    sizes: &[u64],
    missing: &[usize],
    revs: &mut [(usize, fs::File, u64)],
    meta: &Meta,
    coder: &Rsc8,
    protected: u64,
    erasures: &[usize],
    cancel: Option<&AtomicBool>,
    chunk: usize,
    zero_from: Option<u64>,
) -> RarResult<Option<Vec<usize>>> {
    let mut codeword = vec![0u8; meta.data_count + meta.rec_count];
    let mut offset = 0u64;
    while offset < protected {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(RarError::Cancelled);
        }
        let want = (protected - offset).min(chunk as u64) as usize;
        let columns = load_chunk(
            slot_paths,
            sizes,
            missing,
            revs,
            meta.rec_count,
            offset,
            want,
            zero_from,
        )?;
        for position in 0..want {
            columns.codeword(position, meta, &mut codeword);
            if !erasures.is_empty() {
                coder
                    .correct_erasures(&mut codeword, erasures)
                    .map_err(map_coder)?;
            }
            let syndromes = coder.syndromes(&codeword);
            if syndromes.iter().any(|&value| value != 0) {
                let positions = coder
                    .locate_errors(&syndromes, codeword.len())
                    .map_err(map_coder)?;
                return Ok(Some(positions));
            }
        }
        offset += want as u64;
    }
    Ok(None)
}

/// Offset just past the `ENDARC` block when the bytes after it are all
/// zero, or `None` when the volume carries no parseable end block.
///
/// A rebuilt volume is plaintext at this point (header encryption is not
/// re-applied by this path), so the block walk never needs a password;
/// malformed input reads as "no end block" rather than an error.
pub(super) fn endarc_end(file: &mut fs::File) -> RarResult<Option<u64>> {
    let len = file.metadata()?.len();
    if len < 7 {
        return Ok(None);
    }
    let mut head = [0u8; 7];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut head)?;
    if head != *crate::detect::RAR4_SIGNATURE {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(7))?;
    loop {
        let block = match crate::format::rar4::read_block(
            file,
            false,
            None,
            crate::format::rar4::EnvelopePolicy::REPAIR,
        ) {
            Ok(Some(block)) => block,
            Ok(None) | Err(RarError::Format(_)) => return Ok(None),
            Err(other) => return Err(other),
        };
        if block.head_type != ENDARC_HEAD {
            continue;
        }
        let end = block.end();
        if end > len {
            return Ok(None);
        }
        let mut tail_len = len - end;
        let mut tail = [0u8; 64 * 1024];
        while tail_len > 0 {
            let want = tail_len.min(tail.len() as u64) as usize;
            file.read_exact(&mut tail[..want])?;
            if tail[..want].iter().any(|&byte| byte != 0) {
                return Ok(None);
            }
            tail_len -= want as u64;
        }
        return Ok(Some(end));
    }
}

/// First free `*.bad` sibling for a damaged volume (`x.r00` → `x.r00.bad`).
pub(super) fn unique_bad_path(path: &Path) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!("{ext}.bad"))
        .unwrap_or_else(|| "bad".to_string());
    let mut candidate = path.with_extension(extension);
    let mut counter = 2usize;
    while candidate.exists() {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!("{ext}.bad{counter}"))
            .unwrap_or_else(|| format!("bad{counter}"));
        candidate = path.with_extension(extension);
        counter += 1;
    }
    candidate
}
