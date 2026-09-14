//! Read-side orchestration shared by the RAR5, RAR4 and RAR13 families.
//!
//! The family-specific scan and member decode live in
//! [`crate::format::rar5::extract`], [`crate::format::rar4::extract`] and
//! [`crate::format::rar13::extract`]; this module owns the archive-level
//! read policy (opening, destinations, extraction) and the single dispatch
//! point per operation.

pub(crate) mod dest;
pub(crate) mod members;
pub(crate) mod open;
pub(crate) mod read;

pub use members::Destination;

use crate::archive::RarArchive;
use crate::error::{RarError, RarResult};

/// Ceiling on how many entries a catalog may hold. A full scan's entry count
/// is bounded only by the physical archive size, and the quick-open payload
/// can describe millions of small entries; neither may expand into multi-GiB
/// allocation of `ArchiveEntry`/`FileHeader` objects. The bound is far above
/// any real archive, so only hand-made inputs are rejected.
pub(crate) const MAX_CATALOG_ENTRIES: usize = 1_000_000;

/// Reject a catalog that would grow past `max`.
pub(crate) fn check_entry_cap(count: usize, max: usize) -> RarResult<()> {
    if count >= max {
        return Err(RarError::Format(format!(
            "archive catalog exceeds the {max}-entry ceiling"
        )));
    }
    Ok(())
}

impl RarArchive {
    /// Decode member `idx` to memory, honoring the family's solid chains.
    pub(crate) fn decode_entry_at(&mut self, idx: usize) -> RarResult<Vec<u8>> {
        if self.rar4 || self.rar13 {
            return self.decode_rar4_at(idx);
        }
        if self.is_solid_chain_member(idx) {
            return self.decode_solid_through(idx);
        }
        self.decode_file_at(idx, None)
    }

    /// Decode member `idx` streaming into `writer`, honoring the family's
    /// solid chains.
    pub(crate) fn decode_entry_to(
        &mut self,
        idx: usize,
        writer: &mut dyn std::io::Write,
    ) -> RarResult<u64> {
        if self.rar4 || self.rar13 {
            return self.decode_rar4_to(idx, writer);
        }
        if self.is_solid_chain_member(idx) {
            return self.decode_solid_through_to(idx, writer);
        }
        self.decode_file_to(idx, writer, None)
    }
}
