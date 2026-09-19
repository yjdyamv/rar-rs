//! Checksums shared by the legacy families.
//!
//! RAR 1.3/1.4 stamps every file header with a 16-bit rolling checksum, and
//! RAR 1.5–4.x repeats it for a split member's intermediate fragments. Both
//! families therefore need it, and neither should own it: it lives here so
//! `rar4` does not reach into `rar13` (or the reverse) for one loop.

/// The legacy 16-bit rolling checksum (`sum + rotate-left 1` per byte).
pub(crate) fn rolling_sum_u16(data: &[u8]) -> u16 {
    let mut value = 0u16;
    for &byte in data {
        value = value.wrapping_add(u16::from(byte)).rotate_left(1);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::rolling_sum_u16;

    #[test]
    fn rolling_sum_matches_the_legacy_definition() {
        // Empty input has no contributions; a single 0x01 rotates to 0x02.
        assert_eq!(rolling_sum_u16(&[]), 0);
        assert_eq!(rolling_sum_u16(&[0x01]), 0x0002);
        assert_eq!(rolling_sum_u16(&[0x01, 0x02]), 0x0008);
        assert_eq!(rolling_sum_u16(&[0x40, 0x40, 0x40]), 0x0380);
    }
}
