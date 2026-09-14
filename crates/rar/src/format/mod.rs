//! Format implementations: RAR 1.3/1.4 (the `RE~^` family), RAR4 (the
//! legacy `Rar!\x1a\x07\x00` container family) and RAR5 (the modern
//! container, including RAR7 v70 members). Internal home of the historical
//! `crate::format::rar4` / `crate::rar50` module trees.

// The trees are `pub(crate)` (ADR 0007 retired the downstream `raw`
// feature); wire-level helpers needed outside the crate are re-exported
// through `wire`, and the remaining constants/helpers stay crate-internal.

pub mod rar13;
pub mod rar4;
pub mod rar5;
pub(crate) mod shared;
