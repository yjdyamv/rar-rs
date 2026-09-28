//! Transactional archive replacement for the `rar` update/create flows.
//!
//! The durability rule itself lives in the library
//! ([`rar_rs::StagedCopy`]: same-directory copy, sync, durable replace,
//! remove on drop); these adapters keep the command closures' error seam
//! **typed**, so a library error inside the operation keeps its
//! [`ErrorCode`](rar_rs::ErrorCode) and therefore its WinRAR exit code.

use std::path::Path;

use crate::error::{CliError, CliResult};

/// Run `operation` against a same-directory copy of `archive` and install the
/// copy over the original only when the operation succeeds; the operation's
/// value is handed back after the commit.
pub(crate) fn update_archive_transactionally_with<T>(
    archive: &Path,
    operation: impl FnOnce(&Path) -> CliResult<T>,
) -> CliResult<T> {
    let mut staged = rar_rs::StagedCopy::create(archive).map_err(CliError::from)?;
    let value = operation(staged.path())?;
    staged.commit().map_err(CliError::from)?;
    Ok(value)
}

/// [`update_archive_transactionally_with`] for operations that return nothing.
pub(crate) fn update_archive_transactionally(
    archive: &Path,
    operation: impl FnOnce(&Path) -> CliResult<()>,
) -> CliResult<()> {
    update_archive_transactionally_with(archive, operation)
}
