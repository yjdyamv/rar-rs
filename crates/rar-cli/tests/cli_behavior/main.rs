//! CLI behavior tests: drive the built `rar`/`unrar` binaries through
//! WinRAR-compatible switches. Lives in this crate because the
//! `CARGO_BIN_EXE_*` env vars are only defined for the package that builds
//! the binaries (moved here from the library's interop.rs).
//!
//! [`support`] holds the shared fixtures and the `rarfiles.lst` lock; the
//! other modules group the tests by area.

mod basics;
mod compression;
mod config;
mod edits;
mod formats;
mod input;
mod legacy;
mod operations;
mod parity;
mod parity2;
mod recovery;
mod support;
mod switches;
mod timestamps;
mod update;
