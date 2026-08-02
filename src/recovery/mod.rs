//! RAR 5 recovery-record primitives.
//!
//! Ported from the `rars` project (github.com/bitplane/rars, WTFPL): RAR 5
//! recovery data uses GF(2^16) with reduction polynomial `0x1100b` and a
//! Cauchy encoder matrix, stored as `{RB}` shards in the `"RR"` service
//! header of the archive.

pub mod rar5;
pub mod rev5;
