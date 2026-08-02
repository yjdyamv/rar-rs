/// RAR5 variable-length integer encoding/decoding.
///
/// Each byte contributes 7 data bits (bits 0-6). Bit 7 is a continuation
/// flag: 1 = more bytes follow, 0 = last byte. Little-endian order.
use std::io::{self, Read, Write};

/// Encode a `u64` as a RAR5 vint, returning the bytes.
pub fn encode(value: u64) -> Vec<u8> {
    let mut result = Vec::with_capacity(4);
    let mut v = value;
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        result.push(byte);
        if v == 0 {
            break;
        }
    }
    result
}

/// Write a vint to an `io::Write`.
pub fn write<W: Write>(w: &mut W, value: u64) -> io::Result<usize> {
    let bytes = encode(value);
    w.write_all(&bytes)?;
    Ok(bytes.len())
}

/// Decode a vint from an `io::Read`. Returns the decoded value.
pub fn read<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    for _ in 0..8 {
        let mut buf = [0u8; 1];
        r.read_exact(&mut buf)?;
        let byte = buf[0];
        result |= ((byte & 0x7F) as u64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "vint exceeds 8 bytes",
    ))
}

/// Decode a vint from a byte slice at `offset`.
/// Returns `(value, bytes_consumed)`.
pub fn decode_from_slice(data: &[u8], offset: usize) -> io::Result<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    let mut pos = offset;
    for _ in 0..8 {
        if pos >= data.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "buffer ended while reading vint",
            ));
        }
        let byte = data[pos];
        pos += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok((result, pos - offset));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "vint exceeds 8 bytes",
    ))
}

/// Number of bytes needed to encode `value` as a vint.
pub fn encoded_size(value: u64) -> usize {
    let mut v = value;
    let mut size = 0;
    loop {
        size += 1;
        v >>= 7;
        if v == 0 {
            return size;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small() {
        for v in 0..300u64 {
            let encoded = encode(v);
            let (decoded, len) = decode_from_slice(&encoded, 0).unwrap();
            assert_eq!(decoded, v);
            assert_eq!(len, encoded.len());
        }
    }

    #[test]
    fn roundtrip_large() {
        let values = [
            0,
            1,
            127,
            128,
            16383,
            16384,
            (1u64 << 49) - 1,
            (1u64 << 56) - 1,
        ];
        for &v in &values {
            let encoded = encode(v);
            let (decoded, _) = decode_from_slice(&encoded, 0).unwrap();
            assert_eq!(decoded, v);
        }
    }

    #[test]
    fn encoded_size_matches() {
        for v in [0u64, 1, 127, 128, 300, 16384, 1 << 20] {
            assert_eq!(encode(v).len(), encoded_size(v));
        }
    }

    #[test]
    fn stream_roundtrip() {
        let v = 12345u64;
        let mut buf = Vec::new();
        write(&mut buf, v).unwrap();
        let mut cursor = std::io::Cursor::new(&buf);
        let decoded = read(&mut cursor).unwrap();
        assert_eq!(decoded, v);
    }
}
