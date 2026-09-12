# ADR 0006: Public API convergence on the typed role facades

- Status: accepted (executed 2026-09)
- Date: 2026-09-12
- Related: [ADR 0002](0002-format-neutral-model-and-api-v2.md) (role APIs), [ADR 0003](0003-breaking-release-scope.md) (breaking-release scope, decision 4)

## Context

Since Phase 4/5 the crate exposes two API generations side by side:

- the typed role facades `ArchiveReader` / `ArchiveWriter` / `ArchiveEditor`
  (with `OpenOptions`, `WriterOptions`, `AppendOptions`, `EntryWriteOptions`,
  `EditPlan`), and
- the legacy `RarArchive` engine/façade, whose methods predate the roles
  (`open*`, `create_with_options`, `add*`, `close`, `namelist`, `get_entry`,
  `read`, `extract_all`, `test`, `delete`, `rename`, `lock`, …).

ADR 0003 decision 4 recorded the intent to delete the legacy façade at the
next breaking release, after migrating production consumers. Two facts made
that plan drift: the `#[deprecated]` attributes applied in Phase 5 were lost
in later refactors, and `CreateOptions` (a public options type with no public
constructor entry point — `RarArchive::create_with_options` is
`pub(crate)`) is still re-exported from the crate root although nothing
outside `crates/rar/src` uses it.

An inventory of the current consumers:

| consumer | legacy `RarArchive` uses |
| --- | --- |
| `crates/rar-cli` (`rar r` verification, `rar cw`) | 4 |
| `crates/rar-napi` (`test_archive`, `extract_archive`) | 4 |
| `crates/rar/examples/bench.rs` | 2 |
| `fuzz/` | 2 |
| `crates/rar/tests/**` (byte-parity corpus) | 23 |
| `crates/rar/src/**` (internal engine, role delegation) | 215 |

The only public capability the roles did not cover was reading the
archive-level comment: `ArchiveReader` had no counterpart to
`RarArchive::get_comment`.

## Decision

1. **The typed role facades are the only supported public API surface.**
   New code uses `ArchiveReader` / `ArchiveWriter` / `ArchiveEditor` and the
   validated option types.

2. **`RarArchive` leaves the crate root.** It stays `pub` (so the in-tree
   byte-parity corpus, fuzzer and internal delegation keep working) but is
   marked `#[doc(hidden)]` and is reachable only as
   `rar_rs::archive::RarArchive`, the documented-as-unsupported compat path.
   It is not deleted yet: the compat corpus deliberately exercises the
   legacy byte behaviour, and ADR 0003 decision 4 keeps full removal for a
   future breaking release, when the corpus is migrated or retired.

3. **Production consumers migrate to the roles now.** The CLI, N-API
   binding, examples and fuzzer no longer call `RarArchive`; the one missing
   read capability is filled by `ArchiveReader::comment()` (delegating to the
   same service-block parser), so no role consumer needs the legacy type.

4. **`CreateOptions` is demoted to `pub(crate)`.** It is the internal
   options struct consumed by `archive/create.rs` and the RAR4 repack
   pipeline; the public builder is `WriterOptions` (whose validation rules
   `CreateOptions::validate` shares). It is removed from the crate-root
   re-exports; `parse_dict_size` / `parse_dict_bytes` stay public because the
   CLI and N-API map `-md` strings through them.

5. **Deprecation is expressed by documentation, not `#[deprecated]`.** The
   engine is used internally on every write/read path; a deprecated attribute
   would require crate-wide `#[allow(deprecated)]` and lose its signal. The
   supported/unsupported line is the root re-export list plus rustdoc.

## Consequences

- `rar_rs::RarArchive` stops compiling for downstream users; the compat path
  is `rar_rs::archive::RarArchive` (undocumented). This is a breaking change,
  acceptable in the pre-1.0 series and consistent with ADR 0003's scope.
- `rar_rs::CreateOptions` stops compiling; `WriterOptions` is the replacement
  and already validates every combination the internal struct does.
- The compat-test corpus keeps compiling unchanged except for the import
  path, so the byte-parity guarantees stay covered.
- Filling `ArchiveReader::comment()` makes the reader role complete with
  respect to read-only archive metadata.
