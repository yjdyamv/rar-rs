//! WinRAR interoperability tests (Windows-friendly).
//!
//! Unlike `tests/interop.rs` (which uses the Linux `rar`/`unrar` console
//! tools through `SA_OFFICIAL_RAR` / `SA_OFFICIAL_UNRAR`), this file
//! locates an installed WinRAR (default `C:\Program Files\WinRAR`) and

//! [`support`] holds the tool lookup and process helpers; the other
//! modules group the cases by area.

mod compression;
mod filters;
mod large;
mod misc;
mod rar4_create;
mod rar4_edit;
mod recovery;
mod solid;
mod streaming;
mod streams;
mod support;
mod timestamps;
mod v70;
mod volumes;
mod winrar_created;
