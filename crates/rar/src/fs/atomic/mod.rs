//! Atomic, durable file staging and replacement: temp siblings, the commit
//! journal and its recovery path, `fsync` ordering and platform-specific
//! `replace_file`.
//!
//! Role split:
//! - [`primitives`] — temp naming, `create_new` staging, the platform
//!   `replace_file` variants and the fsync ordering,
//! - [`staged`] — the [`StagedFile`]/[`StagedCopy`] single-file values,
//! - [`journal`] — the on-disk commit journal and its field escaping,
//! - [`set`] — [`StagedSet`], the commit phases and commit recovery.

mod journal;
mod primitives;
mod set;
mod staged;
#[cfg(test)]
mod tests;

pub(crate) use primitives::{
    copy_prefix, install_durable, parent_dir, read_write_create, replace_file, temp_sibling_path,
    temp_suffix,
};
pub(crate) use set::{StagedSet, commit_files, recover_interrupted_commit};
pub use staged::StagedCopy;
pub(crate) use staged::StagedFile;

// Helpers the unit tests drive directly (same-file private before the split).
#[cfg(test)]
use journal::{commit_done_path, journal_path, plain_journal_name, write_commit_journal};
// The name escaping is byte-level on Unix and field-level elsewhere, and each
// spelling has its own test: import each where its test compiles.
#[cfg(all(test, unix))]
use journal::unescape_journal_name;
#[cfg(all(test, not(unix)))]
use journal::{escape_journal_field, unescape_journal_field};
