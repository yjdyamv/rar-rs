//! RAR 5.0 recovery volumes (`.rev` files).
//!
//! A `.rev` file stores Reed-Solomon parity for a set of multi-volume
//! archives, using the same 16-bit GF(2^16) Cauchy codec as the inline
//! recovery record (WinRAR's `-rv` switch). Each `.rev` file protects the
//! whole volume set; up to `NR` missing/corrupt volumes can be rebuilt.

use super::rar50::encode_parity_shards;
use crate::error::{RarError, RarResult};
use crate::fs::atomic::read_up_to;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// REV5 file signature, distinct from the RAR archive marker.
pub const REV5_SIGNATURE: &[u8] = b"Rar!\x1aRev";

/// Fixed REV5 header length before the per-volume table: signature, stored
/// header CRC, header-size field, version, counts, rev number, payload CRC.
const REV5_FIXED_HEADER_LEN: u64 = 8 + 4 + 4 + 1 + 2 + 2 + 2 + 4;

/// Stripe size of the streaming `.rev` builder and rebuilder: one stripe of
/// the shard space is read, coded and written before the next is touched, so
/// memory stays O(stripe x volumes) instead of O(volumes x volume size).
/// Tests lower it through the internal `*_chunked` entry points to exercise
/// multi-stripe runs on small volume sets.
const CHUNK: u64 = 1024 * 1024;

/// Refuse non-RAR5 volume sets: legacy RAR 1.5–4.x sets are dispatched to
/// [`crate::recovery::rev3`] before this check runs, so anything still
/// reaching it (e.g. a RAR 1.3/1.4 `RE~^` set) is unsupported here.
fn ensure_rar5_volume_set(first_volume: &Path) -> RarResult<()> {
    let mut head = [0u8; 8];
    let mut file = fs::File::open(first_volume)?;
    let read = file.read(&mut head)?;
    if read >= 7 && head[..7] == *crate::detect::RAR4_SIGNATURE {
        return Err(RarError::Format(
            "legacy recovery volumes are handled by the RAR 1.5-4.x codec".into(),
        ));
    }
    if read >= 4 && head[..4] == *crate::detect::RAR13_SIGNATURE {
        return Err(RarError::Unsupported(
            "recovery volumes are not supported for RAR 1.3/1.4 archives".into(),
        ));
    }
    Ok(())
}

/// Number of `.rev` files for `data_count` volumes at `rec_percent`
/// (0-100): `max(1, ceil(pct * ND / 100))`, capped at `ND`.
pub fn plan_recovery_volume_count(data_count: usize, rec_percent: u64) -> RarResult<usize> {
    if data_count == 0 {
        return Err(RarError::Format(
            "no data volumes for recovery volumes".into(),
        ));
    }
    let nd = data_count as u64;
    let pct = rec_percent.min(100);
    let nr = (pct * nd).div_ceil(100).max(1).min(nd);
    Ok(nr as usize)
}

/// Serialize the REV5 header body (everything after the 4-byte size field):
/// version, counts, rev number, payload CRC and the per-volume size/CRC
/// table.
fn rev5_header_body(
    rec_index: usize,
    rec_count: usize,
    volume_sizes: &[u64],
    volume_crcs: &[u32],
    payload_crc: u32,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(11 + 12 * volume_sizes.len());
    body.push(1u8); // version
    body.extend((volume_sizes.len() as u16).to_le_bytes()); // data count
    body.extend((rec_count as u16).to_le_bytes());
    body.extend(((volume_sizes.len() + rec_index) as u16).to_le_bytes()); // rev number
    body.extend(payload_crc.to_le_bytes()); // payload CRC32
    for (&size, &crc) in volume_sizes.iter().zip(volume_crcs) {
        body.extend(size.to_le_bytes());
        body.extend(crc.to_le_bytes());
    }
    body
}

/// The 4-byte size field followed by [`rev5_header_body`]; the stored
/// header CRC covers exactly these bytes.
fn rev5_header_content(body: &[u8]) -> Vec<u8> {
    let mut content = Vec::with_capacity(4 + body.len());
    content.extend((body.len() as u32).to_le_bytes());
    content.extend(body);
    content
}

