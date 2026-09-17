//! RAR4 member cipher range emitters: emit one member's continuous ciphertext
//! in arbitrary byte ranges so volumes can split at byte-exact boundaries.
//!
//! Every emitter is stateful and expects ranges in ascending order; the caller
//! (`Rar4PayloadSource`) feeds it the plaintext file (or the compressed spill)
//! and the member's plaintext length.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

use crate::crypto;
use crate::error::{RarError, RarResult};

/// A RAR4 member cipher that transforms whole 16-byte blocks (RAR 2.x, 3.x).
pub(crate) trait Rar4BlockCipher {
    /// Encrypt `block` (a multiple of 16 bytes) in place.
    fn encrypt_blocks(&mut self, block: &mut [u8]) -> RarResult<()>;
}

impl Rar4BlockCipher for crypto::Rar30Cipher {
    fn encrypt_blocks(&mut self, block: &mut [u8]) -> RarResult<()> {
        self.encrypt_in_place(block)
            .map_err(|e| RarError::Format(format!("RAR4 member (RAR29) encrypt: {e:?}")))
    }
}

impl Rar4BlockCipher for crypto::Rar20Cipher {
    fn encrypt_blocks(&mut self, block: &mut [u8]) -> RarResult<()> {
        self.encrypt_in_place(block)
            .map_err(|e| RarError::Format(format!("RAR4 member (RAR20) encrypt: {e}")))
    }
}

/// Emits a member's ciphertext in arbitrary byte ranges.
pub(crate) trait Rar4RangeEmitter {
    /// Append the ciphertext for plaintext range `[start, end)` of a member
    /// with `plain_len` plaintext bytes to `out`.
    fn emit_to(
        &mut self,
        reader: &mut File,
        plain_len: u64,
        start: u64,
        end: u64,
        out: &mut Vec<u8>,
    ) -> RarResult<()>;
}

/// Range emitter for a block cipher: the encryptor produces complete 16-byte
/// blocks, so a range reads the plaintext ahead to the next block boundary and
/// carries the produced-but-unemitted tail (≤ 15 bytes) over to the following
/// range. The member's final partial block is zero-padded (the RAR4
/// convention, unlike RAR5's PKCS#7-style padding).
pub(crate) struct Rar4BlockRangeEmitter<C: Rar4BlockCipher> {
    cipher: C,
    /// Plaintext position consumed by the encryptor (block-aligned).
    consumed: u64,
    /// Ciphertext bytes already produced but belonging to a later range.
    carry: Vec<u8>,
}

/// RAR 3.x (AES-128-CBC) member emitter.
pub(crate) type Rar30RangeEmitter = Rar4BlockRangeEmitter<crypto::Rar30Cipher>;
/// RAR 2.x member emitter.
pub(crate) type Rar20RangeEmitter = Rar4BlockRangeEmitter<crypto::Rar20Cipher>;

impl<C: Rar4BlockCipher> Rar4BlockRangeEmitter<C> {
    pub(crate) fn new(cipher: C) -> Self {
        Self {
            cipher,
            consumed: 0,
            carry: Vec::new(),
        }
    }
}

impl<C: Rar4BlockCipher> Rar4RangeEmitter for Rar4BlockRangeEmitter<C> {
    fn emit_to(
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
                self.cipher.encrypt_blocks(&mut block)?;
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

/// Range emitter for the RAR 1.5 XOR stream cipher: it has no block structure,
/// so a range only has to advance the keystream to the range start.
pub(crate) struct Rar15RangeEmitter {
    cipher: crypto::Rar15Cipher,
    /// Plaintext position the keystream is positioned at.
    position: u64,
}

impl Rar15RangeEmitter {
    pub(crate) fn new(cipher: crypto::Rar15Cipher) -> Self {
        Self {
            cipher,
            position: 0,
        }
    }
}

impl Rar4RangeEmitter for Rar15RangeEmitter {
    fn emit_to(
        &mut self,
        reader: &mut File,
        plain_len: u64,
        start: u64,
        end: u64,
        out: &mut Vec<u8>,
    ) -> RarResult<()> {
        if start < self.position {
            return Err(RarError::Format(format!(
                "RAR4 member (RAR15) cipher ranges must be ascending: {start} after {}",
                self.position
            )));
        }
        if start > self.position {
            self.cipher.skip(start - self.position);
            self.position = start;
        }
        let want = end.min(plain_len).saturating_sub(start) as usize;
        let mut buf = vec![0u8; (end - start) as usize];
        if want > 0 {
            reader.seek(SeekFrom::Start(start))?;
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
            self.cipher.crypt_in_place(&mut buf[..want]);
        }
        self.position = end;
        out.extend_from_slice(&buf);
        Ok(())
    }
}
