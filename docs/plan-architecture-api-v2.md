# Architecture and API v2 implementation plan

This plan implements [ADR 0002](adr/0002-format-neutral-model-and-api-v2.md)
in small, independently validated changes. Checkboxes describe repository state,
not aspirational feature claims.

## Goals

- Make dependency direction explicit: archive orchestration depends on a
  format-neutral model, not on RAR5 wire headers.
- Add role-specific APIs that prevent invalid read/write/edit operations by
  construction.
- Correctly address duplicate member names through catalog-scoped entry IDs.
- Replace primitive option values with validated domain types in new APIs.
- Preserve current Rust, CLI, and N-API behavior until each consumer is migrated.

## Non-goals

- No immediate crate rename.
- No big-bang workspace split into format/codec/crypto crates.
- No rewrite of codec hot paths.
- No removal of `RarArchive` or current low-level public modules.
- No behavior change to legacy name-based methods during the additive phase.

## Phase 1: format-neutral model ownership

- [x] Add a private `model` module.
- [x] Move `FileHeader`, `DataChunk`, and the existing `FileHeader::default`
      implementation into `model` without changing layout or values.
- [x] Preserve `rar50::headers` and `rar50` public paths through re-exports.
- [x] Change `ArchiveEntry` to depend directly on `model`.
- [x] Remove `rar40` read/scan dependency on `rar50::headers` model types.
- [x] Replace RAR4 use of RAR5 normalization constants with local or model-level
      normalized values where doing so does not alter behavior.
- [x] Change RAR4 write-side in-memory model construction to use `model` paths.
- [x] Add compatibility/default/chunk-invariant tests.
- [x] Add a lightweight architecture guard against new
      `rar40 -> rar50::headers` model dependencies.

Exit criteria:

- old public paths compile and refer to the same concrete types;
- model has no format-module dependency;
- RAR4/RAR5/RAR7 tests remain green;
- no model layout or serialized-byte behavior changes.

## Phase 2: entry identity and reader API

- [x] Lock down current duplicate-name behavior with regression tests.
- [x] Add opaque `EntryId` scoped to a catalog generation.
- [x] Add `EntryRef`, `Entries`, and name-match iteration.
- [x] Add `ArchiveReader::open` and `ArchiveReader::open_with`.
- [x] Add ID-based `read_entry`, `copy_entry_to`, and `extract_entry`.
- [x] Add `VerificationReport` that preserves per-entry failures.
- [x] Add stable `ErrorCode` values for bindings, logs, and future CLI exit mapping.
- [x] Keep legacy `list`, `namelist`, `get_entry`, and `read(name)` behavior.
- [x] Migrate CLI list/print/selected extract/test paths to IDs.

Exit criteria:

- duplicate names can be listed and read independently;
- reader methods cannot invoke writer-only state;
- existing Rust API tests and examples compile unchanged.

### Current recommendation after Phase 2

Proceed to Phase 3 before moving format directories. The writer facade will exercise
transaction ownership and option validation against the current implementation;
those boundaries should be proven before physical module moves make diffs harder to
review.

Recommended order:

1. introduce validated option value types without changing legacy options;
2. add `ArchiveWriter::create`, `append`, and consuming `finish()`;
3. migrate CLI and N-API creation paths and compare produced bytes;
4. add `ArchiveEditor` IDs and combined rewrite transactions;
5. converge the physical directory layout only after reader/writer/editor seams are
   in active use.

Known deferred risks:

- the legacy facade and role-specific APIs coexist until all bindings migrate;
- RAR4 solid `copy_entry_to` still materializes a solid target member before writing;
- mutation commands remain name-based until `ArchiveEditor` is implemented;
- low-level public modules remain a SemVer constraint until a breaking release.

## Phase 3: validated options and writer API

Phase 3 completed: the Rust writer API, its transaction boundary, and the CLI
and N-API create/append migrations are in place and byte-compatible. RAR4
append and the editor-style rewrite steps (delete/rename/version control)
remain on the legacy seam until Phase 4.

- [x] Add `CompressionLevel`, `DictionarySize`, `ThreadCount`, and `SolidMode`.
- [x] Add `WriterOptions`, `AppendOptions`, and `EntryWriteOptions` with private
      fields and validated builders.
- [x] Add `ArchiveWriter::create` and `ArchiveWriter::append` as wrappers around
      the existing implementation.
- [x] Add consuming `ArchiveWriter::finish() -> WriteReport`.
- [x] Return final volume paths in `WriteReport`; remove binding-side rediscovery
      from new APIs.
