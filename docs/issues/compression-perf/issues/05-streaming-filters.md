# 05 — streaming-path auto filters for large members

Type: research
Status: done (2026-09-07)

## Question

The auto delta/x86 filters only run on the in-memory path
(`encode_with_auto_delta_filter` in `add_file`, members < 64 MiB). Larger
members go through the spill/streaming window path (`flush_window` →
`encode_chunked_mt` / `compress_chunked`) with no filter attempt. A 100 MB
WAV/PCM disk image compresses far worse than WinRAR would.

## Resolution (implemented)

Streaming members (>= `STREAM_COMPRESS_THRESHOLD`, 64 MiB) now get the
automatic **delta** filter, decided on the leading 64 KiB sample (the same
head sample the in-memory path packs). The x86 filter stays memory-path-only:
finding E8/E8E9 clusters needs the whole member, and delta is the relevant
filter for the common large case (disk images / PCM).

Mechanics:

- `add_file_streaming` reads a min(64 KiB, file_size) sample, gates on
  `auto_delta_filter_channels`, then `pick_delta_channel` chooses the channel
  count on that sample. Guard: `file_size < u32::MAX` (`FilterSpec.block_start`
  is u32).
- A filtered member is **standalone**: `reset_solid_chain()` before computing
  `chain_solid` (so the broken chain is what the header records) and again
  after the member, because its window holds transformed bytes that must never
  seed the next solid member.
- Each window is forward-transformed in independent regions capped at
  `MAX_FILTER_BLOCK_LENGTH` (262143) aligned on **absolute member
  coordinates** (`delta_stream_window`), reproducing the memory path's
  fresh-lanes-per-piece split exactly.
- Region records lead that window's first emitted block via
  `encode_chunked_raw_with_lead` (sequential path, first chunk only) or
  `encode_chunked_mt_with_progress`'s `lead_symbols` (MT path). Critical
  detail: records are serialized **relative to the window start** — the
  decoder (`parse_filter`) adds its current write position, so absolute
  member-relative values desync every window past the first (found as
  "unapplied RAR5 filter at end of stream", fixed 2026-09-07).
- STORE fallback (`packed_size >= file_size`) still rewrites the member from
  the file, unfiltered.

New code: `codec::modern::lzss_huff::encoder::delta_stream_window`,
`encode_chunked_raw_with_lead` (+ `_inner` split of `encode_chunked_raw`),
`Symbol` / `delta_stream_window` pub(crate) re-exports. Regression test:
`crates/rar/tests/large_paths.rs::large_streamed_delta_filter_roundtrips`
(64 MiB lane-walk in a solid archive, threads=2 → 3 windows, byte-exact
extract-all + hard-compression assertion).

## Analysis

- Filter regions are member-relative; the streaming writer processes windows
  of up to 64 MiB. Applying a delta filter requires transforming the member
  before LZSS — either per-window transforms with member-relative region
  bookkeeping, or a two-pass approach (scan + transform + compress).
- The x86 filter needs the whole member to find E8/E8E9 clusters; delta only
  needs the channel layout (cheap per window after the first).
- WinRAR applies filters per-file; parity for large files is a ratio gap
  (not a correctness one — output remains valid).

## Open questions (remaining)

- Sample-gate is a heuristic: delta is chosen when it wins on the 64 KiB
  sample, not the whole member (a full second read+encode is impractical).
  Worst case: a marginal 64 MiB+ delta miss costs a reset solid chain.
- x86 streaming remains unimplemented (whole-member scan requirement);
  largest gap persists for >64 MiB x86 binaries, flagged as unlikely.
