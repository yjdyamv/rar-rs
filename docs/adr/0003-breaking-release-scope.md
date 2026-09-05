# ADR 0003: Phase 6 breaking-release scope

Status: proposed (decision inputs recorded; each decision needs maintainer
sign-off before execution).

## Context

Phases 2-5 delivered role APIs (`ArchiveReader` / `ArchiveWriter` /
`ArchiveEditor` + `EditPlan`), validated option types, and a converged
internal layout (`archive/{reader,writer,editor,transaction}.rs`,
`fs/`, `model/`, `format/rar4`, `format/rar5`, `codec/{common,legacy,
modern}`). The remaining API v2 work is a breaking release: legacy surface
removal, public-path convergence and (possibly) a crate rename. This ADR
records what is actually used in-tree, so those decisions are data-driven.

## In-tree usage inventory (after Phase 4/5 migration, deprecation applied)

Phase 5 also finished the wildcard import sweep: every production
`use crate::…::*` / `use super::*` is now an explicit import (public barrels
re-exported as explicit item lists, `#[cfg]`-gated where consumers were
platform/feature/test-scoped). The only globs left are the idiomatic test-
module `use super::*`, `rayon::prelude::*` in parallel paths,
`napi::bindgen_prelude::*`, and integration-test `support::*`. The remaining
path-level transitional surface — the `#[doc(hidden)]` `rar40`/`rar50` aliases
and the `format` tree visibility — is removed at the breaking release.

The CLI and N-API run entirely on the typed role APIs. Every legacy facade
method with a role equivalent now carries `#[deprecated]` pointing at its
replacement, and the remaining in-tree callers are the compat-test body
(which exists to prove byte behavior and keeps compiling against the facade
under `#![allow(deprecated)]`), the fuzz harness, and the examples. The two
roles still delegate to the facade internally (`ArchiveWriter` → `create`/
`add*`/`close`, `ArchiveReader::extract_all` → `extract_all`,
`ArchiveEditor::lock` → `lock`); those seams carry `#[allow(deprecated)]` and
die with the facade.

| deprecated facade call | remaining call sites | where |
| --- | --- | --- |
| `create_with_options` | many | compat tests, fuzz |
| `.close()` | many | compat tests, fuzz, examples |
| `.add_bytes` / `.add` | many | compat tests, examples, fuzz |
| `.namelist()` | ~76 | compat tests only (CLI now uses `ArchiveReader`) |
| `.delete()` / `.delete_with_progress()` / `.rename()` | ~30 | compat tests only (`rar u/f` now uses `ArchiveEditor`) |
| `.lock()` | few | rewrite/lock parity tests only (`rar k` now uses `ArchiveEditor::lock`) |
| `.get_entry()` | many | compat tests only |
| `.extract_all()` | several | compat tests, examples |
| `.read()` | several | compat tests, fuzz (N-API now uses `read_entry`) |

The version-control rename chain in `rar u/f` and the `rar k` lock command
were the last CLI users of the legacy mutation surface; both now run on
`ArchiveEditor` (`editor_chained_rename_plan` / `editor_delete_plan` two-apply
rewrite for version control, `ArchiveEditor::lock` for locking), so the
facade's only remaining consumers are the byte-parity test corpus, fuzz and
the benchmarks.

Low-level public modules and their in-tree users:

| module | visibility | in-tree users |
| --- | --- | --- |
| `rar_rs::rar50` (alias of `format::rar5`) | public | 3 test files (wire-level helpers) |
| `rar_rs::rar40` (alias of `format::rar4`) | public | none |
| `rar_rs::recovery` | doc(hidden) | CLI `rv`, robustness, fuzz |
| `rar_rs::name_policy` | doc(hidden) | CLI mask/exclusion logic |
| `rar_rs::codec::lzss_huff` | public | napi streaming, mtbench examples |
| `rar_rs::options` / `rar_rs::version` | public | CLI/typed option mapping |

Crate naming surface: package name `rar-rs` in `crates/rar/Cargo.toml`, the
workspace dependency key (`rar-rs = { path = "crates/rar" }`), the two
binding crates' dependency entries, and code references `rar_rs::…` across
tests/examples/fuzz. The npm package is already `rar-rs-napi`.

## Decisions and recommendations

1. Crate rename. Decided: rename the lib crate to `rar-rs`.
   Executed with Phase 6: package name, workspace dependency key and every
   external `use rar5::…` reference became `rar_rs::…` (the fuzz harness is
   `rar-rs-fuzz`); the `format::rar5`/`format/rar4` internal module paths are
   untouched. A rename is mechanical (workspace alias + `crate::`-free
   external refs) but touches every external reference, so it rides the same
   breaking release as the removals below.
2. Split archive format from compression version. The public
   `ArchiveVersion` conflated container (`Rar40`) with codec selection
   (`Rar50` v50-only vs `Rar70` forced-v70 + auto v50/v70 on Rar50).
   Decided and executed: `ArchiveFormat` is a new container-family type
   (`Rar40` / `Rar5`); `ArchiveVersion` is reduced to the member codec
   versions inside the RAR5 container (`Rar50` / `Rar70`); the typed
   `WriterOptions` select container and codec independently
   (`WriterOptions::format(ArchiveFormat)` + `compression(CompressionVersion)`,
   no mutual rewriting), and legacy `CreateOptions::format_version` now takes
`ArchiveFormat`. Nothing on the read path emitted `ArchiveVersion::Rar40`
    (RAR4 members report raw `format_version: 4` / `unp_ver`), and the
    codec variant positions only ever saw `Rar50`/`Rar70`; the RAR4
    1.5–4.x unpack versions (15/20/26/29/36) live on the raw model
    (`Entry::unp_ver`), not on `ArchiveVersion`. Reader-facing code
    keeps reporting `ArchiveVersion` per member.
    **Superseded by ADR 0004**: the two public axes were collapsed back into
    the single `ArchiveVersion` table (`V15`–`V70`, two-digit names) with the
    container derived from it; `ArchiveFormat` and `CompressionVersion` were
    removed.
3. Supported low-level modules. `rar40` (alias) has no in-tree user;
   `rar50` (alias) is used by three wire-level test files. `recovery`,
   `name_policy`, `lzss_huff` internals are used by bindings/CLI/examples.
   Recommendation: gate the wire-level `rar40` surface behind a `raw`
   feature (done; `rar_rs::rar40` is now feature-gated and `format::rar4` is
   internal), keep `rar50` public while the in-tree wire tests still use
   it, keep `recovery`/`name_policy` doc(hidden) as-is, and keep
   `codec::lzss_huff` public (stable enough for the mtbench/napi streaming
   paths).
4. Legacy facade removal. The typed roles now cover the full surface
   including the previously blocking seams: `archive u/f` version control
   runs on `ArchiveEditor` (chained rename + delete, two applies) and
   `rar k` uses `ArchiveEditor::lock`. The remaining facade callers are the
   byte-parity compat corpus, fuzz and examples, all under explicit deprecation
   allows. Recommendation: keep the facade until the breaking release, then
   delete it together with the `#[allow(deprecated)]` seams and the compat-test
   allows (or migrate the tests to the roles where byte parity still holds).

## Sequence

Each decision above becomes its own Phase 6 commit after maintainer
sign-off, each gated by the full validation matrix and by the public
path-compat compile tests.