- [x] Migrate CLI create/append and N-API create/append to the writer API.
- [x] Keep `CreateOptions`, numeric compression levels, and `close()` available.

Exit criteria:

- invalid option combinations fail during construction;
- successful `finish()` is the only explicit commit path in the new writer API;
- Drop cleans staging files but is not relied upon to report commit errors.

### Contract tightening pass (recorded before public release)

Review of the transaction seam against the legacy [`Drop`] auto-commit found
boundaries that were still too loose; all fixed and covered by tests
(`tests/archive_writer.rs`, unit tests in `archive/tests.rs`):

- `abort()` now clears the recovery state (`recovery_percent`,
  `recovery_volumes_percent`, `recovery_volumes_count`). `Drop` still runs
  `close()`, which generates `.rev` files after the data volumes are
  committed; an aborted transaction must never reach that step again (the
  volume set may not exist, or may be only partially committed).
- Append now explicitly rejects RAR4 archives with `RarError::Unsupported`
  before any staging I/O; the append rewrite is RAR5-container only.
- `WriterOptions` rejects combinations the legacy writer would silently
  downgrade: quick-open + header encryption, quick-open + data volumes,
  and any dictionary size on RAR4 (its writer picks the window internally).
- `finish()` documentation states commit granularity honestly: a
  single-volume replace is atomic; multi-volume output is moved volume by
  volume (each file complete, but a partial set is possible if interrupted
  between renames); `.rev` generation runs only after all data volumes are
  committed, so a `.rev` failure returns the error with data on disk.

Deferred with the editor migration (Phase 4): the CLI's delete-then-append
replacement rewrite in `rar a` and the staged delete/rename/version-control
steps of `rar u`/`rar f` still use the legacy read/delete seam; their
create/append stages already run on `ArchiveWriter`. RAR4 append remains
unsupported (matching the legacy seam).

### Migration notes (recorded with the binding migrations)

- `WriterOptions` on `Rar50` accepts dictionaries above 4 GiB with WinRAR's
  auto semantics (v50 for small members, v70 only when the effective
  dictionary exceeds 4 GiB), which the CLI's default/`-ma5` + `-md>4g`
  combination requires for byte parity; `-ma7` maps to the `Rar70` format
  (forced v70). `-ma4` keeps ignoring `-md`, as before.
- Bindings surface typed validation errors: invalid cross-field
  combinations that the legacy layer silently downgraded or reported as
  `Unsupported` now fail as `InvalidOption` (`rar a` errors out; N-API
  reports `InvalidArg`; the JS test expectation was updated).
- `rar a` on an existing archive now aborts cleanly on a failed append
  instead of leaving the legacy partial-commit artifacts, and N-API
  create/append tasks leave nothing at the output path when a member add
  fails or is cancelled.

## Phase 4: editor API and combined transactions

Phase 4 core slice completed here: the typed `ArchiveEditor` role with a
catalog and ID-based structural edits is in place and verified against the
legacy rewrite engine. `EditPlan`, comment/recovery combination, and the
binding migrations remain.

- [x] Add `ArchiveEditor` catalog and ID validation.
- [x] Extract internal index-based delete and rename operations.
- [x] Add ID-based delete/rename APIs.
- [x] Add catalog generation invalidation after structural edits.
- [x] Introduce `EditPlan` only after individual ID operations are stable.
- [x] Combine delete, rename, comment, and recovery changes into one rewrite.
- [x] Define and test multi-volume transaction/rollback behavior.
- [ ] Migrate CLI and N-API edit operations (nearly done: CLI edit commands
      `d`/`rn`/`ch`/`c`/`rr`/`m`, the `rar a` replacement + `-as` sync seams,
      the non-version staged `rar u`/`rar f` rewrite, and the N-API
      `deleteEntries` task are migrated; the map-aware chained-rename
      version-control branch of `rar u`/`rar f` and `rar k` (lock, no
      editor counterpart yet) stay on the legacy seam).

Exit criteria:

- stale IDs are detected;
- duplicate names can be edited independently;
- a failed combined edit leaves all original volumes intact.

### Phase 4 slice notes (recorded with the editor role)

- The rewrite engine was already index-based under the hood; the legacy
  name-based `delete`/`rename` now resolve names to catalog indexes and
  delegate to the extracted `delete_indexes`/`rename_indexes` cores, so
  both entry points share one implementation and byte behavior.
