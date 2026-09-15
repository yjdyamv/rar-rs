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

pub use members::ExtractionReport;

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
    /// Whether the parallel extraction path may decode this archive. The
    /// parallel phase decodes with the RAR5 codec and uses the RAR5 solid
    /// rule, so legacy families always stream sequentially.
    #[cfg(feature = "parallel")]
    pub(crate) fn supports_parallel_extract(&self) -> bool {
        !self.is_legacy()
    }

    /// Decode member `idx` to memory, honoring the family's solid chains.
    pub(crate) fn decode_entry_at(&mut self, idx: usize) -> RarResult<Vec<u8>> {
        if self.is_legacy() {
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
        if self.is_legacy() {
            return self.decode_rar4_to(idx, writer);
        }
        if self.is_solid_chain_member(idx) {
            return self.decode_solid_through_to(idx, writer);
        }
        self.decode_file_to(idx, writer, None)
    }
}

#[cfg(all(test, feature = "parallel"))]
mod tests {
    use super::*;

    #[test]
    fn parallel_extraction_is_rar5_only() {
        let dir = tempfile::tempdir().unwrap();

        let legacy = dir.path().join("v29.rar");
        let mut archive = RarArchive::create_with_options(
            &legacy,
            crate::options::CreateOptions {
                compression: crate::version::ArchiveVersion::V29,
                ..Default::default()
            },
        )
        .unwrap();
        archive.add_bytes("a.txt", b"legacy data", 0).unwrap();
        archive.close().unwrap();
        let legacy = RarArchive::open(&legacy).unwrap();
        assert!(!legacy.supports_parallel_extract());

        let modern = dir.path().join("v50.rar");
        let mut archive =
            RarArchive::create_with_options(&modern, crate::options::CreateOptions::default())
                .unwrap();
        archive.add_bytes("a.txt", b"modern data", 0).unwrap();
        archive.close().unwrap();
        let modern = RarArchive::open(&modern).unwrap();
        assert!(modern.supports_parallel_extract());
    }
}
