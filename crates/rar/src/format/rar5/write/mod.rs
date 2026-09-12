//! RAR5 write pipeline: member addition, emission, volume splitting and the
//! parallel batch. Methods on [RarArchive] live in sibling impl blocks (see
//! src/archive.rs for the shared state).
//!
//! The format-neutral member dispatchers live in
//! `crate::format::shared::write_ops`. This module tree is RAR5-only:
//! [`add`] holds the member entry points and extra-record builders, [`emit`]
//! the block headers and split writers, `stream.rs` the bounded-memory
//! payload writers, `batch.rs` the parallel wave preparation, `engine.rs`
//! the AES-256 range emitter and `layout.rs`/`windows.rs` the sizing and
//! Windows helpers.

mod add;
#[cfg(feature = "parallel")]
mod batch;
mod emit;
pub(crate) mod engine;
pub(crate) mod layout;
mod stream;
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use self::windows::{windows_set_creation_time, write_windows_stream};