- `ArchiveEditor` mirrors the reader's catalog (`entries`/`entries_named`/
  `entry`/`unique_entry`, scoped `EntryId`s) plus `delete_entries` and
  `rename_entries`. Each call is one atomic rewrite: a failed edit leaves
  the archive and the catalog generation untouched; a successful edit
  re-scans the catalog and bumps the generation so every previously
  issued ID fails with `StaleEntryId`.
- Directory renames expand to descendants exactly like the legacy rename;
  delete-everything erases the archive like `rar d`; duplicate names are
  edited independently by ID. RAR4 (legacy-container) edits are refused
  with `Unsupported` — the rewrite engine is RAR5-only.
- Tests: `tests/archive_editor.rs` — duplicate-safe catalog, byte parity
  with the legacy delete/rename on twin archives, generation
  invalidation (including foreign-editor IDs), erase-all, solid-chain
  delete round-trip, RAR4 rejection.

### EditPlan slice notes (recorded with the combined rewrite)

- `EditPlan` (`delete(id)` / `rename(id, name)` ops, one `apply` per
  transaction) runs the whole plan through a single staged rewrite — one
  delete mask + one rename map — so delete+rename combine atomically. The
  core is a single `edit_plan` on `RarArchive`; the name-based
  `delete`/`rename` and the ID-based single-op helpers all delegate to it
  (three thin entry points, one implementation).
- Validation happens before any rewrite: stale IDs, renaming a member the
  same plan deletes (`InvalidOption`), and renaming a member of a solid
  chain that also loses a member (`Unsupported` — the recompressed chain
  would not carry the new name) all leave the archive untouched.
- Multi-volume plans re-split the volume set in one transaction; a failed
  plan leaves every volume byte-identical (tested by snapshot compare).
  Delete-everything still erases the archive like `rar d`.
- Comment/recovery changes are auto-carried by the rewrite engine (the RR
  percentage is re-adopted, CMT is droppable) but are not yet plan ops;
  that is the next step before the binding migrations.

### Comment/recovery plan-op notes (recorded with the combined rewrite)

- `EditPlan` gained `set_comment(bytes)` (empty removes the comment, like
  `rar c`) and `set_recovery(percent)` (rebuild the inline recovery
  record, like `rar rr`); at most one of each per plan, and they combine
  with deletes and renames in the same single rewrite.
- The core `edit_plan` now carries `force_rr`/`comment` through to the
  engine; comment/recovery ops are refused on multi-volume archives and
  alongside delete-everything, mirroring the legacy methods.
- Byte-parity tests: plan `set_comment`/`set_recovery` outputs equal the
  legacy `set_comment`/`add_recovery_record` outputs on twin archives; a
  combined delete+rename+comment+recovery plan lands atomically; refused
  ops leave every multi-volume file byte-identical.

### CLI edit migration notes (recorded with the edit commands)

- `rar d`, `rar rn`, `rar ch`, `rar c`, `rar rr` and `rar m` now drive
  `ArchiveEditor`/`EditPlan`/`ArchiveWriter` instead of the legacy facade.
  Name-based delete/rename semantics are preserved through catalog
  resolvers that mirror the legacy first-match rules (repeated delete
  names remove successive duplicates; a missing name fails the whole plan
  before any rewrite). `rar m` was the last create/append path left on
  the legacy writer and now uses `ArchiveWriter` like `rar a`/`u`/`f`.
- `rar a` on an existing archive deletes same-named members through the
  editor before its typed append; `-as` synchronization deletes stale
  members through the editor too. `rar ch` now also converts directory
  members consistently (the legacy name path failed on dir trees because
  expansion renames made later lookups miss).
- Remaining legacy edit surface: the staged `rar u`/`rar f`
  version-control rewrite (rename/delete on the staged copy, tested by
  `cli_version_control_keeps_previous_versions`) and the N-API
  `deleteEntries` task.

### Edit-migration completion notes (recorded with the bindings)

- The N-API `deleteEntries` task now drives `ArchiveEditor` (name
  resolution preserving the legacy first-match delete semantics,
  progress through `delete_entries_with_progress`, cancellation through
  `set_cancel_flag`); JS suite 33/33 against a rebuilt addon.
- The non-version staged delete inside `rar u`/`rar f` runs through the
  editor; only the version-control branch keeps its map-aware chained
  rename on the legacy seam (resolution semantics differ), and `rar k`
  stays on `RarArchive::lock`.

## Phase 5: internal directory convergence

Target layout after the APIs above have exercised the boundaries:

