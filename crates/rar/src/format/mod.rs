//! Format implementations: RAR 1.3/1.4 (the `RE~^` family), RAR4 (the
//! legacy `Rar!\x1a\x07\x00` container family) and RAR5 (the modern
//! container, including RAR7 v70 members). Internal home of the historical
//! `crate::format::rar4` / `crate::rar50` module trees.

// The trees are only publicly reachable with the `raw` feature (see
// `lib.rs`). Without it they are crate-internal and a fair number of wire
// constants and helpers have no in-tree caller — they exist for downstream
// raw access, so silence dead-code for that configuration only.

pub mod rar13;
pub mod rar4;
pub mod rar5;
pub(crate) mod shared;
