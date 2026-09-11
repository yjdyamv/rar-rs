# 09 — DLL single-thread parse speed (m3): measurements & levers

Type: task
Status: open — measured; one micro-opt landed (dd2d01c), the big lever (a
cache-resident near finder) is designed but not implemented

## Where the time goes (13 MB ntoskrnl, m3 seq, this machine)

Phase timers on the encode path (temporary instrumentation):

- plain path: collect 5.3 s (67%), dp 2.4 s (p1 1.1 + p2 1.3), encode 0.09 s
- x86-filtered path (the user-facing one): collect ~5.3 s, dp ~3.2 s
  (the transform makes the data more repetitive -> longer tree chains)

WinRAR 7.23 does the whole 13 MB in 1.8 s (mt1) / 0.42 s (mt8).

## Root cause of the collect cost

The BT4 descent runs ~5.5 steps per query (12 M queries), each step = one
dependent random read into the multi-MiB son array (~85-100 ns, DRAM):
the descent is latency-bound, not compute-bound. Depth histogram: spike at
depth 0 (10%, empty hash buckets), tail to 32; the floor guard bounds the
depth (newer-child links), so the chain budget (96 at m3) is never the
limit — budget 8 == budget 96 output-wise.

## Measured levers (all ratio-checked)

| change | dll speed | dll ratio | verdict |
|---|---|---|---|
| u64 word compare in descent | -10% (8799→7904) | identical | LANDED (dd2d01c) |
| tree window cap 4 MiB | -27% collect | +0.9% | rejected (ratio) |
| tree window cap 2 MiB | -37% total (1.6x) | +1.9% (plain) / +2.2% (x86f) | rejected (ratio) |
| DP pass 2 at m3 dropped | -1.3 s | +0.47% dll, +1.7% text64 | rejected (ratio) |
| prefetch son/input (T0) | regression ~+15% | identical | rejected (cache pollution) |
| mid-range via LR (cap + LR min_dist) | not built | — | LR full-cadence probes would cost ~4 s |

## The remaining lever: cache-resident near finder (two-tier)

