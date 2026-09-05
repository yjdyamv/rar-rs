# ADR 0004: Single `ArchiveVersion` table

Status: accepted (grilled through the `grill-with-docs` flow, 2026-09-05).

## Context

Phase 6 decision 2 (ADR 0003) split version vocabulary into two orthogonal
public axes: `ArchiveFormat` (`Rar40`/`Rar5`, the container family) and
`ArchiveVersion` (`Rar50`/`Rar70`, the RAR5 member codec version), plus an
internal `CompressionVersion` (`V50`/`V70`) for the RAR5 writer. Grilling that
split against `CONTEXT.md` surfaced several inconsistencies:

1. **Incompleteness.** The vocabulary contained "4 5 7" (`Rar40`/`Rar5`/`Rar70`)
   but never the full RAR4 unpack-version range (`15/20/26/29/36`). `Entry`
   has no public accessor for `unp_ver`, so RAR4 codec versions were
   invisible to consumers despite being the versions RAR4 members actually
   decode with.
2. **Naming drift.** Individual variants were inconsistent (`Rar50`/`Rar70`
   RAR5-style vs `V50`/`V70` RAR7-style). The wire-level `comp_version 0/1`
   and legacy `unp_ver 15–36` were not unified onto one table.
3. **Wrong field name in docs.** `version.rs` doc comment mentioned
   `Entry::unpack_version`; the actual model field is `unp_ver`. `CONTEXT.md`
   propagated that error.

## Decision

Replace the two-axis model with a **single public `ArchiveVersion` table**
covering every member codec version this library reads and writes:

| Version | Codec                          | Container    | Writable |
|---------|--------------------------------|--------------|----------|
| `v15`   | RAR15 (adaptive-Huffman LZ)    | RAR 1.5–4.x  | —        |
| `v20`   | RAR20 (LZSS + Huffman)         | RAR 1.5–4.x  | —        |
| `v26`   | RAR20 (LZSS + Huffman)         | RAR 1.5–4.x  | —        |
| `v29`   | RAR29 (LZSS + Huffman + PPMd)  | RAR 1.5–4.x  | yes      |
| `v36`   | RAR29 (same codec as `v29`)    | RAR 1.5–4.x  | —        |
| `v50`   | RAR5 v50 (64-entry distance)   | RAR5         | yes      |
| `v70`   | RAR7 (80-entry DCX)            | RAR5         | yes      |

- Variants are two-digit `V15`–`V70`; `as_str()`/`Display` yield `"v15"`…`"v70"`.
- **No public container axis.** The container family is *derived* from the
  version (`v15`–`v36` → RAR 1.5–4.x envelope, `v50`/`v70` → RAR5 envelope);
  `ArchiveFormat` is removed from the public API. `ArchiveVersion::is_legacy()`
  reports the family; `is_writable()` reports the writable subset.
- `CompressionVersion` is deleted. The writer exposes a single
  `WriterOptions::compression(ArchiveVersion)` knob; the writable subset is
  `{v29, v50, v70}` and read-only versions are rejected at validation
  (`InvalidOption`) rather than silently downgraded.
- `CreateOptions.format_version` becomes `ArchiveVersion` (default `v50`).
- `ArchiveEntry::version()` maps each member onto the table: RAR4 via
  `unp_ver`, RAR5 via `comp_version`.
- `from_v70(bool)` and `uses_extra_dist()` (v70 only) keep their semantics.
- CLI: `-ma4` → `v29` (legacy RAR4 pipeline), `-ma5`/default → `v50`,
  `-ma7` → `v70`.

`v29` and `v36` are behaviorally identical (both dispatch to the RAR29 codec;
`>= 29` is one decode bucket, same cipher shape); `v29` is the RAR 3.x-era
value that the RAR4 writer emits, `v36` the RAR 4.x-era value readers accept.
Both are listed so the read side can report faithfully.

## Consequences

- Public API shrinks by `ArchiveFormat` and `CompressionVersion`; the writer
  tail (`is_writable` validation, `is_legacy` container selection) moves behind
  one knob.
- `ArchiveFormat`/`CompressionVersion` string constants (`"rar40"`, `"rar5"`,
  `"rar50"`, `"rar70"`) had zero consumers and are gone; machine-readable names
  are the two-digit strings.
- This is a breaking change; it ships in the Phase 6 breaking release alongside
  ADR 0003, superseding the two-public-axis shape of decision 2.