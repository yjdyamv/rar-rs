# ADR 0008: One-way family layering behind an `Engine` seam

- Status: accepted (executed 2026-09)
- Date: 2026-09-19
- Related: [ADR 0002](0002-format-neutral-model-and-api-v2.md) (dependency
  direction, updated by this one), [ADR 0006](0006-public-api-convergence.md)
  (role facades), [ADR 0007](0007-raw-feature-retired.md) (same cost/benefit
  reasoning for dropping a switch)

## Context

Each container family (`format::rar13`, `format::rar4`, `format::rar5`) used to
own half of its pipeline as `impl RarArchive` blocks. That made `format` name
`archive`'s type, so the two modules were mutually dependent: `format` could not
be read — let alone compiled or tested — without the engine, and the per-family
code had grown an implicit second façade beside the real one.

A first attempt introduced `engine` as shared vocabulary plus an `Engine` trait
and converted the blocks one at a time. The 2026-09 audit of the actual
references (not the intent) then found the inversion was still true in five
places the incremental conversion had left behind, and that the boundary test
could not see one of them:

- `engine/state.rs` named `format::rar4::LegacyDecoder` (an `engine -> format`
  edge, i.e. the cycle closed again from the other side);
- `format/rar5/create.rs` named `crate::DictionarySize`, a crate-root re-export
  of `archive/writer.rs` — a `format -> archive` edge spelled through a
  re-export, invisible to a test that only greps `crate::archive`;
- `detect` imported `format::rar5::RAR5_SIGNATURE` and `options` imported
  `format::rar5::MAX_METADATA_BYTES`, so leaves depended on a family;
- the RAR4 reader called the RAR5 decoder's `max_packed_bytes`, the RAR4 split
  path called `rar13::file_checksum`, and `rar13` called `rar4`'s DOS-time
  helpers: family-to-family edges for primitives neither family owned alone;
- `recovery` reuses the RAR4 envelope reader with `EnvelopePolicy::REPAIR`,
  which is `recovery -> format` — the opposite of what ADR 0002's diagram drew.

## Decision

1. **`engine` is the vocabulary layer below both** `archive` and `format`:
   `ReadState`/`WriteState` and their groups, `ArchiveEntry`/`BatchEntry`/
   `MemberPlan`, the container-identity flags, the persistent solid-chain
   decoder/encoder carriers, `Engine`, and the limits both sides need. It never
   names `archive` or `format`.
2. **Family code is free functions over a borrowed context**, not methods:
   `pub(crate) fn …(cx: &mut dyn Engine, …)`. `Engine` is split into object-safe
   capability groups (`EngineState`, `CatalogOps`, `StreamOps`,
   `HeaderCryptoOps`, `VolumeOps`, `WriteServices`) with `Engine` as the
   umbrella and a blanket impl, so a reader can see which capability a given
   function actually needs. `Parts` hands out the disjoint state fields a family
   function borrows together, which a per-method trait cannot express.
3. **Engine services own the invariants the families used to maintain by hand**:
   `push_entry`/`clear_catalog`/`replace_catalog` (no `entries_mut()` on the
   trait), `bytes_written`/`add_bytes_written`/`current_volume_index`,
   `record_quick_open_entry`, `begin_solid_member`. A family writer cannot
   silently desynchronise the catalog order, the volume budget or the chain
   framing.
4. **Vocabulary belongs to the leaf that cannot depend on more**: the signature
   table lives in `detect`; `DictionarySize` and `MAX_METADATA_BYTES` live in
   `options`; the legacy decoder carrier lives in `engine`; civil-time
   primitives live in the public `time` module (shared with the CLI).
5. **`format::shared` is the dispatch and adapter layer**, not a family-neutral
   layer: the single family `match`/dispatch per operation lives there, and the
   RAR5-only concepts (redirect records, NTFS streams, parallel extraction,
   BLAKE2 verification) are adapted there through `entry_ext` and `extract/*`.
   Family-to-family primitives (rolling checksum, DOS time, `max_packed_bytes`)
   live there rather than in one family.
6. **`recovery` sits above `format`**: it reuses the RAR4 block envelope with
   `EnvelopePolicy::REPAIR`, so `recovery -> format` is intended and
   `format -> recovery` is forbidden.
7. **The role facades stay off the internals**: `ArchiveReader`/`Writer`/
   `Editor` reach family code only through the method-shaped seam in
   `archive/ops.rs`.
8. **The direction is enforced, not intended**:
   `tests/architecture_boundaries.rs` checks each rule on shipped source lines
   (comments and test modules exempt), parses the crate-root re-export list so
   `crate::<reexported item>` cannot bypass the `format`-off-`archive` rule, and
   `tests/dependency_matrix.rs` snapshots the whole cross-layer edge set
   (`docs/dependency-matrix.txt`) so a new edge is a reviewable diff even when
   no rule covers it yet.

## Consequences

Positive:

- The dependency graph is acyclic and one-way:
  `archive -> format ->
  engine -> {codec, crypto, fs, model, options}` with
  `detect`/`version`/ `vint`/`time`/`error` as leaves. `format` compiles without
  naming `archive`, so a family pipeline can be read (and reviewed) on its own.
- Each piece of vocabulary has exactly one owner, so the same constant or helper
  cannot drift between two layers or two families.
- The invariants that used to be "remember to also update X" are engine
  services, and the remaining shared state is documented as such.

Negative:

- Every family function is a free function with a `cx` first parameter instead
  of a method, and `Engine`'s capability groups have to be named in
  `archive/ctx.rs` (one `impl` per group).
- `Engine` and `RarArchive` keep two façades whose names overlap in eleven
  places; the trait impl forwards to `archive/engine.rs`. That is the seam, but
  it is a place where a method can be added twice.
- `format::shared` reads as if it were format-neutral while it adapts
  RAR5-specific concepts; the module docs now say so explicitly.
- `recovery -> format` means recovery cannot be read without the family
  envelope, which is exactly the reuse we wanted and also a coupling to keep in
  mind when changing the RAR4 reader.

## Rejected alternatives

- **Feature-gate the legacy families and/or `recovery`**: they are the product
  scope (RAR 1.3–4.x read/write, `r`/`rv`/`rc`, the `-hp`/solid paths that
  depend on them), so a default-on switch buys nothing for this repository's CI,
  local builds or released artifacts while adding 50–90 `#[cfg]` sites (on top
  of the existing 115) and a gate on every future change. Recorded in
  `PLAN.md`'s 一致拒绝 list.
- **Move `RarArchive` itself below `format`**: it would drag the orchestration
  and lifecycle down with it instead of leaving one narrow seam.
- **Generic `E: Engine` instead of `&mut dyn Engine`**: monomorphisation and
  `dyn`-friendly call sites argue for the trait object; the family code is not
  performance-critical enough to want the generic.
