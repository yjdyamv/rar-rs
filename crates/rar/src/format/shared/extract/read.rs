//! Member reads by name or index, with and without explicit limits.

use std::io::Write;

use crate::engine::Engine;
use crate::error::{RarError, RarResult};

/// Read an entry selected by its archive-order catalog index.
pub(crate) fn read_at_index_with_options(
    cx: &mut dyn Engine,
    target_idx: usize,
    opts: crate::options::ExtractOptions,
) -> RarResult<Vec<u8>> {
    if target_idx >= cx.entries().len() {
        return Err(RarError::InvalidState(
            "entry index is outside the current catalog".into(),
        ));
    }
    cx.read_ctx_mut().extract_options = opts;
    crate::format::shared::extract::members::validate_entry_limits(cx, target_idx)?;
    crate::format::shared::extract::decode_entry_at(cx, target_idx)
}

/// Stream an entry selected by its archive-order catalog index.
pub(crate) fn read_to_writer_at_index_with_options(
    cx: &mut dyn Engine,
    target_idx: usize,
    writer: &mut dyn Write,
    opts: crate::options::ExtractOptions,
) -> RarResult<u64> {
    if target_idx >= cx.entries().len() {
        return Err(RarError::InvalidState(
            "entry index is outside the current catalog".into(),
        ));
    }
    cx.read_ctx_mut().extract_options = opts;
    crate::format::shared::extract::decode_entry_to(cx, target_idx, writer)
}