/// Serialize one `.rev` file: signature, header (with the per-volume
/// metadata table) and the parity payload.
pub fn build_recovery_volume_file(
    rec_index: usize,
    rec_count: usize,
    volume_sizes: &[u64],
    volume_crcs: &[u32],
    payload: &[u8],
) -> Vec<u8> {
    let body = rev5_header_body(
        rec_index,
        rec_count,
        volume_sizes,
        volume_crcs,
        crc32fast::hash(payload),
    );
    let header_content = rev5_header_content(&body);
    let header_crc = crc32fast::hash(&header_content);

    let mut out = Vec::with_capacity(8 + 4 + header_content.len() + payload.len());
    out.extend(REV5_SIGNATURE);
    out.extend(header_crc.to_le_bytes());
    out.extend(header_content);
    out.extend(payload);
    out
}

/// Verify the header CRC32 of a `.rev` file over the exact writer coverage:
/// [`build_recovery_volume_file`] hashes `header_content`, i.e. the 4-byte
/// header-size field plus the header body (file bytes `12..16 + hsize`),
/// and stores the CRC right after the signature (bytes `8..12`).
fn verify_rev5_header(data: &[u8], path: &Path) -> RarResult<()> {
    if data.len() < 16 {
        return Err(RarError::Format(format!(
            "{}: truncated recovery volume header",
            path.display()
        )));
    }
    let stored = u32::from_le_bytes(data[8..12].try_into().unwrap());
    let hsize = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let Some(content) = 16usize.checked_add(hsize).and_then(|end| data.get(12..end)) else {
        return Err(RarError::Format(format!(
            "{}: truncated recovery volume header",
            path.display()
        )));
    };
    if crc32fast::hash(content) != stored {
        return Err(RarError::Format(format!(
            "{}: recovery volume header CRC mismatch",
            path.display()
        )));
    }
    Ok(())
}

/// Parsed REV5 header. Only this bounded part (a 12-byte table entry per
/// data volume) of a `.rev` is held in memory; the parity payload is always
/// seeked and read stripe by stripe.
struct Rev5Header {
    data_count: usize,
    rec_count: usize,
    payload_crc: u32,
    /// Offset of the first payload byte.
    header_end: u64,
    volume_sizes: Vec<u64>,
    volume_crcs: Vec<u32>,
}

fn truncated_header(path: &Path) -> RarError {
    RarError::Format(format!(
        "{}: truncated recovery volume header",
        path.display()
    ))
}

/// Read and verify the header of the `.rev` file `file` (of length `len`)
/// through bounded reads, without materializing the payload.
fn read_rev5_header(file: &mut fs::File, path: &Path, len: u64) -> RarResult<Rev5Header> {
    let mut fixed = [0u8; 16];
    file.seek(SeekFrom::Start(0))?;
    let got = read_up_to(file, &mut fixed)?;
    if got < 12 || fixed[..8] != *REV5_SIGNATURE {
        return Err(RarError::Format(format!(
            "{}: not a RAR5 recovery volume",
            path.display()
        )));
    }
    if got < 16 || len < 16 {
        return Err(truncated_header(path));
    }
    let hsize = u32::from_le_bytes(fixed[12..16].try_into().unwrap()) as u64;
    let Some(header_end) = 16u64.checked_add(hsize) else {
        return Err(truncated_header(path));
    };
    if header_end > len {
        return Err(truncated_header(path));
    }
    let mut data = Vec::with_capacity(header_end as usize);
    data.extend_from_slice(&fixed);
    let mut body = vec![0u8; hsize as usize];
    file.read_exact(&mut body)?;
    data.extend_from_slice(&body);
    verify_rev5_header(&data, path)?;

    let body = &data[16..];
    if body.len() < 11 {
        return Err(truncated_header(path));
    }
    if body[0] != 1 {
        return Err(RarError::Format(
            "unsupported recovery volume version".into(),
        ));
    }
    let data_count = usize::from(u16::from_le_bytes(body[1..3].try_into().unwrap()));
    let rec_count = usize::from(u16::from_le_bytes(body[3..5].try_into().unwrap()));
    let payload_crc = u32::from_le_bytes(body[7..11].try_into().unwrap());
    let table_len = data_count.saturating_mul(12);
    let Some(table) = body.get(11..11 + table_len) else {
        return Err(truncated_header(path));
    };
    let mut volume_sizes = Vec::with_capacity(data_count);
    let mut volume_crcs = Vec::with_capacity(data_count);
    for entry in table.as_chunks::<12>().0 {
        volume_sizes.push(u64::from_le_bytes(entry[..8].try_into().unwrap()));
        volume_crcs.push(u32::from_le_bytes(entry[8..].try_into().unwrap()));
    }
    Ok(Rev5Header {
        data_count,
        rec_count,
        payload_crc,
        header_end,
        volume_sizes,
        volume_crcs,
    })
}

