# ADR 0007: Retire the `raw` feature, promote the `wire` module

- Status: accepted (executed 2026-09)
- Date: 2026-09-12
- Related: [ADR 0003](0003-breaking-release-scope.md) (decision 3, superseded), [ADR 0006](0006-public-api-convergence.md) (typed role facades)

## Context

ADR 0003 gated the `format` / `recovery` / `crypto` module trees behind the
opt-in `raw` feature: off by default so internal refactors would never become
SemVer breaks, on for the in-tree tests and the fuzz workspace that need
wire-level access. `PLAN.md` recorded the deletion criterion: if the crate
would never serve external `use rar_rs::format::…` consumers, the feature is
ceremony.

The 2026-09 audit found:

- no product consumer — `rar-cli` and `rar-rs-napi` build without `raw`;
- no external consumer — the only importers are in-tree tests and `fuzz/`;
- a small, enumerable API subset actually used:

| consumer | items |
| --- | --- |
| `crates/rar/tests/support` (`scan_blocks`) | `read_block`, `BlockMeta`, `RawBlock` |
| `crates/rar/tests/robustness.rs` | `vint::encode`, `crc64_xz`, `crc64_rar_state` |
| `crates/rar/tests/model_api_compat.rs` | `DataChunk`, `FileHeader`, `RawBlock` |
| `fuzz/` | `build_structural_inline_recovery_data`, `build_recovery_volume_file`, `crc64_*`, `EncryptionParams`, `decrypt_data`, `derive_keys`, `encrypt_data` |

Keeping the feature cost a self dev-dependency (`rar-rs` enabling its own
feature for the dev graph), module-level `allow(dead_code, unused_imports)`
in the three trees, and an entirely undocumented public surface (the items
were `#[doc(hidden)]` even with `raw` on).

## Decision

1. Delete the `raw` feature and the `rar40` / `rar50` aliases.
2. `format` / `recovery` / `crypto` stay `pub(crate)` permanently.
3. A new always-public `wire` module re-exports exactly the subset above:
   the RAR5 block-envelope reader and varints, the archive model structs,
   the recovery/parity builders and the encryption primitives. It is
   supported API, but low-level — the typed role facades remain the
   recommended path.
4. Code that only existed for the retired surface is deleted; the remaining
   rars-parity helpers carry targeted `#[allow(dead_code)]` comments.

## Consequences

- Three whole module trees leave the reachable surface; the public API only
  gains the small `wire` re-export list.
- In-tree tests and fuzz drop the self dev-dependency and import
  `rar_rs::wire::…`.
- `model_api_compat.rs` loses its legacy-alias subject; it now checks the
  `wire` model structs' serialization helpers.
- The feature matrix in CI shrinks from eight combinations to four
  (`""`, `parallel`, `simd`, `parallel,simd`).
