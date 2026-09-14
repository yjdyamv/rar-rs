//! Test suites for the RAR4 edit engine, grouped by area: `basic` (header
//! and comment primitives), `delete`, `append`, `repack` (solid),
//! `multivolume` (set-level edits) and `hp` (header encryption end to end).

mod append;
mod basic;
mod delete;
mod hp;
mod multivolume;
mod repack;