/// Feed exactly `len` bytes of `file` (from its current position) into
/// `hasher`, reading through a bounded buffer.
fn hash_exact(
    file: &mut fs::File,
    len: u64,
    hasher: &mut crc32fast::Hasher,
    buf: &mut [u8],
) -> io::Result<()> {
    let mut remaining = len;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        file.read_exact(&mut buf[..want])?;
        hasher.update(&buf[..want]);
        remaining -= want as u64;
    }
    Ok(())
}

/// CRC32 of the first `len` bytes of `path`, streamed in bounded chunks.
fn crc32_file(path: &Path, len: u64) -> RarResult<u32> {
    let mut file = fs::File::open(path)?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; CHUNK as usize];
    hash_exact(&mut file, len, &mut hasher, &mut buf)?;
    Ok(hasher.finalize())
}

/// [`rebuild_missing_volumes`] with a cancellation flag and progress
/// reporting. `progress` receives `(rebuilt_bytes, total_bytes)`, strictly
/// non-decreasing up to `total` on success; `cancel` is polled per chunk.
pub fn rebuild_missing_volumes_with(
    first_volume: &Path,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress: Option<&mut dyn FnMut(u64, u64)>,
) -> RarResult<Vec<PathBuf>> {
    rebuild_missing_volumes_chunked(first_volume, cancel, progress, CHUNK)
}

