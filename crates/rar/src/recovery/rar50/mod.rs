//! RAR5 inline recovery records: the `"RR"` service block carries GF(2^16)
//! Cauchy parity over the archive prefix.
//!
//! Ported from the `rars` project (https://github.com/bitplane/rars), licensed
//! MIT OR Apache-2.0, at upstream revision `c08a17b`. See NOTICE for
//! attribution and the unresolved workspace-metadata/COPYING difference.
//!
//! Role split (the ported core stays a faithful unit in [`gf16`]):
//! - [`plan`] — shard geometry, `InlineRecoveryPlan` and CRC64 helpers,
//! - [`gf16`] — GF(2^16) tables, Cauchy matrix and the parity encoder,
//! - [`encode`] — record builder (parity payload, buffered and streaming),
//! - [`repair`] — in-memory chunk scan, shard reconstruction and patching,
//! - [`stream`] — file-to-file streaming repair for archives larger than RAM.

mod encode;
mod gf16;
mod plan;
mod repair;
mod stream;
#[cfg(test)]
mod tests;

pub use encode::build_structural_inline_recovery_data;
pub(crate) use encode::build_structural_inline_recovery_data_streaming;
pub use gf16::{Gf16, encode_parity_shards, make_encoder_matrix};
pub use plan::{crc64_rar_state, crc64_xz};
pub use repair::{reconstruct_data_shards, repair_inline_recovery_archive};
pub use stream::repair_inline_recovery_archive_path;

const CRC64_XZ_POLY: u64 = 0xc96c_5795_d787_0f42;
const CRC64_XZ_INIT: u64 = 0xffff_ffff_ffff_ffff;
const FIELD_SIZE: usize = 65_535;
const FIELD_MASK: u32 = 0xffff;
const PRIMITIVE_POLYNOMIAL: u32 = 0x1100b;
const ZERO_LOG_SENTINEL: u32 = (FIELD_SIZE * 2) as u32;
const MAX_WINRAR602_DATA_SHARDS: u64 = 200;
const KIB: u64 = 1024;
const RAR5_RECOVERY_CHUNK_FIXED_HEADER_SIZE: u64 = 0x48;

/// The process-wide GF(2^16) field tables (256 KiB, built once).
pub fn shared_gf16() -> &'static Gf16 {
    static GF16: std::sync::OnceLock<Gf16> = std::sync::OnceLock::new();
    GF16.get_or_init(Gf16::new)
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    BadRecoveryChunk,
    OddShardSize,
    PlanOverflow,
    PrefixExceedsPlan,
    TooManyDamagedShards,
    ShardSizeMismatch,
    TooManyShards,
    SingularElement,
    /// Operation aborted by the caller's cancellation flag.
    Cancelled,
    /// I/O failure while streaming a repair (source read / destination
    /// write); carries the underlying error message.
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRecoveryChunk => f.write_str("RAR 5 recovery chunk is invalid"),
            Self::OddShardSize => f.write_str("RAR 5 recovery shard size is odd"),
            Self::PlanOverflow => f.write_str("RAR 5 recovery plan overflows"),
            Self::PrefixExceedsPlan => {
                f.write_str("RAR 5 recovery prefix exceeds planned shard capacity")
            }
            Self::TooManyDamagedShards => {
                f.write_str("RAR 5 recovery data cannot repair this many damaged shards")
            }
            Self::ShardSizeMismatch => f.write_str("RAR 5 recovery shard sizes differ"),
            Self::TooManyShards => f.write_str("RAR 5 recovery shard count is invalid"),
            Self::SingularElement => f.write_str("RAR 5 recovery matrix is singular"),
            Self::Cancelled => f.write_str("operation cancelled"),
            Self::Io(msg) => write!(f, "recovery I/O: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[inline]
fn check_cancel(cancel: Option<&std::sync::atomic::AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed)) {
        return Err(Error::Cancelled);
    }
    Ok(())
}
