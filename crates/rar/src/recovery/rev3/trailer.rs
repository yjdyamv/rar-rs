//! The 7-byte metadata trailer of the RAR 4.20+ `.rev` layout ([`super`] documents
//! both layouts and the repair flow).

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use super::repair::CHUNK;
use super::rs8::MAX_CODEWORD;

/// Metadata trailer length of the RAR 4.20+ `.rev` layout.
pub(super) const TRAILER_LEN: usize = 7;

/// Recovery-set metadata: how many data volumes the set has, how many
/// recovery volumes protect it, and which recovery volume a file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    pub data_count: usize,
    pub rec_count: usize,
    pub recovery_index: usize,
}

impl Meta {
    fn valid(&self) -> bool {
        self.data_count > 0
            && self.rec_count > 0
            && self.recovery_index < self.rec_count
            && self.data_count + self.rec_count <= MAX_CODEWORD
    }
}

/// Which of the two recovery layouts a set uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Format {
    /// Trailer format: parity protects `0..len - 7`; the tail is zeroed.
    Trailer,
    /// Legacy format: full-file parity, metadata in the name.
    Legacy,
}

/// Parse the 7-byte trailer of a trailer-format `.rev` file.
#[cfg(test)]
pub(super) fn parse_trailer(bytes: &[u8]) -> Option<Meta> {
    if bytes.len() < TRAILER_LEN {
        return None;
    }
    let tail = &bytes[bytes.len() - TRAILER_LEN..];
    let stored = u32::from_le_bytes(tail[3..7].try_into().ok()?);
    if crc32fast::hash(&bytes[..bytes.len() - 4]) != stored {
        return None;
    }
    let meta = Meta {
        data_count: usize::from(tail[0]) + 1,
        rec_count: usize::from(tail[1]) + 1,
        recovery_index: usize::from(tail[2]),
    };
    meta.valid().then_some(meta)
}

/// Streaming [`parse_trailer`]: the CRC covers `bytes[..len - 4]`, so it is
/// verified through a bounded read instead of materializing the file. I/O
/// failures surface as `Err` so callers can decide whether to skip the file.
pub(super) fn parse_trailer_reader(file: &mut fs::File) -> io::Result<Option<Meta>> {
    let len = file.metadata()?.len();
    if len < TRAILER_LEN as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(len - TRAILER_LEN as u64))?;
    let mut tail = [0u8; TRAILER_LEN];
    file.read_exact(&mut tail)?;
    let stored = u32::from_le_bytes(tail[3..7].try_into().unwrap());
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut remaining = len - 4;
    let mut buf = vec![0u8; CHUNK];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        file.read_exact(&mut buf[..want])?;
        hasher.update(&buf[..want]);
        remaining -= want as u64;
    }
    if hasher.finalize() != stored {
        return Ok(None);
    }
    let meta = Meta {
        data_count: usize::from(tail[0]) + 1,
        rec_count: usize::from(tail[1]) + 1,
        recovery_index: usize::from(tail[2]),
    };
    Ok(meta.valid().then_some(meta))
}

/// Parse the trailer of the `.rev` file at `path` through a bounded read.
pub(super) fn parse_trailer_file(path: &Path) -> io::Result<Option<Meta>> {
    let mut file = fs::File::open(path)?;
    parse_trailer_reader(&mut file)
}

/// Append the 7-byte trailer for `payload` to `out`.
#[cfg(test)]
pub(super) fn write_trailer(meta: &Meta, payload: &[u8], out: &mut Vec<u8>) {
    let head = [
        (meta.data_count - 1) as u8,
        (meta.rec_count - 1) as u8,
        meta.recovery_index as u8,
    ];
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(payload);
    hasher.update(&head);
    out.extend_from_slice(&head);
    out.extend_from_slice(&hasher.finalize().to_le_bytes());
}
