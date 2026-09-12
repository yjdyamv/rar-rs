//! Bounded-memory writer adapters shared by both containers' write paths:
//! byte counting, progress reporting, CRC sinks/sources and the temporary
//! spill-file guard.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::fs::atomic::temp_suffix;
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

/// Reader adapter that hashes everything it hands out. The RAR4 streaming
/// path computes the member's plaintext CRC during the compression pass, so
/// the source file is read once.
pub(crate) struct CrcReader<R> {
    pub(crate) inner: R,
    pub(crate) hasher: crc32fast::Hasher,
}

impl<R: Read> Read for CrcReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// Removes a temporary spill file on drop (covers every error path).
pub(crate) struct SpillGuard(pub(crate) PathBuf);

impl Drop for SpillGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Temporary spill file for the streaming compressed paths, kept next to
/// the archive being written.
pub(crate) fn spill_path_for(archive_path: &Path) -> PathBuf {
    let name = archive_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".to_string());
    archive_path.with_file_name(format!(".{name}.spill-{}", temp_suffix()))
}
