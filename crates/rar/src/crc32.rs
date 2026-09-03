//! Shared RAR CRC-32 primitives.
//!
//! RAR4 (and the legacy RAR1.5–4.x ciphers) are built directly on the CRC-32
//! table: header CRCs are the low 16 bits of a CRC, and the RAR15/RAR20
//! stream ciphers step the table as their key schedule. `crc32fast` handles
//! bulk checksums elsewhere, but those consumers need the raw table entries,
//! which live here.

/// Compute the standard (IEEE, reflected 0xEDB88320) CRC-32 of `input`.
pub fn crc32(input: &[u8]) -> u32 {
    let mut crc = crc32fast::Hasher::new();
    crc.update(input);
    crc.finalize()
}

/// Uninverted running CRC as the RAR4 container and legacy ciphers use it.
pub fn crc32_raw(input: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in input {
        crc = (crc >> 8) ^ table_entry((crc as u8) ^ byte);
    }
    crc
}

/// The `index`-th entry of the reflected CRC-32 lookup table.
pub fn table_entry(index: u8) -> u32 {
    TABLE[index as usize]
}

const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut value = i as u32;
        let mut bit = 0;
        while bit < 8 {
            let mask = 0u32.wrapping_sub(value & 1);
            value = (value >> 1) ^ (0xedb8_8320 & mask);
            bit += 1;
        }
        table[i] = value;
        i += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn raw_crc_matches_unfinalized_seeded_rar15_value() {
        assert_eq!(crc32_raw(b"password"), 0xca3d_b92a);
    }

    #[test]
    fn raw_crc_consistent_with_crc32fast() {
        for input in [&b"hello"[..], b"rar-rs crc32", &[]] {
            assert_eq!(!crc32_raw(input), crc32(input));
        }
    }

    #[test]
    fn table_entry_matches_bitwise_generation() {
        assert_eq!(table_entry(0), 0);
        assert_eq!(table_entry(1), 0x7707_3096);
        assert_eq!(table_entry(0xff), 0x2d02_ef8d);
    }
}
