# 02 — collector fast mode gating `longest==0` + thresholds 256

Type: task
Status: resolved

## Question

The collector's fast mode (skip tree/LR searches on incompressible runs)
needs a miss threshold and a miss definition that engage early on random data
without degrading compressible data.

## Answer

Thresholds 4096 → 256 for both the tree and long-range probes, AND the miss
definition changed from `longest < 16` to `longest == 0`.

The `< 16` definition was the trap: on text, 4-15 byte matches (word
prefixes) are real signal, and gating on them let fast mode starve the
recovery searches of candidates — text ratio regressed 15.32% → 15.61% (the
previous session's threshold change shipped with that regression; this ticket
fixed it). With `== 0`, text keeps full-cadence search (5,563,944 searches =
identical to baseline, ratio byte-identical on all corpora) while random data
engages fast mode after 256 real misses (a 4-byte hash-collision match is
~2^-32 per position, so it never blocks the mode).

Verified: 64 MiB random m3 mt8 1253 → 486 ms; text/mixed/xml/sparse ratios
unchanged or better (sparse 91.18 → 91.17); all outputs decode identically.

Context: 3cd6b37.
