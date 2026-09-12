//! RAR4 (RAR29) AES-128-CBC range emitter: emits one member's continuous
//! ciphertext in arbitrary byte ranges so volumes can split at byte-exact
//! boundaries.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use crate::crypto;
use crate::error::{RarError, RarResult};

/// Emits one RAR4 (RAR29, AES-128-CBC) member's ciphertext in arbitrary byte
/// ranges: the encryptor produces complete 16-byte blocks, so a range reads
/// the plaintext ahead to the next block boundary and carries the
/// produced-but-unemitted tail (≤ 15 bytes) over to the following range. The
/// member's final partial block is zero-padded (RAR4 convention, unlike
/// RAR5's PKCS#7-style padding).
pub(crate) struct Rar30RangeEmitter {
    cipher: crypto::Rar30Cipher,
    /// Plaintext position consumed by the encryptor (block-aligned).
    consumed: u64,
    /// Ciphertext bytes already produced but belonging to a later range.
    carry: Vec<u8>,
}

impl Rar30RangeEmitter {
    pub(crate) fn new(cipher: crypto::Rar30Cipher) -> Self {
        Self {
            cipher,
            consumed: 0,
            carry: Vec::new(),
        }
    }

    /// Append the ciphertext for plaintext range `[start, end)` of a member
    /// with `plain_len` plaintext bytes to `out`. Ranges must be issued in
    /// ascending order; the emitter keeps the CBC chain across them.
    pub(crate) fn emit_to(
        &mut self,
        reader: &mut File,
        plain_len: u64,
        start: u64,
        end: u64,
        out: &mut Vec<u8>,
    ) -> RarResult<()> {
        const SUB: u64 = 1 << 20;
        let mut pos = start;
        while pos < end {
            let sub_end = (pos + SUB).min(end);
            let read_end = sub_end.div_ceil(16) * 16;
            let mut buf = std::mem::take(&mut self.carry);
            if self.consumed < read_end {
                let total = (read_end - self.consumed) as usize;
                let mut block = vec![0u8; total];
                let want = plain_len.saturating_sub(self.consumed).min(total as u64) as usize;
                if want > 0 {
                    reader.seek(SeekFrom::Start(self.consumed))?;
                    let mut filled = 0usize;
                    while filled < want {
                        let n = reader.read(&mut block[filled..want])?;
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
                self.cipher
                    .encrypt_in_place(&mut block)
                    .map_err(|e| RarError::Format(format!("RAR4 member (RAR29) encrypt: {e}")))?;
                buf.extend_from_slice(&block);
                self.consumed = read_end;
            }
            let keep = (read_end - sub_end) as usize;
            let split = buf.len() - keep;
            self.carry = buf.split_off(split);
            out.extend_from_slice(&buf);
            pos = sub_end;
        }
        Ok(())
    }
}
