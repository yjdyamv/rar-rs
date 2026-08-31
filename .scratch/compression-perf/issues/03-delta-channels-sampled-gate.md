# 03 — delta filter candidate channels + sampled pre-gate

Type: task
Status: resolved

## Question

`picks_correct_channel_count_for_interleaved_streams` failed (broken since
be7a254, gated behind the simd feature so CI never ran it). Diagnose and fix.

## Answer

Two real defects, not just a wrong test:

1. **Candidate channels [1,2,3,4] missed multi-byte frame sizes.** An
   interleaved little-endian frame packs best with one delta lane per byte
   position, i.e. channels = bytes × channels: 16-bit stereo → 4, 32-bit
   stereo → 8, 24-bit 3-channel → 9, 32-bit 4-channel → 16. Extended
   `AUTO_DELTA_CHANNELS` to [1,2,3,4,6,8,9,12,16]. Measured: 32-bit stereo
   11% (was ~18% capped at ch4), 24-bit 3ch 14%, 16-bit 4ch 22% vs 84% plain.
2. **Pre-gate scanned the whole member** (hundreds of ms on 64 MiB members)
   to protect a 64 KiB sample encode → now samples the head 64 KiB. Its
   accept decision followed the min-mag channel's near-zero ratio, which
   0/255 byte wrapping fools into a coarser lane (an 8-bit wrapping walk was
   rejected) → now follows the best near-zero ratio across candidates.

The size-based selection was extracted to `pick_delta_channel` and tested
directly: `delta_selection_prefers_frame_size` checks all nine layouts pick
their frame size; the pre-gate test asserts Some/None only (its channel
return is a hint, never consumed). Roundtrip test extended to 2/3/4-byte
samples; text-rejection asserted.

Context: 45fa1e0. All 120 lib tests green; clippy clean on parallel + simd.
