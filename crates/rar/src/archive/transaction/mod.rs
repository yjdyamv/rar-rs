//! Edit transactions: delete, rename, comment and recovery-record
//! mutation share one plan/execute pipeline (the rewrite engine behind
//! [`RarArchive`] and [`super::ArchiveEditor`]). Methods on [`RarArchive`]
//! in a sibling impl block (see `crate::archive::mod`).
//!
//! Role split:
//! - [`multivolume`] — multi-volume rewrite and `.rev` regeneration,
//! - [`edit`] — edit entry points (plan, comment, lock check),
//! - [`plan`] — the rewrite plan walker,
//! - [`execute`] — plan execution and the copy pipeline,
//! - [`header`] — main-header rebuilding and locator/record helpers.

mod edit;
mod execute;
mod header;
mod multivolume;
mod plan;
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
        qo_header: Option<Vec<u8>>,
    },
    /// Decode (and recompress when kept) one member of the affected solid
    /// chain.
    Recompress { idx: usize, is_deleted: bool },
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
