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

Phase 3 core slice completed here: the Rust writer API and transaction boundary are
in place. CLI and N-API migration remain intentionally deferred.

- [x] Add `CompressionLevel`, `DictionarySize`, `ThreadCount`, and `SolidMode`.
- [x] Add `WriterOptions`, `AppendOptions`, and `EntryWriteOptions` with private
      fields and validated builders.
- [x] Add `ArchiveWriter::create` and `ArchiveWriter::append` as wrappers around
      the existing implementation.
- [x] Add consuming `ArchiveWriter::finish() -> WriteReport`.
- [x] Return final volume paths in `WriteReport`; remove binding-side rediscovery
      from new APIs.
- [ ] Migrate CLI create/append and N-API create/append to the writer API.
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

Deferred with the CLI/N-API migration: binding-side create/append still uses
`CreateOptions`/numeric levels/`close()`, and RAR4 append remains
unsupported (matching the legacy seam).

## Phase 4: editor API and combined transactions

- [ ] Add `ArchiveEditor` catalog and ID validation.
- [ ] Extract internal index-based delete and rename operations.
- [ ] Add ID-based delete/rename APIs.
- [ ] Add catalog generation invalidation after structural edits.
- [ ] Introduce `EditPlan` only after individual ID operations are stable.
- [ ] Combine delete, rename, comment, and recovery changes into one rewrite.
- [ ] Define and test multi-volume transaction/rollback behavior.
- [ ] Migrate CLI and N-API edit operations.

Exit criteria:

- stale IDs are detected;
- duplicate names can be edited independently;
- a failed combined edit leaves all original volumes intact.

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

- [ ] Move format modules only after model ownership is stable.
- [ ] Move filesystem policy out of format extraction/writing.
- [ ] Split CLI binaries into `src/bin` plus shared command modules.
- [ ] Split N-API tasks/options/error mapping out of `src/lib.rs`.
- [ ] Remove transitional wildcard imports and add dependency guards.

## Phase 6: breaking-release decisions

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
