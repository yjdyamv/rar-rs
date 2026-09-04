# ADR 0002: Format-neutral model and API v2

- Status: Accepted
- Date: 2026-09-05
- Scope: `crates/rar` public API and internal dependency direction

## Context

The core crate supports RAR 1.5–4.x, RAR5, and RAR7, but its shared entry
model is still owned by `rar50::headers`. `rar40`, `archive`, and the public
`ArchiveEntry` therefore depend on a RAR5 wire-format module even when they
operate on legacy archives.

At the API boundary, `RarArchive` also represents reading, writing, appending,
and editing. The valid method set depends on a runtime mode and optional read /
write state. This makes invalid-state errors possible and leaves several
low-level format types in the public compatibility surface.

A full crate split or immediate replacement of `RarArchive` would mix internal
architecture work with a large SemVer change and make byte-level interoperability
regressions difficult to isolate.

## Decision

### 1. Establish a private format-neutral model layer

`crates/rar/src/model/` owns normalized archive-domain types shared by all
formats. The first migration moves `FileHeader` and `DataChunk` without changing
their fields, defaults, or methods.

The model layer is a leaf:

- it must not depend on `archive`, `rar40`, or `rar50`;
- format parsers translate wire data into model values;
- format serializers consume model values;
- archive orchestration stores model values;
- codecs receive only the normalized data needed for decoding or encoding.

The initial module remains private. Existing public paths are preserved with
real re-exports from `rar50::headers`:

```rust
pub use crate::model::{DataChunk, FileHeader};
```

This keeps `rar5::rar50::headers::FileHeader` and
`rar5::rar50::FileHeader` source-compatible while internal consumers migrate to
`crate::model`.

### 2. Keep format-specific wire structures in format modules

`RawBlock`, `BlockMeta`, `ArchiveHeader`, header parsing, and serialization stay
under the RAR5 format implementation. RAR4 fixed-width block structures stay
under RAR4. Moving the common model does not merge the two wire formats or
introduce a lowest-common-denominator format trait.

### 3. Add API v2 beside the current API

API v2 will introduce role-specific facades:

- `ArchiveReader` for catalog, read, verify, and extraction;
- `ArchiveWriter` for create/append and consuming `finish()`;
- `ArchiveEditor` for identity-based structural edits.

`RarArchive` remains available as a compatibility facade until v2 is exercised
by the CLI and N-API bindings. It will not be converted into a public generic
typestate type.

### 4. Give entries catalog-scoped identity

API v2 will introduce an opaque `EntryId` and borrowed `EntryRef`. Duplicate
member names must remain distinct. Names become query keys rather than stable
identity.

An ID is valid only for the catalog generation that created it. Structural
edits invalidate prior IDs. Data offsets and member names are not IDs.

### 5. Introduce validated option types incrementally

New APIs use private-field builders and validated domain types such as:

- `CompressionLevel`;
- `DictionarySize`;
- `ThreadCount`;
- `SolidMode`;
- `Encryption`;
- `OverwritePolicy`;
- `PathLayout`;
- `ScanStrategy`.

Existing `CreateOptions` and `ExtractOptions` retain their source-compatible
fields. Conversion into the new options performs strict validation; old API
behavior is not silently changed during the internal model migration.

## Dependency direction

```text
CLI / N-API
    |
public reader / writer / editor API
    |
archive orchestration and transactions
    |----------------------|
format-neutral model       filesystem policy
    |
format implementations (RAR4, RAR5/RAR7)
    |
codecs / crypto / recovery
```

Forbidden dependency directions include:

- `model -> rar40` or `model -> rar50`;
- `rar40 -> rar50::headers` for common entry/chunk types;
- CLI or N-API calling internal codecs directly;
- wire-format structs leaking into new high-level API signatures.

## Compatibility policy

The following are explicitly deferred to a breaking release:

- renaming the `rar5` crate;
- splitting `ArchiveVersion` into format and codec-version types;
- hiding all existing low-level public modules;
- changing the semantics of name-based `read`, `delete`, or `rename`;
- removing `RarArchive`;
- changing public primitive accessors on `ArchiveEntry`.

New API is additive first. Deprecation starts only after CLI, N-API, examples,
and documentation have migrated.

## Consequences

### Positive

- RAR4 no longer depends on a RAR5 header module for its normalized model.
- Format parsing, archive orchestration, and public API can evolve separately.
- Reader/writer/editor types remove most runtime mode errors from API v2.
- Entry IDs provide correct behavior for duplicate member names.
- Existing users retain source-compatible low-level paths during migration.

### Negative

- During migration, old and new API layers coexist.
- `FileHeader` initially remains a mixed normalized structure; moving ownership
  does not immediately make every field strongly typed.
- Re-exports preserve a low-level compatibility surface that cannot be removed
  until a breaking release.
- Some internal modules will temporarily import both model and wire-format
  types.

## Validation

Each migration step must keep:

- workspace formatting and all-target Clippy clean;
- default and all-feature builds working;
- RAR4/RAR5/RAR7 roundtrip and fixture tests passing;
- legacy public type paths compiling;
- model defaults byte-for-byte compatible with their previous values;
- no executable `rar40` source dependency on `rar50::headers` for model types.
