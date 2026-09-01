# 08 — persistent tree finder corrupted multi-chunk members (fixed)

Type: bug
Status: resolved (308157b)

## Symptom

13 MB ntoskrnl.exe at m3: our archives failed `unrar t` AND WinRAR `t`
with a checksum error. The corruption was silent — nothing in the test
suite caught it (synthetic corpora never triggered it).

## Root cause (two sibling bugs in TreeMatchFinder)

1. **grow_to wiped the son array.** When the window grew across a chunk
   boundary (`tree_window = min(dict, combined)` grows as combined grows),
   `son` was replaced with `vec![0; ...]` while the head table survived.
   `NO_LINK = u32::MAX`, so a zero slot is a *valid* link to position 0 —
   the next descent followed the zeros to the member head. The bogus
   matches copied the DOS MZ header over real code (decode-side trace:
   `match dist=4585040 len=7` copying `4d 5a 90 00 03 00 00` from offset 0).
2. **rebase left links at the wrong ring slots.** The son array is indexed
   by `(pos & mask) << 1`; shifting link *values* in place without moving
   them between slots left the slots the new frame reads stale or zero
   (same position-0 vector).

The len0/len1 prefix-skip in the descent then trusted a ≥4-byte "match"
from a sibling node and reported the bogus match without comparing the
first bytes.

## Fix (three layers)

- `grow_to`: copy the old links into the grown array (positions below the
  old mask map to identical slots under a larger mask).
- `rebase`: migrate each surviving link to its new slot
  (`(pos - sub) & mask`), drop slid-out links.
- Collector safety net: byte-verify every tree report before the parse can
  price it (truncate over-reports, drop non-matches) — cheap, and catches
  any future invariant break.

## Regression

`tests/fixtures/x86/ntoskrnl_dense_prefix.bin` (129,400 B of a real
Windows kernel image; reproduced the corruption at byte 129,334 under dict
2^3 + 64 KiB chunks on the old code). Test
`dense_x86_multi_chunk_roundtrips_byte_identical` roundtrips 3 dict/chunk
configs; verified it FAILS on the pre-fix tree and passes now.

## Lesson (fragility)

A parameter change (block size) exposed a latent encoder bug that shipped
corrupt archives silently. The safety net + the dense-binary fixture close
this class: future changes that break the tree's invariants now either
fail the regression test or get caught by the collector verification.
