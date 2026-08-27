//! Bounded-memory emission machinery: counting/progress/CRC writer
//! adapters, the encrypted/plaintext payload emitter and the arbitrary-range
//! CBC emitter used for exact multi-volume splits.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::crypto;
use crate::error::{RarError, RarResult};
use crate::io_util::temp_suffix;
use crate::write_progress::ProgressTracker;
/// Wraps a writer and counts the bytes written through it.
pub(crate) struct CountingWriter<'a> {
    pub(crate) inner: &'a mut dyn Write,
    written: u64,
}

impl<'a> CountingWriter<'a> {
    pub(crate) fn new(inner: &'a mut dyn Write) -> Self {
        Self { inner, written: 0 }
    }

    pub(crate) fn written(&self) -> u64 {
        self.written
    }
}

impl Write for CountingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Wraps a writer and reports the member's written bytes through the shared
/// progress tracker after every write. `written` may be seeded with a
/// non-zero offset (multi-volume members resume their counter across volume
/// boundaries).
pub(crate) struct ProgressWriter<'a> {
    pub(crate) inner: &'a mut dyn Write,
    pub(crate) total: u64,
    pub(crate) written: u64,
    pub(crate) member: usize,
    pub(crate) progress: Option<std::sync::Arc<std::sync::Mutex<ProgressTracker>>>,
}

impl Write for ProgressWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        if let Some(progress) = &self.progress {
            let member = self.member;
            progress
                .lock()
                .expect("progress lock")
                .report(member, self.written, self.total);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// CRC32 sink for the streaming probe pass.
pub(crate) struct CrcSink<'a>(pub(crate) &'a mut crc32fast::Hasher);

impl Write for CrcSink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A member's payload in transit: plaintext passthrough or on-the-fly
/// AES-256-CBC encryption.
#[allow(clippy::large_enum_variant)] // the emitter holds an AES cipher + carry buffer
pub(crate) enum PayloadStream {
    Plain,
    Encrypted(CbcRangeEmitter),
}

pub(crate) fn payload_stream(key_iv: &Option<([u8; 32], [u8; 16])>) -> PayloadStream {
    match key_iv {
        Some((key, iv)) => PayloadStream::Encrypted(CbcRangeEmitter::new(key, iv)),
        None => PayloadStream::Plain,
    }
}

impl PayloadStream {
    /// Stream the payload bytes for plaintext range `[start, end)` of a
    /// member with `plain_len` plaintext bytes to `sink`. The bytes
    /// emitted are the member's on-disk data (the ciphertext when
    /// encrypted). Ranges must be issued in ascending order covering
    /// `[0, packed_len)` exactly.
    pub(crate) fn emit_to(
        &mut self,
        reader: &mut File,
        plain_len: u64,
        start: u64,
        end: u64,
        sink: &mut dyn Write,
    ) -> RarResult<()> {
        match self {
            PayloadStream::Plain => {
                reader.seek(SeekFrom::Start(start))?;
                let mut remaining = end - start;
                let mut buf = vec![0u8; 1 << 20];
                while remaining > 0 {
                    let want = buf.len().min(remaining as usize);
                    let mut filled = 0usize;
                    while filled < want {
                        let n = reader.read(&mut buf[filled..want])?;
                        if n == 0 {
                            return Err(RarError::Io(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                format!(
                                    "file changed size while being archived: still missing {remaining} bytes"
                                ),
                            )));
                        }
                        filled += n;
                    }
                    sink.write_all(&buf[..want]).map_err(RarError::Io)?;
                    remaining -= want as u64;
                }
                Ok(())
            }
            PayloadStream::Encrypted(emitter) => {
                emitter.emit_to(reader, plain_len, start, end, sink)
            }
        }
    }
}

/// Emits one member's continuous AES-256-CBC ciphertext in arbitrary byte
/// ranges. RAR5 volume chunks split the member's ciphertext at arbitrary
/// boundaries (WinRAR's volumes are byte-exact), so the encryptor — which
/// only produces complete 16-byte blocks — reads the plaintext ahead to
/// the next block boundary and carries the produced-but-unemitted tail
/// bytes (≤ 15) over to the following range.
pub(crate) struct CbcRangeEmitter {
    enc: crypto::Aes256CbcStream,
    /// Ciphertext bytes already produced but belonging to a later range.
    carry: Vec<u8>,
    /// Plaintext position consumed by the encryptor (block-aligned).
    consumed: u64,
}

impl CbcRangeEmitter {
    fn new(key: &[u8; 32], iv: &[u8; 16]) -> Self {
        Self {
            enc: crypto::Aes256CbcStream::new(key, iv),
            carry: Vec::new(),
            consumed: 0,
        }
    }

    /// Emit the ciphertext for plaintext range `[start, end)` of a member
    /// with `plain_len` plaintext bytes, zero-padding the member's final
    /// partial block (RAR5 padding). Emits exactly `end - start` bytes.
    pub(crate) fn emit_to(
        &mut self,
        reader: &mut File,
        plain_len: u64,
        start: u64,
        end: u64,
        sink: &mut dyn Write,
    ) -> RarResult<()> {
        // Bounded sub-ranges keep the read-ahead buffer at ~1 MiB even for
        // multi-GiB volume chunks; the carry keeps the stream continuous.
        const SUB: u64 = 1 << 20;
        let mut pos = start;
        while pos < end {
            let sub_end = (pos + SUB).min(end);
            let read_end = sub_end.div_ceil(16) * 16;
            let mut out = std::mem::take(&mut self.carry);
            if self.consumed < read_end {
                let total = (read_end - self.consumed) as usize;
                let mut buf = vec![0u8; total];
                let want = plain_len.saturating_sub(self.consumed).min(total as u64) as usize;
                if want > 0 {
                    reader.seek(SeekFrom::Start(self.consumed))?;
                    let mut filled = 0usize;
                    while filled < want {
                        let n = reader.read(&mut buf[filled..want])?;
                        if n == 0 {
                            return Err(RarError::Io(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                format!(
                                    "file changed size while being archived: expected {plain_len} plaintext bytes"
                                ),
                            )));
                        }
                        filled += n;
                    }
                }
                self.enc.encrypt_in_place(&mut buf)?;
                out.extend_from_slice(&buf);
                self.consumed = read_end;
            }
            let keep = (read_end - sub_end) as usize;
            let split = out.len() - keep;
            self.carry = out.split_off(split);
            sink.write_all(&out).map_err(RarError::Io)?;
            pos = sub_end;
        }
        Ok(())
    }
}

/// Removes a temporary spill file on drop (covers every error path).
pub(crate) struct SpillGuard(pub(crate) PathBuf);

impl Drop for SpillGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Temporary spill file for the streaming compressed path, kept next to
/// the archive being written.
pub(crate) fn spill_path_for(archive_path: &Path) -> PathBuf {
    let name = archive_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_string());
    archive_path.with_file_name(format!(".{name}.rar5spill-{}", temp_suffix()))
}
