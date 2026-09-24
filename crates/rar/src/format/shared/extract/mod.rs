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

use crate::engine::Engine;
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
        return Err(RarError::format(format!(
            "archive catalog exceeds the {max}-entry ceiling"
        )));
    }
    Ok(())
}

/// Ceiling on how many data chunks one continuing member may accumulate
/// across volumes. A real set contributes at most one chunk per volume, so
/// the bound sits far above any archival use; without it a crafted set of
/// tiny continuation headers grows one member's chunk vector (and the
/// cloned extra records it holds) without bound. Enforced by the RAR5
/// catalog builder and the shared legacy split merge alike.
pub(crate) const MAX_MEMBER_CHUNKS: usize = 1_000_000;

/// Reject a continuing member that would grow past `max` chunks.
pub(crate) fn check_chunk_cap(count: usize, max: usize, member: &str) -> RarResult<()> {
    if count >= max {
        return Err(RarError::format(format!(
            "member {member} exceeds the {max}-chunk ceiling"
        )));
    }
    Ok(())
}

/// Whether the parallel extraction path may decode this archive. The
/// parallel phase decodes with the RAR5 codec and uses the RAR5 solid
/// rule, so legacy families always stream sequentially.
#[cfg(feature = "parallel")]
pub(crate) fn supports_parallel_extract(cx: &dyn Engine) -> bool {
    !cx.is_legacy()
}

/// Maximum packed bytes accepted when the payload must be aggregated in
/// memory. Bounded by the configured unpacked limit plus a small overhead,
/// or a hard 8 GiB allocation guard when output is otherwise unlimited.
///
/// Family-neutral: the legacy readers enforce the same budget, so it lives
/// with the shared extraction policy rather than in the RAR5 decoder.
pub(crate) fn max_packed_bytes(cx: &dyn Engine) -> u64 {
    cx.read_ctx()
        .extract_options
        .max_unpacked_bytes
        .map(|u| u.saturating_add(1 << 20))
        .unwrap_or(8 * 1024 * 1024 * 1024)
}

/// Replace a quick-open catalog with the fully scanned one before
/// extraction. A quick-open catalog carries no "STM" service records, and
/// only RAR5 has one — the legacy families always scan, so this is a no-op
/// for them. The family reference stays in this dispatcher.
pub(crate) fn ensure_full_catalog(cx: &mut dyn Engine) -> RarResult<()> {
    if cx.is_legacy() {
        return Ok(());
    }
    crate::format::rar5::extract::open::ensure_full_catalog(cx)
}

/// Decode member `idx` to memory, honoring the family's solid chains.
pub(crate) fn decode_entry_at(cx: &mut dyn Engine, idx: usize) -> RarResult<Vec<u8>> {
    if cx.is_legacy() {
        return crate::format::rar4::extract::decode_rar4_at(cx, idx);
    }
    if crate::format::rar5::extract::solid::is_solid_chain_member(cx, idx) {
        return crate::format::rar5::extract::solid::decode_solid_through(cx, idx);
    }
    crate::format::rar5::extract::decode::decode_file_at(cx, idx, None)
}

/// Decode member `idx` streaming into `writer`, honoring the family's
/// solid chains.
pub(crate) fn decode_entry_to(
    cx: &mut dyn Engine,
    idx: usize,
    writer: &mut dyn std::io::Write,
) -> RarResult<u64> {
    if cx.is_legacy() {
        return crate::format::rar4::extract::decode_rar4_to(cx, idx, writer);
    }
    if crate::format::rar5::extract::solid::is_solid_chain_member(cx, idx) {
        return crate::format::rar5::extract::solid::decode_solid_through_to(cx, idx, writer);
    }
    crate::format::rar5::extract::decode::decode_file_to(cx, idx, writer, None)
}

#[cfg(all(test, feature = "parallel"))]
mod tests {
    use crate::archive::RarArchive;
    use crate::format::shared::extract::supports_parallel_extract;

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
        assert!(!supports_parallel_extract(&legacy));

        let modern = dir.path().join("v50.rar");
        let mut archive =
            RarArchive::create_with_options(&modern, crate::options::CreateOptions::default())
                .unwrap();
        archive.add_bytes("a.txt", b"modern data", 0).unwrap();
        archive.close().unwrap();
        let modern = RarArchive::open(&modern).unwrap();
        assert!(supports_parallel_extract(&modern));
    }
}
