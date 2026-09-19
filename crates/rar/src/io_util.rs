//! Generic `std::io` helpers that belong to no single layer.
//!
//! [`read_up_to`] used to live in `fs::atomic`, which never called it: the
//! two users are `codec` (probing samples) and `recovery` (reading `.rev`
//! headers). Keeping it there made the public codec layer depend on the
//! filesystem layer for nine lines of loop, so it lives here instead.

use std::io::{self, Read};

/// Read until `buf` is full or EOF; returns the number of bytes read.
///
/// Unlike [`Read::read_exact`] a short read is not an error, which is what
/// every caller wants: a truncated file or `.rev` header must be inspected,
/// not rejected before the caller can see how much arrived.
pub(crate) fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}
