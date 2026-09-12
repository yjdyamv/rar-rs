//! Streaming repair of a damaged archive on disk.
//!
//! `repair_inline_recovery_archive_path` scans the source for `"RR"` chunks
//! without loading it whole, reconstructs the damaged shards and copies the
//! prefix ranges to the destination, so archives far larger than RAM can be
//! repaired.

use std::fs::File;
use std::io::{self, Read, Seek};

use super::repair::{
    InlineRecoveryChunk, parse_inline_recovery_chunk, repair_inline_recovery_prefix_shards,
};
use super::{Error, Result, check_cancel};

/// Streaming repair of a damaged RAR5 archive: scan `src` for the
/// inline recovery chunks, hold only the recovery data in memory, and
/// write the repaired archive to `dst`. Returns `true` when damage was
/// repaired, `false` when the archive was already intact (a pure copy).
///
/// Memory stays bounded (recovery data + damaged-shard outputs), so
/// archives far larger than RAM can be repaired. `dst` must not alias
/// `src`.
pub fn repair_inline_recovery_archive_path(
    src: &mut File,
    dst: &mut impl io::Write,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<bool> {
    let file_size = src.seek(io::SeekFrom::End(0)).map_err(io_err)?;
    src.seek(io::SeekFrom::Start(0)).map_err(io_err)?;
    // Total work is read-once (scan) + write-once (copy), so the progress
    // denominator is ~2x the file size; the per-phase reports below are
    // then strictly non-decreasing.
    let total = file_size.saturating_mul(2);
    let mut report = |done: u64, _: u64| {
        if let Some(p) = progress.as_deref_mut() {
            p(done, total);
        }
    };
    let found = find_inline_recovery_chunks_in_file(src, file_size, cancel, &mut |pos, _| {
        report(pos, total)
    })?;
    let first = found.first().ok_or(Error::BadRecoveryChunk)?;
    let protected_size =
        usize::try_from(first.1.protected_size).map_err(|_| Error::PlanOverflow)?;
    if protected_size as u64 > file_size {
        return Err(Error::BadRecoveryChunk);
    }
    let recovery_data: Vec<u8> = {
        let total: usize = found.iter().map(|(_, _, raw)| raw.len()).sum();
        let mut out = Vec::with_capacity(total);
        for (_, _, raw) in &found {
            out.extend_from_slice(raw);
        }
        out
    };

    let damaged = {
        let mut read_prefix = |range: std::ops::Range<usize>| -> Result<Vec<u8>> {
            src.seek(io::SeekFrom::Start(range.start as u64))
                .map_err(io_err)?;
            let mut buf = vec![0u8; range.len()];
            src.read_exact(&mut buf).map_err(io_err)?;
            Ok(buf)
        };
        repair_inline_recovery_prefix_shards(
            protected_size,
            &recovery_data,
            &mut read_prefix,
            cancel,
        )?
    };

    // Intact archive: nothing to write. `dst` is left untouched (like
    // `rar r`'s "All OK" — no fixed.<name> is produced).
    if damaged.is_empty() {
        return Ok(false);
    }

    // Copy the whole archive to dst, substituting the repaired bytes for
    // the damaged prefix ranges (bounded streaming I/O either way).
    // Progress runs from the scan result (the whole file) through the
    // copy phase, so it never moves backwards.
    report(file_size, total);
    src.seek(io::SeekFrom::Start(0)).map_err(io_err)?;
    let mut damaged = damaged;
    damaged.sort_by_key(|(range, _)| range.start);
    let mut next = 0usize;
    let mut copied = 0u64;
    for (range, repaired) in &damaged {
        copy_range(src, dst, next as u64, range.start as u64, cancel)?;
        copied += (range.start - next) as u64;
        report(file_size + copied, total);
        dst.write_all(repaired).map_err(io_err)?;
        copied += (range.end - range.start) as u64;
        report(file_size + copied, total);
        next = range.end;
    }
    copy_range(src, dst, next as u64, file_size, cancel)?;
    report(total, total);
    Ok(!damaged.is_empty())
}

/// Stream a byte range of `src` into `dst` in bounded blocks.
fn copy_range(
    src: &mut File,
    dst: &mut impl io::Write,
    start: u64,
    end: u64,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<()> {
    if end <= start {
        return Ok(());
    }
    src.seek(io::SeekFrom::Start(start)).map_err(io_err)?;
    let mut left = end - start;
    let mut buf = vec![0u8; 1 << 20];
    while left > 0 {
        check_cancel(cancel)?;
        let want = left.min(buf.len() as u64) as usize;
        let n = src.read(&mut buf[..want]).map_err(io_err)?;
        if n == 0 {
            return Err(Error::BadRecoveryChunk); // file shrank mid-copy
        }
        dst.write_all(&buf[..n]).map_err(io_err)?;
        left -= n as u64;
    }
    Ok(())
}

fn io_err(e: io::Error) -> Error {
    Error::Io(e.to_string())
}

/// Scan a file for valid `{RB}` recovery chunks, returning their
/// absolute offsets, parsed chunks and exact raw bytes. Streaming with
/// bounded reads; markers inside a validated chunk are skipped.
fn find_inline_recovery_chunks_in_file(
    src: &mut File,
    file_size: u64,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Vec<(u64, InlineRecoveryChunk, Vec<u8>)>> {
    const SCAN: usize = 64 * 1024;
    let mut found = Vec::new();
    let mut skip_until = 0u64;
    let mut tail = [0u8; 3];
    let mut tail_len = 0usize;
    let mut pos = 0u64;
    let mut buf = vec![0u8; SCAN];
    loop {
        check_cancel(cancel)?;
        let n = crate::fs::atomic::read_up_to(src, &mut buf).map_err(io_err)?;
        if n == 0 {
            break;
        }
        let base = pos.saturating_sub(tail_len as u64);
        let win_len = tail_len + n;
        let mut i = 0usize;
        while i + 4 <= win_len {
            let candidate = base + i as u64;
            if candidate < skip_until {
                i += 1;
                continue;
            }
            let b0 = if i < tail_len {
                tail[i]
            } else {
                buf[i - tail_len]
            };
            if b0 == b'{' {
                let b1 = if i + 1 < tail_len {
                    tail[i + 1]
                } else {
                    buf[i + 1 - tail_len]
                };
                let b2 = if i + 2 < tail_len {
                    tail[i + 2]
                } else {
                    buf[i + 2 - tail_len]
                };
                let b3 = if i + 3 < tail_len {
                    tail[i + 3]
                } else {
                    buf[i + 3 - tail_len]
                };
                if b1 == b'R'
                    && b2 == b'B'
                    && b3 == b'}'
                    && let Some((abs, chunk, raw)) = try_parse_chunk_at(src, file_size, candidate)?
                {
                    let shard_size = chunk.plan.shard_size;
                    skip_until = abs.saturating_add(shard_size);
                    found.push((abs, chunk, raw));
                }
                // `try_parse_chunk_at` seeks the stream to probe a
                // candidate; restore the scan position so `pos` below
                // stays the true file offset (it feeds both the next
                // read and the progress reports).
                src.seek(io::SeekFrom::Start(pos + n as u64))
                    .map_err(io_err)?;
            }
            i += 1;
        }
        tail_len = n.min(3);
        for (k, b) in buf[n.saturating_sub(3)..n].iter().enumerate() {
            tail[k] = *b;
        }
        pos += n as u64;
        progress(pos, file_size);
    }
    Ok(found)
}

/// Attempt to parse a `{RB}` chunk at absolute offset `abs`; `None` for
/// any structural/CRC violation.
fn try_parse_chunk_at(
    src: &mut File,
    file_size: u64,
    abs: u64,
) -> Result<Option<(u64, InlineRecoveryChunk, Vec<u8>)>> {
    if abs + 0x48 > file_size {
        return Ok(None);
    }
    src.seek(io::SeekFrom::Start(abs)).map_err(io_err)?;
    let mut hdr = vec![0u8; 0x48];
    src.read_exact(&mut hdr).map_err(io_err)?;
    if &hdr[..4] != b"{RB}" {
        return Ok(None);
    }
    let total_size = u32::from_le_bytes(hdr[0x0c..0x10].try_into().unwrap()) as u64;
    let header_size = u32::from_le_bytes(hdr[0x10..0x14].try_into().unwrap()) as u64;
    if header_size < 0x48 || total_size < header_size || abs.saturating_add(total_size) > file_size
    {
        return Ok(None);
    }
    src.seek(io::SeekFrom::Start(abs)).map_err(io_err)?;
    let mut raw = vec![0u8; total_size as usize];
    src.read_exact(&mut raw).map_err(io_err)?;
    match parse_inline_recovery_chunk(&raw) {
        Ok(chunk) => Ok(Some((abs, chunk, raw))),
        Err(_) => Ok(None),
    }
}
