//! Edit transactions: delete, rename, comment and recovery-record
//! mutation share one plan/execute pipeline (the rewrite engine behind
//! `RarArchive` and [`super::ArchiveEditor`]). Methods on `RarArchive`
//! in a sibling impl block (see `crate::archive::mod`).
//!
//! Role split:
//! - [`multivolume`] — multi-volume rewrite and `.rev` regeneration,
//! - [`edit`] — edit entry points (plan, comment, lock check),
//! - [`plan`] — the rewrite plan walker,
//! - [`execute`] — plan execution and the copy pipeline,
//! - [`solid`] — the shared solid-chain packed read / decode / recompress,
//! - [`header`] — main-header rebuilding and locator/record helpers.

mod edit;
mod execute;
mod header;
mod multivolume;
mod plan;
mod solid;
#[cfg(test)]
mod tests;

use crate::format::rar5::headers::BlockMeta;

/// One step of a surgical archive rewrite.
enum RewriteOp {
    /// Copy one block verbatim: `header_bytes` followed by `len` bytes of
    /// data starting at `src_data` in the original archive. `qo_header`
    /// holds the header bytes for the rebuilt quick-open record (copied
    /// file blocks only, plaintext archives only).
    CopyBlock {
        header_bytes: Vec<u8>,
        src_data: u64,
        len: u64,
        /// The header was re-serialized in plaintext (a rename) and must be
        /// re-emitted through `write_block_header`, so `-hp` encrypts it.
        /// Verbatim copies carry their original on-disk bytes (already
        /// encrypted) and are written raw.
        rebuild_header: bool,
        qo_header: Option<Vec<u8>>,
    },
    /// Decode (and recompress when kept) one member of the affected solid
    /// chain. `chain_head` marks the first member of a chain range: the
    /// executor starts a fresh shared encoder/decoder state there, so a
    /// member of a later chain never references an earlier chain's window.
    Recompress {
        idx: usize,
        is_deleted: bool,
        chain_head: bool,
    },
    /// Replace member `idx`'s content in place: recompress `data` at the
    /// member's own slot with a fresh (non-solid) encoder, rebuilding the
    /// header from the original entry with the new size, CRC and mtime.
    /// Only valid for a member that is not part of a solid chain.
    Replace {
        idx: usize,
        data: Vec<u8>,
        mtime: u32,
        mtime_ns: u64,
    },
}

/// New content for one in-place member replacement (see
/// [`RewriteOp::Replace`]): the member's new plaintext and the mtime to
/// stamp on the rebuilt header.
pub(crate) struct Replacement {
    pub data: Vec<u8>,
    pub mtime: u32,
    pub mtime_ns: u64,
}

/// The result of planning a rewrite: the blocks to emit, in order.
struct RewritePlan {
    ops: Vec<RewriteOp>,
    /// Verbatim bytes of the archive encryption header (if any), written
    /// before the rebuilt main header.
    encrypt_header: Option<Vec<u8>>,
    /// Parsed main header block (plaintext).
    main_meta: BlockMeta,
    /// Recovery percentage from the dropped RR record; the record is
    /// rebuilt over the rewritten archive.
    rr_percent: Option<u8>,
    /// New archive comment (CMT service block), written right after the
    /// main header.
    comment: Option<Vec<u8>>,
}

/// Result of one combined structural edit: how many members were
/// deleted and how many were renamed (explicit rename pairs only;
/// directory-expanded descendants are not counted).
pub(crate) struct EditSummary {
    pub deleted: usize,
    pub renamed: usize,
}