```text
src/
  archive/
    reader.rs
    writer.rs
    editor.rs
    transaction.rs
    discovery.rs
  model/
    entry.rs
    chunk.rs
    path.rs
    timestamp.rs
    compression.rs
    redirect.rs
  format/
    rar4/
    rar5/
  codec/
    common/
    legacy/
    modern/
  fs/
    atomic.rs
    safe_path.rs
    volume.rs
```

Convergence: the edit-transaction engine moved from `archive/rewrite.rs` to
`archive/transaction.rs`; `src/fs/{atomic,volume,safe_path}.rs` owns
filesystem policy; the CLI entry points live under `src/bin`; the N-API
binding is split across `lib.rs` (JS surface) + `options.rs` + `tasks.rs` +
`error.rs`; codec files are bucketed under `common/`/`legacy/`/`modern`;
and the format modules live at `src/format/rar4` + `src/format/rar5`.
Deferred to the Phase 6 breaking release: the remaining transitional
wildcard imports, the `#[doc(hidden)]` `rar40`/`rar50` re-export aliases
(and the equally hidden `format` tree visibility), and low-level module
splits that still reference the old names.

### fs/ convergence notes (recorded with the filesystem policy move)

- `src/fs/atomic.rs` (was `src/io_util.rs`): bounded reads, unique temp
  sibling staging, `create_new` file opening and atomic `replace_file`.
  The crate-internal `crate::io_util` import path moved to
  `crate::fs::atomic`.
- `src/fs/volume.rs`: `.partN.rar` name parsing, volume base/width and
  the part path builders (moved out of archive discovery).
- `src/fs/safe_path.rs`: `sanitize_archive_path` moved out of
  `rar50/extract` so format extractors consume the policy instead of
  owning it.
- `archive/discovery.rs` (was `archive/discover.rs`) keeps only
  `discover_volumes`, matching the target layout.

- [x] Move format modules only after model ownership is stable
      (both done: `rar50` and `rar40` moved to `src/format/rar5` and
      `src/format/rar4`; the historical public `rar5::rar40`/`rar5::rar50`
      paths are kept as `#[doc(hidden)]` re-export aliases until the
      breaking release).
- [x] Move filesystem policy out of format extraction/writing
      (done: `src/fs/{atomic.rs, volume.rs, safe_path.rs}` — see notes below).
- [ ] Split CLI binaries into `src/bin` plus shared command modules
      (done: entry points moved to `src/bin/{rar,unrar}.rs`, shared modules
      stay in `src/` referenced via `#[path]`).
- [x] Split N-API tasks/options/error mapping out of `src/lib.rs`
      (done: `error.rs` error mapping, `options.rs` option structs +
      validation/conversion, `tasks.rs` task implementations + factories;
      `lib.rs` keeps the JS-facing structs, module wiring and re-exports).
- [ ] Remove transitional wildcard imports and add dependency guards
      (guards added; codec legacy/common aliases removed after bucketing;
      remaining wildcard imports and the `#[doc(hidden)]` `rar40`/`rar50`
      re-export aliases are dropped at the Phase 6 breaking release).

## Phase 6: breaking-release decisions

Decision inputs and recommendations are recorded in
[ADR 0003](adr/0003-breaking-release-scope.md) (in-tree usage inventory,
rename surface, low-level module users). Each bullet below needs a
maintainer decision before execution.

- [ ] Decide whether to rename crate `rar5`.
- [ ] Split archive format from compression version in the public model.
- [ ] Decide which low-level modules remain supported.
- [ ] Move unsupported low-level APIs behind `raw`/`unstable`, if appropriate.
- [ ] Deprecate, then remove, legacy facade methods only after all in-tree
      consumers use API v2.

## Validation matrix

Every phase runs:

```text
cargo fmt --all --check
cargo clippy --workspace --all-features --all-targets --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo check --manifest-path fuzz/Cargo.toml --all-targets --locked
npm test  # after rebuilding the N-API addon when Rust exports change
```

Format/model changes additionally require:

- RAR4 fixture reads;
- RAR4/RAR5/RAR7 create/read roundtrips;
- encrypted, solid, quick-open, and multi-volume coverage;
- public compatibility-path compile tests;
- duplicate-name behavior tests before introducing IDs.

## Change-management rules

1. One architectural boundary per change; do not mix codec optimization with API
   migration.
2. Preserve old API semantics until the replacement has in-tree consumers.
3. Add characterization tests before moving stateful read/write logic.
4. Prefer re-export shims and conversion adapters over synchronized duplicate
   models.
5. Record every rejected abstraction in an ADR rather than leaving transitional
   code unexplained.
