# 04 — window-level incompressible skip for MT encode

Type: task
Status: open

## Question

On fully incompressible members the MT encode still costs ~486 ms (mt8,
64 MiB) — collect (tree/LR descents) + Huffman encoding of literal-only
blocks that the member-level STORE fallback throws away anyway. Can the
window path detect an incompressible window cheaply and skip the parse?

## Analysis

- The member-level fallback (`packed_size >= file_size` → STORE) makes all
  this output disposable for fully-incompressible members.
- A window-level probe (sampled 4-byte-window distinctness, like
  `mt_tail_is_incompressible`) can gate the skip. A skipped window emits
  literal-only blocks (~1.05-1.1× input) — the same ballpark the parse
  produces on random data, so the member decision is preserved.
- **The ratio risk:** a window ≥99.5% random with a small compressible patch
  parses ~0.3-0.7% smaller than literal-only. A member hovering within that
  band of the 100% boundary could flip compressed → STORE. Any member that
  ends up compressed has ≤ ~93% random content per window (else output ≥
  ~1.05× ≥ 1.0), so a ≥99.5% threshold never fires on windows of a
  compressed member — the flip is bounded to members sitting within ~0.5%
  of the boundary with a ≥99.5%-random tail. Rare, but violates strict
  "ratio not worse".

## Options

- Ship it with a strict threshold (≥99.5% distinct) and document the bounded
  flip risk; validate on ratiocheck + a boundary-case corpus.
- Reject: keep the honest parse; the current 486 ms is mostly unavoidable
  per-block work.
- Middle ground: skip only collect, keep encode, when the head block of a
  window is matchless AND the probe passes (still heuristic).

## Decisions so far

Not started. The safe-per-member variant needs a boundary-case corpus
(members at 99-101% ratio) to quantify the flip before committing.