/// [`rebuild_missing_volumes_with`] with an explicit stripe size (tests
/// lower it to exercise multi-stripe runs on small volume sets).
fn rebuild_missing_volumes_chunked(
    first_volume: &Path,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
    chunk: u64,
) -> RarResult<Vec<PathBuf>> {
    use crate::recovery::rar50::reconstruct_data_shards;

    // Legacy RAR 1.5–4.x sets use their own recovery-volume codec.
    if crate::recovery::rev3::is_legacy_rev_set(first_volume)? {
        return crate::recovery::rev3::rebuild_missing_volumes(first_volume, cancel, progress);
    }

    ensure_rar5_volume_set(first_volume)?;

    let check_cancel = |cancel: Option<&std::sync::atomic::AtomicBool>| -> RarResult<()> {
        if cancel.is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed)) {
            return Err(RarError::Cancelled);
        }
        Ok(())
    };

    let base = crate::archive::volume_base_of(first_volume);
    let parent = first_volume
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();

    // Parse the first `.rev` file for the set parameters. WinRAR (and
    // `build_recovery_volumes_for_set`) pad the part number to the digit
    // count of the volume count (part01.rev .. part15.rev), so probe the
    // padding width from 1 to 4 digits.
    let mut rev1: Option<PathBuf> = None;
    let mut width = 1usize;
    for w in 1..=4 {
        let probe = parent.join(format!("{base}.part{:0w$}.rev", 1, w = w));
        if probe.exists() {
            rev1 = Some(probe);
            width = w;
            break;
        }
    }
    let Some(rev1) = rev1 else {
        return Err(RarError::Format(format!(
            "{}: no recovery volumes found",
            first_volume.display()
        )));
    };
    let mut rev1_file = fs::File::open(&rev1)?;
    let rev1_len = rev1_file.metadata()?.len();
    if rev1_len < REV5_FIXED_HEADER_LEN {
        return Err(RarError::Format(format!(
            "{}: not a RAR5 recovery volume",
            rev1.display()
        )));
    }
    let header = read_rev5_header(&mut rev1_file, &rev1, rev1_len)?;
    let data_count = header.data_count;
    let rec_count = header.rec_count;
    if data_count == 0 || rec_count == 0 || rec_count > 65535 - data_count {
        return Err(RarError::Format(
            "implausible recovery volume parameters".into(),
        ));
    }
    let volume_sizes = header.volume_sizes;
    let volume_crcs = header.volume_crcs;
    let max_len = *volume_sizes.iter().max().unwrap_or(&0);
    let padded_max = if max_len % 2 == 0 {
        max_len
    } else {
        max_len + 1
    };
    let first_payload_len = rev1_len - header.header_end;
    {
        // The stored payload CRC covers the whole payload, so it is
        // streamed through a bounded buffer instead of materializing the
        // (potentially multi-GB) first recovery volume.
        let mut hasher = crc32fast::Hasher::new();
        let mut buf = vec![0u8; chunk as usize];
        rev1_file.seek(SeekFrom::Start(header.header_end))?;
        hash_exact(&mut rev1_file, first_payload_len, &mut hasher, &mut buf)?;
        if hasher.finalize() != header.payload_crc {
            return Err(RarError::Format(
                "recovery volume payload CRC mismatch".into(),
            ));
        }
    }
    if first_payload_len != padded_max {
        return Err(RarError::Format(format!(
            "recovery volume payload size {} does not match the volume set ({padded_max})",
            first_payload_len
        )));
    }

    // Identify the surviving and missing data volumes.
    let mut survivors: Vec<Option<PathBuf>> = Vec::with_capacity(data_count);
    let mut missing: Vec<usize> = Vec::new();
    for i in 0..data_count {
        let vol = parent.join(format!("{base}.part{:0width$}.rar", i + 1, width = width));
        if vol.exists() {
            survivors.push(Some(vol));
        } else {
            survivors.push(None);
            missing.push(i);
        }
    }
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    if missing.len() > rec_count {
        return Err(RarError::Format(format!(
            "{} volume(s) missing but only {rec_count} recovery volume(s) available",
            missing.len()
        )));
    }

    // Open every `.rev` and parse its header through bounded reads; the
    // payloads stay on disk and are seeked per stripe.
    struct RevStream {
        file: fs::File,
        header_end: u64,
        payload_len: u64,
    }
    let mut rev_streams: Vec<RevStream> = Vec::with_capacity(rec_count);
    for k in 0..rec_count {
        let rev_path = parent.join(format!("{base}.part{:0width$}.rev", k + 1, width = width));
        let mut file = fs::File::open(&rev_path)?;
        let len = file.metadata()?.len();
        let header = read_rev5_header(&mut file, &rev_path, len)?;
        rev_streams.push(RevStream {
            file,
            header_end: header.header_end,
            payload_len: len - header.header_end,
        });
    }

    // Reconstructed volumes stream to temporary siblings, so neither a
    // rebuilt volume nor a `.rev` payload is ever held in memory.
    struct RebuiltOutput {
        index: usize,
        final_path: PathBuf,
        tmp: PathBuf,
        file: fs::File,
        written: u64,
    }
    let mut outputs: Vec<RebuiltOutput> = Vec::with_capacity(missing.len());
    for &index in &missing {
        let final_path = parent.join(format!(
            "{base}.part{:0width$}.rar",
            index + 1,
            width = width
        ));
        let tmp = crate::fs::atomic::temp_sibling_path(&final_path);
        match crate::fs::atomic::read_write_create(&tmp) {
            Ok(file) => outputs.push(RebuiltOutput {
                index,
                final_path,
                tmp,
                file,
                written: 0,
            }),
            Err(error) => {
                for output in &outputs {
                    let _ = fs::remove_file(&output.tmp);
                }
                return Err(RarError::Io(error));
            }
        }
    }

    let mut offset = 0u64;
    let result = (|| -> RarResult<()> {
        while offset < padded_max {
            check_cancel(cancel)?;
            if let Some(p) = progress.as_deref_mut() {
                p(offset, padded_max);
            }
            let want = (padded_max - offset).min(chunk) as usize;
            let mut data_shards: Vec<Option<Vec<u8>>> = Vec::with_capacity(data_count);
            for (i, vol) in survivors.iter().enumerate() {
                let Some(path) = vol else {
                    data_shards.push(None);
                    continue;
                };
                let mut buf = vec![0u8; want];
                let size = volume_sizes[i];
                if offset < size {
                    let to_read = (size - offset).min(want as u64) as usize;
                    let mut f = std::fs::File::open(path)?;
                    f.seek(SeekFrom::Start(offset))?;
                    let mut limited = f.take(to_read as u64);
                    limited.read_exact(&mut buf[..to_read])?;
                }
                data_shards.push(Some(buf));
            }
            let data_refs: Vec<Option<&[u8]>> = data_shards.iter().map(|s| s.as_deref()).collect();
            let mut rev_bufs: Vec<Vec<u8>> = Vec::with_capacity(rec_count);
            for (k, rev) in rev_streams.iter_mut().enumerate() {
                if offset + want as u64 > rev.payload_len {
                    return Err(RarError::Format(format!(
                        "recovery volume {} is shorter than the volume set it protects",
                        k + 1
                    )));
                }
                let mut buf = vec![0u8; want];
                rev.file.seek(SeekFrom::Start(rev.header_end + offset))?;
                rev.file.read_exact(&mut buf)?;
                rev_bufs.push(buf);
            }
            let recovery_shards: Vec<(usize, &[u8])> = rev_bufs
                .iter()
                .enumerate()
                .map(|(k, buf)| (k, buf.as_slice()))
                .collect();
            let all = reconstruct_data_shards(&data_refs, &recovery_shards)
                .map_err(|e| RarError::Format(format!("recovery volume reconstruction: {e}")))?;
            for (slot, &index) in missing.iter().enumerate() {
                outputs[slot].file.write_all(&all[index])?;
                outputs[slot].written += all[index].len() as u64;
            }
            offset += want as u64;
            if let Some(p) = progress.as_deref_mut() {
                p(offset.min(padded_max), padded_max);
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        for output in &outputs {
            let _ = fs::remove_file(&output.tmp);
        }
        return Err(error);
    }

    // Validate each rebuilt volume against its recorded size and CRC32,
    // then install it over the missing volume.
    let tmp_paths: Vec<PathBuf> = outputs.iter().map(|output| output.tmp.clone()).collect();
    let mut rebuilt_paths = Vec::with_capacity(outputs.len());
    for (slot, output) in outputs.into_iter().enumerate() {
        let RebuiltOutput {
            index,
            final_path,
            tmp,
            file,
            written,
        } = output;
        let size = volume_sizes[index];
        let expected_crc = volume_crcs[index];
        let step = (move || -> RarResult<PathBuf> {
            check_cancel(cancel)?;
            if written < padded_max {
                return Err(RarError::Format(format!(
                    "reconstructed volume {} is shorter than expected",
                    index + 1
                )));
            }
            file.set_len(size)?;
            file.sync_all()?;
            drop(file);
            let actual_crc = crc32_file(&tmp, size)?;
            if actual_crc != expected_crc {
                return Err(RarError::Crc {
                    expected: expected_crc,
                    actual: actual_crc,
                    context: format!("reconstructed volume {}", index + 1),
                });
            }
            crate::fs::atomic::replace_file(&tmp, &final_path)?;
            Ok(final_path)
        })();
        match step {
            Ok(path) => rebuilt_paths.push(path),
            Err(error) => {
                for path in &tmp_paths[slot..] {
                    let _ = fs::remove_file(path);
                }
                return Err(error);
            }
        }
    }
    Ok(rebuilt_paths)
}

/// Build `.rev` recovery volumes for an existing multi-volume set,
/// streaming all volumes in lockstep chunks (memory stays bounded at
/// O(chunk × volume count)).
///
/// `rec_count` is the exact number of `.rev` files to produce; it is
/// clamped to `10 × volume count` (WinRAR's `rv[N]` cap) and to the
/// 65535 total-volume limit of the format. The `.rev` files are named
/// after the set with the same zero-padding as the volumes
/// (`<base>.partNN.rev`), matching WinRAR. Each file is built under a
/// temporary sibling and installed only once the whole set is complete,
/// so a failure leaves existing `.rev` files untouched. Returns the
/// final paths.
pub fn build_recovery_volumes_for_set(
    volume_paths: &[PathBuf],
    rec_count: usize,
) -> RarResult<Vec<PathBuf>> {
    build_recovery_volumes_for_set_chunked(volume_paths, rec_count, CHUNK)
}

/// [`build_recovery_volumes_for_set`] with an explicit stripe size (tests
/// lower it to exercise multi-stripe runs on small volume sets).
fn build_recovery_volumes_for_set_chunked(
    volume_paths: &[PathBuf],
    rec_count: usize,
    chunk: u64,
) -> RarResult<Vec<PathBuf>> {
    let nd = volume_paths.len();
    if nd == 0 {
        return Err(RarError::Format("no volumes for recovery volumes".into()));
    }
    // Legacy RAR 1.5–4.x sets use their own recovery-volume codec.
    if crate::recovery::rev3::is_legacy_rev_set(&volume_paths[0])? {
        return crate::recovery::rev3::build_recovery_volumes_for_set(volume_paths, rec_count);
    }
    ensure_rar5_volume_set(&volume_paths[0])?;
    if nd > 65535 {
        return Err(RarError::Format(format!(
            "too many volumes ({nd}) for recovery volumes; maximum is 65535"
        )));
    }
    let rec_count = rec_count.min(nd * 10).max(1);
    if nd + rec_count > 65535 {
        return Err(RarError::Format(format!(
            "data ({nd}) + recovery ({rec_count}) volumes exceed the 65535 limit"
        )));
    }

    // Stream all volumes in lockstep stripes: per-stripe Reed-Solomon
    // parity keeps memory bounded at O(stripe x volumes) and CRCs are
    // computed in the same pass.
    let mut volume_sizes = Vec::with_capacity(nd);
    let mut readers = Vec::with_capacity(nd);
    let mut crcs = Vec::with_capacity(nd);
    for vol in volume_paths {
        let size = fs::metadata(vol)?.len();
        volume_sizes.push(size);
        readers.push(fs::File::open(vol)?);
        crcs.push(crc32fast::Hasher::new());
    }
    let max_len = *volume_sizes.iter().max().unwrap_or(&0);
    let padded_max = if max_len % 2 == 0 {
        max_len
    } else {
        max_len + 1
    };

    let base = crate::archive::volume_base_of(&volume_paths[0]);
    let parent = volume_paths[0].parent().unwrap_or(Path::new("."));
    // `.rev` names must carry the same padding as the volume set (which
    // comes from the file names, not the discovered count: a set with a
    // missing middle volume is discovered as a prefix but keeps its
    // original padding).
    let pad = crate::archive::volume_part_width(&volume_paths[0]).max(1);

    // Create the `.rev` files as temporary siblings and fill them stripe
    // by stripe; the header is backfilled in place once the volume CRCs
    // and the payload CRC are known. The temps are installed over the
    // final paths only after the whole parity set is built, so a failure
    // leaves existing `.rev` files untouched.
    struct RevOutput {
        final_path: PathBuf,
        tmp_path: PathBuf,
        file: fs::File,
        /// Header body with placeholder CRCs (finalized in place later).
        body: Vec<u8>,
        payload_crc: crc32fast::Hasher,
    }
    let zero_crcs = vec![0u32; nd];
    let mut outputs: Vec<RevOutput> = Vec::with_capacity(rec_count);
    for k in 0..rec_count {
        let rev_path = parent.join(format!("{base}.part{:0pad$}.rev", k + 1, pad = pad));
        let tmp_path = crate::fs::atomic::temp_sibling_path(&rev_path);
        let body = rev5_header_body(k, rec_count, &volume_sizes, &zero_crcs, 0);
        let header_content = rev5_header_content(&body);
        let create = (|| -> RarResult<fs::File> {
            let mut file = fs::File::create(&tmp_path)?;
            file.write_all(REV5_SIGNATURE)?;
            file.write_all(&0u32.to_le_bytes())?; // header CRC placeholder
            file.write_all(&header_content)?;
            Ok(file)
        })();
        match create {
            Ok(file) => outputs.push(RevOutput {
                final_path: rev_path,
                tmp_path,
                file,
                body,
                payload_crc: crc32fast::Hasher::new(),
            }),
            Err(error) => {
                for output in &outputs {
                    let _ = fs::remove_file(&output.tmp_path);
                }
                let _ = fs::remove_file(&tmp_path);
                return Err(error);
            }
        }
    }

    let result = (|| -> RarResult<()> {
        let mut offset = 0u64;
        while offset < padded_max {
            let want = (padded_max - offset).min(chunk) as usize;
            let mut chunk_bufs: Vec<Vec<u8>> = Vec::with_capacity(nd);
            for (i, reader) in readers.iter_mut().enumerate() {
                let mut buf = vec![0u8; want];
                if offset < volume_sizes[i] {
                    let to_read = (volume_sizes[i] - offset).min(want as u64) as usize;
                    let n = read_up_to(reader, &mut buf[..to_read])?;
                    if n != to_read {
                        return Err(RarError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!(
                                "volume {} shrank while building recovery volumes",
                                volume_paths[i].display()
                            ),
                        )));
                    }
                    crcs[i].update(&buf[..to_read]);
                    buf[to_read..].fill(0); // zero-pad to the stripe length
                }
                chunk_bufs.push(buf);
            }
            let refs: Vec<&[u8]> = chunk_bufs.iter().map(|b| b.as_slice()).collect();
            let parity = encode_parity_shards(&refs, rec_count)
                .map_err(|e| RarError::Format(format!("recovery volumes encode: {e}")))?;
            for (output, bytes) in outputs.iter_mut().zip(parity.iter()) {
                output.payload_crc.update(bytes);
                output.file.write_all(bytes)?;
            }
            offset += want as u64;
        }

        // Backfill the final header now that every CRC is known. The
        // payload length is fixed by the stripe loop, so rewriting the
        // header in place does not change the file layout.
        let volume_crcs: Vec<u32> = crcs.into_iter().map(|h| h.finalize()).collect();
        for output in outputs.iter_mut() {
            let payload_crc =
                std::mem::replace(&mut output.payload_crc, crc32fast::Hasher::new()).finalize();
            output.body[7..11].copy_from_slice(&payload_crc.to_le_bytes());
            for (i, crc) in volume_crcs.iter().enumerate() {
                let at = 11 + i * 12 + 8;
                output.body[at..at + 4].copy_from_slice(&crc.to_le_bytes());
            }
            let header_content = rev5_header_content(&output.body);
            let header_crc = crc32fast::hash(&header_content);
            output.file.seek(SeekFrom::Start(8))?;
            output.file.write_all(&header_crc.to_le_bytes())?;
            output.file.seek(SeekFrom::Start(12))?;
            output.file.write_all(&header_content)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        for output in &outputs {
            let _ = fs::remove_file(&output.tmp_path);
        }
        return Err(error);
    }

    // The whole parity set is built: install every temporary over its
    // final path. A failed install removes the remaining temps; a final
    // path is only ever replaced by a fully built `.rev`.
    let tmp_paths: Vec<PathBuf> = outputs
        .iter()
        .map(|output| output.tmp_path.clone())
        .collect();
    let mut written = Vec::with_capacity(outputs.len());
    for (slot, output) in outputs.into_iter().enumerate() {
        let RevOutput {
            final_path,
            tmp_path,
            file,
            ..
        } = output;
        drop(file);
        match crate::fs::atomic::replace_file(&tmp_path, &final_path) {
            Ok(()) => written.push(final_path),
            Err(error) => {
                for path in &tmp_paths[slot..] {
                    let _ = fs::remove_file(path);
                }
                return Err(error);
            }
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_crc_is_verified_but_does_not_cover_payload() {
        let payload = vec![0x5au8; 512];
        let file = build_recovery_volume_file(0, 1, &[1024], &[0xdead_beef], &payload);
        let path = Path::new("set.part1.rev");

        // Positive control: the freshly built header verifies.
        verify_rev5_header(&file, path).unwrap();

        // A flipped header content byte (here the version field) is caught.
        let mut corrupted = file.clone();
        corrupted[16] ^= 0x40;
        let err = verify_rev5_header(&corrupted, path).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "got {err}");

        // A flipped stored CRC is caught too.
        let mut corrupted = file.clone();
        corrupted[8] ^= 0x01;
        assert!(matches!(
            verify_rev5_header(&corrupted, path).unwrap_err(),
            RarError::Format(_)
        ));

        // The parity payload lies outside the verified header.
        let mut payload_flip = file;
        let last = payload_flip.len() - 1;
        payload_flip[last] ^= 0xff;
        verify_rev5_header(&payload_flip, path).unwrap();
    }

    #[test]
    fn truncated_header_is_rejected() {
        let file = build_recovery_volume_file(0, 1, &[1024], &[0xdead_beef], &[0u8; 64]);
        let err = verify_rev5_header(&file[..20], Path::new("x.rev")).unwrap_err();
        assert!(matches!(err, RarError::Format(_)), "got {err}");
    }

    /// Deterministic non-RAR5 byte patterns: the builders only inspect
    /// volume contents, so plain files exercise the `.rev` codec directly.
    fn write_fake_volumes(dir: &Path, sizes: &[u64]) -> Vec<PathBuf> {
        let mut volumes = Vec::with_capacity(sizes.len());
        for (i, &size) in sizes.iter().enumerate() {
            let path = dir.join(format!("set.part{}.rar", i + 1));
            let mut bytes = vec![0u8; size as usize];
            let mut state = 0x1234_5678u32.wrapping_add(i as u32 + 1);
            for byte in &mut bytes {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                *byte = (state >> 16) as u8;
            }
            fs::write(&path, &bytes).unwrap();
            volumes.push(path);
        }
        volumes
    }

    /// Staging temp names the builder may have leaked into `dir`.
    fn temp_leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("rar5tmp"))
            .collect()
    }

    /// Buffered reference for the streaming builder: the same lockstep
    /// parity computed by holding full zero-padded volume shards in memory.
    fn buffered_reference(volumes: &[PathBuf], rec_count: usize) -> Vec<Vec<u8>> {
        let sizes: Vec<u64> = volumes
            .iter()
            .map(|path| fs::metadata(path).unwrap().len())
            .collect();
        let max_len = *sizes.iter().max().unwrap();
        let padded_max = if max_len.is_multiple_of(2) {
            max_len
        } else {
            max_len + 1
        } as usize;
        let shards: Vec<Vec<u8>> = volumes
            .iter()
            .map(|path| {
                let mut bytes = fs::read(path).unwrap();
                bytes.resize(padded_max, 0);
                bytes
            })
            .collect();
        let refs: Vec<&[u8]> = shards.iter().map(Vec::as_slice).collect();
        let parity = encode_parity_shards(&refs, rec_count).unwrap();
        let crcs: Vec<u32> = volumes
            .iter()
            .map(|path| crc32fast::hash(&fs::read(path).unwrap()))
            .collect();
        (0..rec_count)
            .map(|k| build_recovery_volume_file(k, rec_count, &sizes, &crcs, &parity[k]))
            .collect()
    }

    #[test]
    fn streaming_build_matches_buffered_reference_over_stripes() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1024, 701, 1023, 100]);
        let expected = buffered_reference(&volumes, 8);

        // A 64-byte stripe makes the 1024-byte shard space span 16 stripes.
        let written = build_recovery_volumes_for_set_chunked(&volumes, 8, 64).unwrap();
        assert_eq!(written.len(), 8);
        for (k, path) in written.iter().enumerate() {
            assert_eq!(
                path.file_name().unwrap().to_string_lossy(),
                format!("set.part{}.rev", k + 1),
                "the builder must return the final paths"
            );
            assert_eq!(fs::read(path).unwrap(), expected[k], "rev {k}");
        }
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "successful build left temps: {:?}",
            temp_leftovers(dir.path())
        );
    }

    #[test]
    fn streaming_build_failure_leaves_existing_revs_and_removes_temps() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[512, 400]);
        // A pre-existing `.rev` for the second output: the failed build
        // must leave it byte-identical.
        let existing = dir.path().join("set.part2.rev");
        let keep = b"pre-existing parity".to_vec();
        fs::write(&existing, &keep).unwrap();
        // Occupy the first final path with a directory so installing the
        // first completed temp fails after the whole parity set is built.
        fs::create_dir(dir.path().join("set.part1.rev")).unwrap();

        let error = build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap_err();
        assert!(matches!(error, RarError::Io(_)), "got {error}");
        assert_eq!(
            fs::read(&existing).unwrap(),
            keep,
            "the pre-existing .rev must survive the failed build"
        );
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "temps left behind: {:?}",
            temp_leftovers(dir.path())
        );
    }

    #[test]
    fn streaming_rebuild_recovers_missing_volume_across_stripes() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1500, 900, 1100]);
        let original = fs::read(&volumes[1]).unwrap();
        build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap();

        fs::remove_file(&volumes[1]).unwrap();
        let rebuilt = rebuild_missing_volumes_chunked(&volumes[0], None, None, 64).unwrap();
        assert_eq!(rebuilt, vec![volumes[1].clone()]);
        assert_eq!(fs::read(&volumes[1]).unwrap(), original);
    }

    #[test]
    fn streaming_rebuild_rejects_first_recovery_volume_crc_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1500, 900, 1100]);
        let revs = build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap();

        let mut damaged = fs::read(&revs[0]).unwrap();
        let last = damaged.len() - 1;
        damaged[last] ^= 0xff;
        fs::write(&revs[0], &damaged).unwrap();
        fs::remove_file(&volumes[1]).unwrap();

        let error = rebuild_missing_volumes_chunked(&volumes[0], None, None, 64).unwrap_err();
        assert!(matches!(error, RarError::Format(_)), "got {error}");
    }
}
