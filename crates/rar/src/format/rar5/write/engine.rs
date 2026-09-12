//! RAR5 payload emission: the encrypted/plaintext stream selector and the
//! arbitrary-range AES-256-CBC emitter used for byte-exact multi-volume
//! splits. Counting/progress/CRC adapters are format-neutral and live in
//! [`crate::format::shared::engine`].

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::crypto;
use crate::error::{RarError, RarResult};

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