The 32 MiB son array cannot fit the 16 MiB L3 (shared with input etc.).
A separate small hash-chain over the recent ~1-2 MiB (prev ring + head,
~8 MiB total, L3-resident) consulted FIRST; the big tree only as the far
fallback. Consistency analysis: every position must stay reachable for far
matches, so tree insertions cannot simply be skipped for chain-resolved
positions (a far match's source may be a chain-resolved position). Options
to explore: (a) keep the tree insertion but only on the fallback path and
measure the far-match loss; (b) batched re-insertion of chain-expired
positions (no net gain — same descent cost); (c) accept the sampled LR for
the mid-range (probe cadence must be gated hard).

## Note

Ratio is the contract: every candidate speed change was byte-diffed on the
corpora; the tree-window caps and pass reductions all violated it and were
rejected. The DLL ratio itself now wins vs WinRAR (43.90% vs 44.81%
x86-filtered); the speed gap (3.6x mt1, ~7x mt8) is the open item.

## 2026-09-01 follow-up: the levers tried and rejected, and what's left

The session landed hash 20 bits (byte-identical, -4-15%) and adaptive MT
slices (dll mt8 -50%) and then tried several further collect speedups. All
measured on this machine, all rejected because the speed did not pay for the
ratio cost (the plan's ratio contract; the user accepts a modest ratio hit
but these were steep):

- BT4 nice_len early exit (stop descending at a match >= N): at 32 it
  almost never fires on the DLL (matches are 4-15 bytes) so no speed; at 8
  the DLL was -23% at +4% ratio, text64 +3.5% — steep.
- Cache-resident chain near-finder (head + prev ring, the tree as far
  fallback): three configs (T=4 break-first, T=8 break-first, K=16 longest)
  were all net-negative. T=4 gave the DLL -46% but +8.8% ratio (the chain's
  first-match bias loses the longer matches the tree's descent finds) and
  broke the random-data fast path (rand64 15 s + compressed). T=8 held the
  DLL ratio (+0.37%) but gave no speed (the >=8 first-matches are rare) and
  the chain's per-position find overhead slowed rand64 to 10 s.
- Tree window caps (8 MiB, 4 MiB): the L3 is 16 MiB and the working set
  (13 MiB input + son + 4 MiB head + LR) overflows it, so a smaller son
  barely helps; 8 MiB gave ~0 speed at +0.09% ratio, 4 MiB -27% collect at
  +0.9%.
- DP pass-2 dropped at m3 (1 pass): -15% speed but xml +4.8%, text64 +1.7%.

Verdict: the BT4 descent is genuinely DRAM-latency-bound (12 M queries x
~5.5 dependent reads into a 32 MiB son). WinRAR's per-byte parse is ~3x
faster and is likely hand-assembly-tuned; matching it in pure Rust needs
software pipelining (batch several positions' descents so one position's
DRAM read overlaps another's compute) — the designed but unbuilt next lever.
Also on the list at the time was a claimed ~1 s CLI overhead over the library
core (batch-wave + nested MT pool nesting); that was **disproven on
2026-09-07** and re-measured on 2026-09-11 — see the last section.

## 2026-09-08: son-pair prefetch extension — measured negative, keeps the
## BT4 ceiling (issue 11 already killed the interleaved batch on the
## insertion-order invariant; this was the last untried value-neutral lever)

Tried the one remaining byte-identical pipelining step: within `descent`,
after reading the current node's `(child_less, child_greater)` pair,
prefetch BOTH children's son pairs (`T1`/L2 hint, son-only — the rejected
`T0` run prefetched son+input together) so whichever the byte compare
picks is already in L2 next iteration. Value-neutral by construction
(prefetch touches nothing), byte-identity trivially preserved: 210 lib
tests green, seq/mt1 ratio identical, decode byte-identical.

A/B on tsc.exe 6 MiB x86 prefix (m3, dict 32 MiB), 20 interleaved samples
each, release: baseline median 3048 ms vs prefetch median 3078 ms
(≈ +1% slower, inside the ±10% machine noise but not a win on either
median or min). On full 24.5 MiB tsc.exe it flip-flopped with the same
signature. The untaken-branch line pollutes L2 on dense binaries, and the
dependent `input[current+len]` compare reads (the other ~50% of step
traffic) are not prefetchable ahead of the branch. Consistent with the
earlier `T0` son+input rejection — the MLP issued ahead of the branch is
50% wasted, and every level of cache here is already contended by the
13-24 MiB working set.

Closing: the pipelined-first-step value-carry (landed −3.6%) is the full
extent of byte-identical BT4 pipelining. The interleaved batch (issue 11)
and both prefetch variants are measured negative. Any further per-position
step reduction requires the tradeoffs settled in the issue 13 verdict
(MT-only low-step search became the MT default; `RAR_RS_FAR_BAND` is a
dormant seq opt-in) — neither is byte-identical, so both are explicit-tradeoff
options.

## 2026-09-11: CLI orchestration overhead re-measured — not reproducible

The ~1 s CLI overhead claimed above does not reproduce. Release build,
16-core host, three runs each (`clioverhead` = raw codec vs full typed
writer; `rar` = the whole process), m3, `-mt8` unless noted:

| workload | codec | writer | CLI | CLI - writer |
|---|---|---|---|---|
| 64 MiB text | ~200 ms | ~260 ms | ~328 ms | ~70 ms |
| 5.7 MiB DLL (auto x86 filter) | ~320 ms | ~580 ms | ~655 ms | ~75 ms |

Extra checks: 256 x 256 KiB text files (many batch waves) m3 mt8 ~620 ms vs
mt1 ~1900 ms (clean 3x scaling, identical packed size); re-adding the same
set over the existing archive ~630 ms (the editor/append path adds nothing
measurable); a 256-file solid chain gives mt8 ~= mt1 (~4.15 s) — structural
(issue 06), not waste.

The CLI tracks the typed writer within ~15%, and the wave pool and the
inner MT pool are the same cached pool (`compression_pool_for(threads)` ==
`compression_pool()` once `-mt` sets the global), so there is no nested
spawn. An archive-local `WriterOptions::threads` override that differs from
the global would still pair `compression_pool_for(override)` with the
default `compression_pool()`; that is a library-only configuration (the CLI
always sets the global), so it is not part of this claim. The residue over
the raw codec is container/hash/pipeline cost every caller pays. No fix is
warranted; the map verdict (2026-09-07) stands.

## 2026-09-11: design — per-bucket software-pipelined collector

### Why the obvious batching failed

Issue 11 rejected interleaved batch descents: a position's descent reads the
tree state that *every earlier position's insertion* wrote, so processing
positions out of order changes which candidates they see. The prefetch
variants (T0 son+input, the son-pair T1 follow-up) were measured negative:
the untaken branch's line pollutes L2, and half the step traffic
(`input[current+len]`) cannot be fetched ahead of the compare that chooses
it. The pipelined-first-step value-carry (`seed_for` / `matches_seeded`) is
the full extent of *within-one-descent* byte-identical pipelining.

### The lever that is left: overlap independent descents

Each BT4 bucket (positions sharing `hash4`) is its own binary tree. A descent
that starts from `head[h]` only ever visits positions whose hash is `h`
(every node is reachable only through the bucket it was inserted into), so
the `son` slots it reads and rewrites are disjoint from every other bucket's.
The only tree writes that can affect a descent from bucket `h` are insertions
of positions whose hash is `h`. Therefore:

- descents with **different hashes are fully independent** and may be
  interleaved with byte-identical results;
- descents with the **same hash must stay strictly ordered**.

That is the correctness argument the earlier batch attempts lacked. Keeping
`W` descents in flight gives the CPU's out-of-order window `W` independent
`son[pair]` loads to overlap, instead of one dependent chain of ~5.5 loads
per query (~85-100 ns each, the measured 46 ns/step DRAM latency).

Simple prefetch hints did not help because they do not create independent
*dependent* chains; interleaving whole descents in one instruction stream
does, because each slot's next load is independent of the others'.

### Shape

`collect_block_matches` currently runs, per position: `seed_for(pos)` then
`matches_seeded(pos, ...)` then emits. Replace the serial loop with a small
window scheduler:

- one slot per in-flight position holding the resumable descent state
  (`current`, `(child_less, child_greater)`, `ptr0`/`ptr1`, `len0`/`len1`,
  `budget`, `out`, and `depends_on` = the previous in-flight position with
  the same hash);
- a slot may start once its `depends_on` has completed (same-bucket order);
  otherwise it starts immediately;
- each outer iteration advances every runnable slot by exactly one node, so
  the compiler emits the loads back to back and the OoO engine overlaps
  them; results are written back in position order.

The existing `seed_for` / `matches_seeded` first-step pipeline is the depth-1
special case and stays.

### Cost / risk

- **Complexity**: `TreeMatchFinder::descent` must be split into "load the
  next pair" and "consume the pair" without changing a single decision
  (floor/budget/far-band guards, the child choice, attachment-point
  rewiring).
- **Memory**: `W` slot states of a few tens of bytes — negligible;
  `out` vectors are per position either way.
- **Ratio**: byte-identical by construction (same tree, same per-bucket
  order). The `matchless_fast_path_is_byte_identical` unit test and the
  solid/MT byte comparisons are the gate.
- **Independence**: consecutive positions collide in a bucket with
  probability ~2^-20, so ~W independent descents are available except in
  pathological repetitive data (already covered by the fast paths).

### Validation plan

1. Prototype behind `RAR_RS_PIPE_DEPTH` (env, default 0 = current serial
   path), no default behavior change;
2. byte-identity: `cargo test --lib` plus decode-and-diff on the A/B corpora
   (tsc.exe prefix, ntoskrnl, text64, xml, random);
3. speed: `collectbench` / `perfbench` A/B, 20 interleaved samples, medians;
   success bar >= 15% collect on the dense DLL with identical bytes;
4. only then wire it into the default path and decide the MT interaction
   (MT slices reset the tree per slice, so the same collector applies
   inside a slice).
