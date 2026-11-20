//! WinRAR-compatible command-line tools.
//!
//! This crate ships two binaries and no library API of its own:
//!
//! - `rar` — create, update and edit archives (`a`, `u`, `f`, `d`, `m`, `t`,
//!   `x`, `e`, `l`, `rn`, `rr`, …).
//! - `unrar` — extract and test (`x`, `e`, `t`, `l`, `p`, `lt`).
//!
//! Both take the WinRAR switch vocabulary (`-inul`, `-o+`, `-p<pw>`, `-rr`,
//! `-v<size>`, …) and reproduce WinRAR's behaviour on the cases the test suite
//! covers. The switch-by-switch reference is `docs/CLI.md` in the repository:
//! <https://github.com/yjdyamv/rar-rs/blob/main/docs/CLI.md>.
//!
//! The archive format itself, and the reader/writer API both binaries are
//! built on, is the `rar-rs` crate: <https://docs.rs/rar-rs>.
//!
//! # Why this crate has a library target
//!
//! The binaries are the whole product; their code lives under `src/bin/` and
//! `src/common.rs`, `src/ops.rs` … are compiled into each of them. This empty
//! library target exists only so docs.rs can build a page for the crate:
//! docs.rs runs `cargo rustdoc --lib`, which fails with `no library targets
//! found in package 'rar-cli'` for a package that ships nothing but binaries.
