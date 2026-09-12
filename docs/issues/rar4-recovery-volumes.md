# RAR4 recovery volumes (`.rev`) are not implemented

Status: open — `rv`/`rc` refuse RAR4 sets with a clear error (2026-09).

## What WinRAR does

Official `rar rv` creates `.rev` files for RAR4 volume sets. Verified locally
with Rar 5.91, 6.23 and 7.23:

- for an old-naming set `s.rar` / `s.r00` / `s.r01`, WinRAR 7.23 writes
  `s3_1_1.rev` (naming is not the `.partN.rev` scheme);
- `rar rc s.rar` after deleting `s.r01` rebuilds the volume
  byte-identically.

The RAR4 `.rev` container is **not** the RAR5 REV5 format:

| file | leading bytes | size |
| --- | --- | --- |
| official RAR4 `.rev` | legacy container (`{t...`) | one padded volume |
| our former output | `Rar!\x1aRev` (REV5) | REV5 header + parity |

## What we do now

`recovery/rev50.rs::ensure_rar5_volume_set` checks the first volume's
signature; `build_recovery_volumes_for_set` and
`rebuild_missing_volumes(_with)` return `RarError::Unsupported` for RAR4 sets
before writing or scanning anything, and `rar rv` / `rar rc` surface that
error.

Before this gate `rv` silently wrote a REV5 file next to a RAR4 set, which
official WinRAR rejected with a checksum error, and `rc` could not read
official RAR4 `.rev` files (reported "no recovery volumes found").

RAR5 `.rev` creation/rebuild is unaffected and stays cross-validated against
WinRAR in `crates/rar-cli/tests/winrar_interop/recovery.rs`.

## Why not port unrar

The legacy `.rev` codec lives in unrar's `recvol.cpp`. The unrar license is
not compatible with this project's BSD-2-Clause, clean-room provenance
policy (the same reason the RAR5 side is a rars/format-notes port), so an
implementation has to be reverse-engineered from fixtures.

## Next steps

1. Collect fixtures: RAR4 sets created by us and by WinRAR, with their
   official `.rev` files (5.91 / 6.23 / 7.23), covering missing first /
   middle / last volumes, padded and unpadded volume names, and sets with
   split members.
2. Reverse the header (volume count, per-volume CRCs, parity layout) and
   write reader tests first (`rc`), then implement the writer (`rv`).
3. Cross-validate both directions against the locally installed WinRAR
   (7.23 still reads and edits RAR4 sets, so `rc` parity is testable).
