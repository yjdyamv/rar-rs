# 05 — streaming-path auto filters for large members

Type: research
Status: open

## Question

The auto delta/x86 filters only run on the in-memory path
(`encode_with_auto_delta_filter` in `add_file`, members < 64 MiB). Larger
members go through the spill/streaming window path (`flush_window` →
`encode_chunked_mt` / `compress_chunked`) with no filter attempt. A 100 MB
WAV/PCM disk image compresses far worse than WinRAR would.

## Analysis

- Filter regions are member-relative; the streaming writer processes windows
  of up to 64 MiB. Applying a delta filter requires transforming the member
  before LZSS — either per-window transforms with member-relative region
  bookkeeping, or a two-pass approach (scan + transform + compress).
- The x86 filter needs the whole member to find E8/E8E9 clusters; delta only
  needs the channel layout (cheap per window after the first).
- WinRAR applies filters per-file; parity for large files is a ratio gap
  (not a correctness one — output remains valid).

## Open questions

- What is the exact >64 MiB threshold and is it per-member or per-archive?
- Can delta be applied per-window with the first window deciding channels,
  keeping regions member-relative?
- Does the x86 filter matter for large binaries (>64 MiB DLLs/EXEs are rare;
  disk images are the common large case and delta is the right filter)?
