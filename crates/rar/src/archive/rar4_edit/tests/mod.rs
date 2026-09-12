//! Test suites for the RAR4 edit engine, grouped by area: `basic` (header
//! and comment primitives), `delete`, `append`, `repack` (solid) and `hp`
//! (header encryption end to end).

mod append;
mod basic;
mod delete;
mod hp;
mod repack;
