# 11 — Software-pipelined batch descent (the collect's DRAM-latency lever)

Type: task
Status: resolved — pipelined first-step value-carry implemented, tested byte-identical, and landed 2026-09-07 (see the final entry); the CLI-overhead part was closed 2026-09-07 too

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
## 2026-09-01: the interleaved batch is NOT byte-identical — plan revised

Implemented `matches_batch` (interleaved, N=8, three phases) and compared it
against serial `matches()` per position on real corpora (300 K of the DLL,
1 MB xml, window 2^6/8/10): thousands of positions differ on every corpus
and window. The cause is the BT4's insertion-order invariant: "a slot is
always written during its own position's insertion before any link can lead
to it." A later position in the batch (k+5) reads an earlier position's (k)
son slot; the interleaved phases let it read the pre-batch value (k's
previous insertion, or 0 on first touch) instead of k's re-insertion value.
The candidate sets overlap (every position references the earlier batch
positions), so the full MLP cannot be extracted while preserving byte
identity.

The only byte-identical batching is to run the descents strictly in order
(identical to the serial, heads set upfront — harmless since a descent never
touches a later position's head) and pipeline only the *first* step's loads
(upfront prefetch of each position's first son slot — known from the
setup's cur). That hides ~1 of ~5.5 steps per position — roughly an 18%
collect win, not the 2-3x planned. Given the modest payoff versus the
complexity and the several already-rejected collect levers (nice-exit,
chain finder, window caps), this is parked: the BT4's structural invariant
is the real ceiling, and matching WinRAR's hand-assembled per-byte parse in
pure Rust is unlikely. The landed speed wins (hash 20, adaptive slices)
stand.

Open: the CLI's ~1 s overhead over the library core (batch-wave + nested MT
pool nesting) is pure waste and worth a profile pass — a lower-risk user-
facing win than the parse core.

## 2026-09-07: the CLI "~1 s overhead" is not reproducible — closed

Measured with a fresh A/B (release, m3/mt8, 24.5 MB x86 corpus = tsc.exe;
`rar-cli` enables `parallel`, the library example must rebuild with it too —
a default-features example silently runs single-threaded):

| path | median |
|---|---|
| raw codec (`encode_with_auto_delta_filter` → `encode_with_auto_x86_filter`, 24.5 MB as in the writer's file_origin branch) | ~4.8 s |
| full typed writer (`ArchiveWriter::create_with` + `add_batch` + `finish`) | ~5.0 s |
| CLI (`rar a -m3 -mt8`) | ~5.2 s |

The CLI tracks the library within ~3-5%. The old claim compared the CLI
against a bare `encode_with_auto_x86_filter` call and predated the CLI
parallel-feature fix (38a1131); the residual pipeline cost (CRC + BLAKE2sp +
headers + file I/O + wave write-back + finalize) is ~0.2-0.4 s and is paid
identically by every caller. The "nested MT pool" worry is moot: the batch
wave (`compression_pool_for(threads)`) and the inner MT encode
(`compression_pool()`) resolve to the **same** cached pool instance, so no
second pool is ever spawned. Harness: `crates/rar/examples/clioverhead.rs`
(parallel-gated; `codec`/`writer` modes).

## 2026-09-07: pipelined first-step value-carry — landed

Implemented the "pipeline only the first step's loads" lever from the
2026-09-01 note, but as a **settled value-carry** rather than a prefetch:
the next gated position's first tree step is *read* at the end of the
preceding iteration — `head[hash]` (via `seed_for`) and the first
descendant's `son[pair]`/`son[pair+1]` — when those values are provably
settled (every position through `pos` has inserted, and nothing else writes
the tree between the seed read and the next turn). The seed is carried
across the loop boundary and handed to `matches_seeded`, whose descent
starts from the carried `current` + child links instead of loading them.

Byte identity rests on one invariant, proven and now unit-tested: within a
single descent every node the walk visits is written only after it is read,
and no attachment slot aliases another slot encountered in the window, so
reading the child pair one iteration early is value-equal to the serial
top-of-iteration read. (The 2026-09-01 interleaved-batch failure had a
*batch window of many positions* sharing son slots; this carries across one
pure loop-boundary step, where no interleaving exists.)

Changes:
- `match_finder.rs`: `matches` split into `matches` + a shared `descent`;
  new `matches_seeded`, `seed_for` (guards the pair read with the same
  floor/window check the descent starts with), and `prefetch_head_for`
  (x86_64 `_mm_prefetch` of the next head slot, issued at the top of the
  gated iteration).
- `encoder.rs::collect_block_matches`: a `pending` seed slot recomputed at
  the end of every iteration for `pos+1`, gated by the *exact* mirror of
  the tree-query gate using the just-updated `committed_through`/`fast_tree`
  (so a seed exists iff the next turn will descend); consumed in the gate.
- `examples/collectbench.rs` (parallel-gated): file-input bench toggling
  seq/mt8, asserts the decode is byte-identical; baseline + post timings.
- Test `seeded_first_step_is_byte_identical_to_serial`: replays 280 KiB
  mixed (phrase repeat + zeros + random + repeated phrase) at window 2^15
  through both paths — per-position reports equal, and the final head/son
  tables byte-equal.

Results on tsc.exe (24.5 MB, m3, dict 32 MiB), 3-5 runs each, release:

| path | pre | post | Δ |
|---|---|---|---|
| seq  | ~9.56 s (mean) | ~9.21 s (mean) | −3.6% |
| mt8  | ~2.23 s (mean) | ~2.15 s (mean) | −3.7% |

Ratios byte-identical on both paths (seq 33.01%, mt8 33.57%); decode
byte-identical; 207 lib tests + rar50_roundtrip/format_assertions/
rewrite_tests green; clippy `-D warnings` clean.

Verdict vs the goal: byte-identical, no quality tradeoff, but only ~1/5 of
the hoped ~18% collect win — the seed's reads land at the end of the
bookkeeping tail, which is too short to hide a full DRAM read; the rest of
the step-1 latency is absorbed by the next turn's OoO overlap anyway. The
head `_mm_prefetch` helps marginally. Landed because it is free and
monotone; the BT4 insertion-order ceiling (2026-09-01) still stands for any
real batch.
