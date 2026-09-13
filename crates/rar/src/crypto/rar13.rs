//! RAR 1.3/1.4 member and comment cipher (ported from the `rars` project's
//! `crypto/rar13.rs`, WTFPL; see NOTICE).
//!
//! A three-byte key drives a byte-at-a-time additive stream; every password
//! character folds into the key and the stream advance, matching the
//! original DOS implementation.

use crate::crypto::clamp_password;

/// RAR 1.3/1.4 stream cipher.
#[derive(Clone)]
pub struct Rar13Cipher {
    key: [u8; 3],
}

impl Rar13Cipher {
    /// Build the cipher for a member password.
    pub fn new(password: &[u8]) -> Self {
        let password = clamp_password(password);
        let mut key = [0u8; 3];
        for &byte in password {
            key[0] = key[0].wrapping_add(byte);
            key[1] ^= byte;
            key[2] = key[2].wrapping_add(byte).rotate_left(1);
        }
        Self { key }
    }

    /// The fixed key RAR 1.3/1.4 uses for packed archive comments.
    pub fn new_comment() -> Self {
        Self { key: [0, 7, 77] }
    }

    /// Decrypt a whole buffer in place.
    pub fn decrypt_in_place(mut self, data: &mut [u8]) {
        for byte in data {
            *byte = self.decrypt_byte(*byte);
        }
    }

    /// Encrypt a whole buffer in place (write path and test vector parity
    /// with the port).
    pub fn encrypt_in_place(mut self, data: &mut [u8]) {
        for byte in data {
            *byte = self.encrypt_byte(*byte);
        }
    }

    /// Decrypt one byte, advancing the stream.
    pub fn decrypt_byte(&mut self, byte: u8) -> u8 {
        self.advance();
        byte.wrapping_sub(self.key[0])
    }

    /// Encrypt one byte, advancing the stream.
    pub fn encrypt_byte(&mut self, byte: u8) -> u8 {
        self.advance();
        byte.wrapping_add(self.key[0])
    }

    fn advance(&mut self) {
        self.key[1] = self.key[1].wrapping_add(self.key[2]);
        self.key[0] = self.key[0].wrapping_add(self.key[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::Rar13Cipher;

    #[test]
    fn rar13_cipher_matches_pinned_stream_vector() {
        let mut data = *b"hello world";
        Rar13Cipher::new(b"password").encrypt_in_place(&mut data);
        assert_eq!(
            data,
            [
                0x37, 0xcd, 0xaa, 0xbd, 0x10, 0x4e, 0x6f, 0x6e, 0xb5, 0x30, 0xe6
            ]
        );
        Rar13Cipher::new(b"password").decrypt_in_place(&mut data);
        assert_eq!(&data, b"hello world");
    }
}
