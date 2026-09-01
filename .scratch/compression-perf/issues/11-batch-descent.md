# 11 — Software-pipelined batch descent (the collect's DRAM-latency lever)

Type: task
Status: planned — design below; implement and measure step by step

## Goal

The DLL collect is DRAM-latency-bound: ~12 M positions × ~5.5 BT4 descent
steps, each step one dependent random read into the 32 MiB son array
(~85-100 ns, the L3 is 16 MiB and the working set overflows it). WinRAR's
per-byte parse is ~3x faster (likely hand-assembly). The one lever that
directly attacks the latency: process several positions' descents
interleaved so one position's dependent read latency overlaps another's
compute. This is classic memory-level parallelism (MLP) via software
pipelining.

## The serial descent (per position)

State: `hash, current, floor, len0, len1, ptr0, ptr1, budget, longest`.

Each iteration:
1. Guard `current >= floor || pos - current > mask || budget == 0` → seal
   the two attachment points with NO_LINK and return.
2. `pair = (current & mask) << 1`; **load `son[pair]`, `son[pair+1]`** — the
   DRAM-bound dependent read (the next `current` comes from one of these).
3. Byte-compare `input[current+len..]` vs `input[pos+len..]` (the 8-byte
   word compare already landed) — independent of the step-2 load.
4. Branch on the compare; write the compared node to the appropriate
   attachment point; advance `current = resolve(pos, son[pair(+1)])`.

The dependency chain across iterations: step 4's `current` → step 2's
`pair` → the step-2 load → step 4's next `current`. Serial MLP = 1.

## The batch (N in flight)

Process N positions' descents one step each, in three phases per round so
the N independent loads are issued before any is used:

```
round:
  phase A (issue loads):  for i in active: pair[i] = (cur[i] & mask) << 1
  phase B (compute):       for i in active: guard, byte-compare, update
                           longest, decide the branch side
  phase C (consume):       for i in active: write the attachment, advance
                           cur[i] = resolve(pos[i], son[pair[i]+side])
  drop descents whose guard sealed them
until no active descents
```

The N loads of phase A are independent (distinct positions → distinct ring
slots), so they issue together; the N compares of phase B fill the DRAM
latency; phase C consumes. Round k+1's phase A depends on round k's phase
C (the new `cur`s), but the N loads *within* a round overlap. The collect
time drops from `steps × (read + compute)` to roughly `steps × compute +
read` per batch — a several-fold MLP win (N=4-8).

## Correctness

The phases only reorder independent operations; each descent's
state transitions are identical to the serial one. The son writes (the
insertion/attachment updates) happen at the same logical pointsaine, so the
tree structure is byte-identical. The collector byte-verifies every
report anyway (the safety net from the earlier corruption fix), so any
pipelining bug surfaces as a mismatch, and the regression fixture
(dense_x86_multi_chunk_roundtrips_byte_identical) plus the full suite lock
byte-identity.

## Integration (the collector)

The collector's per-position loop interleaves the tree query with the LR
probe, the fast-mode gating, and the committed_through skip. The batch must
respect these gates or the random-data fast path breaks (a regression we
hit with the chain finder).

Design: the collector gathers a run of positions that pass the tree-query
gate (`searching && max_distance>0 && max_length>=4 && pos+3<len &&
!fast_tree`) into a batch of N, runs the batched descent once, then
processes each position's matches + the LR probe + the committed_through
skip as before. Positions inside a committed match or fast-mode are
excluded from the batch (empty output) so random data keeps its fast path.

The batch boundary: process up to N gated-in positions, then flush. Because
a found match can advance `committed_through` (skipping later positions in
the run), a position's gate is decided when it's *added* to the batch, and
a position that the committed_through would skip but that was already
added still runs its descent (its output is then ignored) — on the DLL
(short matches) this is rare; on repetitive text the long matches set
committed_through and the skip dominates anywayreed, so the added-position
waste is bounded.

## Risks

1. Register pressure / loop bookkeeping with N in flight — the compiler
   must keep the N loads in flight. Write the three phases as three
   separate tight loops so the loads are issued before any result is used.
   Measure N=4 and N=8; if the compiler serializes, force the loads with a
   `core::hint::black_box` on the phase-C use or a manually-unrolled inner
   loop.
2. The input byte-reads (step 3) are also DRAM (the 13 MiB combined); the
   batch overlaps those too (the N compares' reads are independent).
3. The collector restructure is the largest risk (the gate handling). Keep
   the serial `matches()` intact as the fallback so the batch is opt-in and
   testable against it.
4. The MT path uses the same `matches()`; the batch benefits it equally
   (each slice's collect) — no special casing needed.

## Implementation steps

1. Add `TreeMatchFinder::matches_batch(&mut self, input, &[BatchPos], n, out_scratch)`.
   `BatchPos { pos, len_limit, max_distance, cut }`; returns per-position
   match lists. Start with a correctness check: batch output must equal the
   serial `matches()` output for the same positions (a unit test comparing
   them byte-for-byte on the corpora).
2. Restructure `collect_block_matches` to batch the gated-in tree queries.
3. Land the byte-identity tests: the existing roundtrip + regression suite
   must stay green (it encodes with the batch path by default).
4. Measure the DLL (seq + mt8) and all corpora: speed win + ratio
   unchanged (the parse must be byte-identical; if not, the batching is
   wrong). Only land if the bytes are identical and the collect measurably
   faster.

## Success criteria

- The collect (and thus the DLL seq/mt8) is measurably faster (target
  ~1.5-2.5x on the collect), and the output bytes are **identical** to the
  serial path on every corpus — this is a pure-latency-hiding change, not
  a quality/speed tradeoff.
- 211+ tests green, fmt/clippy clean.