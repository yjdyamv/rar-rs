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

## In-tree usage inventory (post Phase 5)

Legacy facade methods still called by in-tree consumers (tests/cli/napi/
examples/fuzz, aggregated by call-site count):

| legacy call | call sites | typical user |
| --- | --- | --- |
| `create_with_options` | 140 | rar5 tests (compat) |
| `.close()` | 146 | rar5 tests, leftover seams |
| `.add_bytes` / `.add` | 182 | rar5 tests, leftovers |
| `.namelist()` | 41 | tests + CLI replace/version logic |
| `.delete()` / `.rename()` | 46 | tests + `rar u/f` version-control seam |
| `.lock()` | 14 | CLI `rar k` (no editor counterpart) |
| `.get_entry()` / `.verify()` | 18 | tests, `-t` post-create check |
| `.extract_all()` | 7 | tests |

The CLI/N-API create/append/edit paths already run on the role APIs; the
remaining legacy seams are documented in the plan (version-control
rename chain in `rar u/f`, `rar k` lock) plus the large compat-test body,
which exists to prove byte behavior and should keep compiling against the
legacy facade until it is removed.

Low-level public modules and their in-tree users:

| module | visibility | in-tree users |
| --- | --- | --- |
| `rar5::rar50` (alias of `format::rar5`) | public | 3 test files (wire-level helpers) |
| `rar5::rar40` (alias of `format::rar4`) | public | none |
| `rar5::recovery` | doc(hidden) | CLI `rv`, robustness, fuzz |
| `rar5::name_policy` | doc(hidden) | CLI mask/exclusion logic |
| `rar5::codec::lzss_huff` | public | napi streaming, mtbench examples |
| `rar5::options` / `rar5::version` | public | CLI/typed option mapping |

Crate naming surface: package name `rar5` in `crates/rar/Cargo.toml`, the
workspace dependency alias (`rar5 = { path = "crates/rar" }`), the two
binding crates' dependency entries, and code references `rar5::…` across
tests/examples/fuzz. The npm package is already `rar-rs-napi`.

## Decisions and recommendations

1. Crate rename. Options: (a) keep `rar5`, (b) rename lib crate to
   `rar-rs` (repo/README/binding branding) with the alias plumbing updated,
   (c) rename to a format-neutral name. Recommendation: decide after the
   model split; a rename is mechanical (workspace alias + `crate::`-free
   external refs) but touches every `use rar5::…` in tests/fuzz, so it
   should ride the same release as the removals below.
2. Split archive format from compression version. The public
   `ArchiveVersion` conflates container (`Rar40`) with codec selection
   (`Rar50` v50-only vs `Rar70` forced-v70 + auto v50/v70 on Rar50).
   Recommendation: introduce a format/container type and a codec/version
   knob on the typed writer options (Rar70 semantics already exist as the
   auto path); `ArchiveVersion` stays as the container enum used by
   reader-facing code.
3. Supported low-level modules. `rar40` (alias) has no in-tree user;
   `rar50` (alias) is used by three wire-level test files. `recovery`,
   `name_policy`, `lzss_huff` internals are used by bindings/CLI/examples.
   Recommendation: move the wire-level `rar40`/`rar50` surfaces behind
   `raw`/`unstable` gated modules, keep `recovery`/`name_policy`
   doc(hidden) as-is, and keep `codec::lzss_huff` public (stable enough
   for the mtbench/napi streaming paths).
4. Legacy facade removal. Only the compat tests, the `rar u/f`
   version-control rename chain and `rar k` (lock) still call legacy
   mutation; read paths already use `ArchiveReader`. Recommendation:
   deprecate legacy methods now (warnings in-tree), keep the facade until
   an `ArchiveEditor` lock op and an editor-native chained-rename path
   exist, then delete in the same breaking release.

## Sequence

Each decision above becomes its own Phase 6 commit after maintainer
sign-off, each gated by the full validation matrix and by the public
path-compat compile tests.
