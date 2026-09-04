# Plan: RAR29 LZSS Encoder Port (m1-m5)

## Goal

Port the `Unpack29Encoder` from the rars project into rar-rs, enabling RAR4 creation with LZSS+Huffman compression at levels m1–m5. Phase 1: LZ only (no PPMd, no auto-filter search).

## Scope

- Create `crates/rar/src/codec/rar29_encoder.rs` — self-contained encoder module
- Wire it into the RAR4 write pipeline (`rar50/write/mod.rs::add_file_rar4`)
- Level parameter flows through `add_file` → `add_file_rar4` → encoder → STORE fallback
- Roundtrip test: create RAR4 compressed → extract → diff

## Out of scope (Phase 2)

- PPMd encoder (requires porting `PpmdEncoder` + `RangeEncoder` from rars)
- Auto-filter search (E8/E8E9/Delta/Audio/RGB filter detection + trial encoding)
- Solid chain encoding (multi-member solid — needs `encode_member_with_engine`)
- Encryption (-p on compressed members)

---

## Step 1: Create `codec/rar29_encoder.rs`

Self-contained file, mirroring rars `codec/rar29.rs` encoder half. No PPMd.

### Constants (port from rars lines 9–76)

```rust
const MAIN_COUNT: usize = 299;
const OFFSET_COUNT: usize = 60;
const LOW_OFFSET_COUNT: usize = 17;
const LENGTH_COUNT: usize = 28;
const LEVEL_COUNT: usize = 20;
const TABLE_COUNT: usize = MAIN_COUNT + OFFSET_COUNT + LOW_OFFSET_COUNT + LENGTH_COUNT;
const MAX_HISTORY: usize = 4 * 1024 * 1024;
const MAX_ENCODER_MATCH_OFFSET: usize = 1024 * 1024;
const MAX_ENCODER_MATCH_LENGTH: usize = 258;
const MAX_MATCH_CANDIDATES: usize = 256;
const RAR29_LZ_BLOCK_SIZE: usize = 64 * 1024;
```

Plus the shared lookup tables: `LENGTH_BASES`, `LENGTH_BITS`, `OFFSET_BASES`, `OFFSET_BITS`, `SHORT_BASES`, `SHORT_BITS` (identical to rars and to the existing decoder tables in `rar29.rs`).

### BitWriter (port from rars lines 3400–3444)

Self-contained MSB-first bit packer:
- `struct BitWriter { bytes: Vec<u8>, bit_pos: usize }`
- `write_bits(value, count)` — MSB-first
- `write_bit(bit)` — single bit
- `write_encoded_u32(value)` — 2-bit prefix selector (0:4bit, 1:8bit, 2:16bit, 3:32bit)
- `finish(self) -> Vec<u8>`

### Match finder adapter

Port rars' `MatchFinder<4>` as a private struct `Rar29MatchFinder` inside `rar29_encoder.rs`:
- Owned `head: Vec<u32>` + `prev: Vec<u32>` + `mask` + `newest`
- Same hash (4-byte LE × 0x9E3779B1, 17-bit), same `resolve()`, same `first()`/`previous()`/`insert()` API
- This is ~80 lines; keeps the encoder self-contained like rars does

### match_length (port from rars `fast.rs`)

Private `fn match_length(input, pos, distance, max_length) -> usize`:
- 32-byte chunks (4 × u64 XOR + trailing_zeros), 8-byte chunks, byte-by-byte tail
- ~40 lines

### Encoder types (port from rars lines 1453–1637)

```rust
enum EncodeToken { Literal(u8), Match { length, offset } }

struct EncoderMatchState { old_offsets: [usize; 4], last_offset, last_length }
impl EncoderMatchState {
    fn encode_match(&self, length, offset) -> Result<EncodedMatch>
    fn remember(&mut self, length, offset)
}

enum EncodedMatch {
    LastLengthRepeat,
    RepeatOffset { index, length_slot, length_extra },
    Fresh { length_slot, length_extra, offset_slot, offset_extra },
}

struct MatchCandidate { length, offset, score }
```

### EncodeOptions (port from rars lines 497–543)

```rust
pub struct EncodeOptions {
    pub max_match_candidates: usize,
    pub lazy_matching: bool,
    pub lazy_lookahead: usize,
    pub max_match_distance: usize,
    pub block_size: Option<usize>,
}
```

Plus level-to-options mapping:
```rust
pub fn options_for_level(level: u8) -> EncodeOptions
// m1: candidates=8,  lazy=false
// m2: candidates=32, lazy=false
// m3: candidates=64, lazy=false   (default)
// m4: candidates=96, lazy=true
// m5: candidates=128, lazy=true
// All: block_size = Some(RAR29_LZ_BLOCK_SIZE)
```

### Core encoding functions (port from rars lines 830–1119)

1. `fn encode_tokens_with_progress(input, history, options, progress) -> Result<Vec<EncodeToken>>`
   - Builds `combined = history + input`, creates `Rar29MatchFinder`, inserts history positions
   - Main loop: `best_match()` → `lazy_match_decision()` → emit Literal or Match
   - Reports progress every 1 MiB

2. `fn best_match(input, pos, end, finder, options, state) -> Option<MatchCandidate>`
   - Check 4 `old_offsets` first (cheap repeat candidate)
   - Walk hash chain up to `max_match_candidates`
   - Score = `length * 8 - estimated_match_cost`

3. `fn lazy_match_decision(input, pos, finder, options, state, current) -> (bool, Option<MatchCandidate>)`

4. `fn encode_member_inner(input, history, options, more_blocks_follow, previous_levels, progress) -> Result<Vec<u8>>`
   - Tokenize → count frequencies → build Huffman lengths → build canonical codes
   - Serialize: LZ block bit + keep-tables bit + 20×4-bit level lengths + level tokens + tokens with Huffman codes + end-of-block
   - Block terminator: `more_blocks_follow ? true : (false, true)`

### Level table encoding (port from rars lines 1997–2217)

- `LevelToken` struct + constructors
- `encode_table_level_tokens(lengths)` — outright encoding
- `encode_level_tokens_against(lengths, base)` — delta encoding
- `level_tokens_bit_cost(tokens)` — compare two encodings
- `level_code_lengths(tokens)` — Huffman over level alphabet
- `canonical_codes(lengths) -> Vec<Option<HuffmanCode>>`
- `emit_repeat_level_run()`, `emit_zero_level_run()`
- Slot finders: `length_slot_for_match()`, `length_slot_for_repeat_match()`, `offset_slot_for_match()`

### Public API

```rust
pub struct Unpack29Encoder {
    history: Vec<u8>,
    options: EncodeOptions,
    levels: [u8; TABLE_COUNT],
}

impl Unpack29Encoder {
    pub fn new() -> Self
    pub fn with_options(options: EncodeOptions) -> Self
    pub fn encode_member(&mut self, input: &[u8]) -> RarResult<Vec<u8>>
    fn remember(&mut self, input: &[u8])
}
```

### Shared infrastructure reused from rar-rs

- `crate::codec::huffman::build_code_lengths_from_freqs` — Huffman code-length builder (used for the 4 main/offset/low_offset/length tables; NOT for the level alphabet which needs the rars-specific `lengths_for_frequency_array` variant — port that inline)
- `crate::crc32::crc32` — for CRC computation (already used throughout)
- `crate::error::{RarError, RarResult}` — error types

### Error mapping

```rust
fn encoder_error(message: &'static str) -> RarError {
    RarError::Format(format!("RAR 2.9 encoder: {message}"))
}
```

---

## Step 2: Wire encoder into write pipeline

### `rar50/write/mod.rs` changes

1. **`add_file_rar4`** — add `level: u8` parameter:
   ```rust
   fn add_file_rar4(&mut self, path: &Path, arcname: Option<&str>, level: u8) -> RarResult<()>
   ```

2. **In `add_file_rar4`** — after reading file data, before `emit_segment()`:
   ```rust
   let (packed_data, method) = if level >= 1 && level <= 5 {
       let options = options_for_level(level);
       let mut encoder = Unpack29Encoder::with_options(options);
       let compressed = encoder.encode_member(&data)?;
       if compressed.len() < data.len() {
           (compressed, RAR4_METHOD_STORE + level)
       } else {
           (data.clone(), RAR4_METHOD_STORE)
       }
   } else {
       (data.clone(), RAR4_METHOD_STORE)
   };
   ```
   Then use `packed_data` and `method` in `emit_segment()`.

3. **`add_file`** — pass `level` through to `add_file_rar4`:
   ```rust
   if self.rar4 {
       return self.add_file_rar4(path, arcname, level);
   }
   ```

4. **CRC32** — computed on the **original uncompressed data** (as WinRAR does; the header stores the original CRC, not the compressed CRC).

5. **Dictionary size** — pass `0x40_0000` (4 MiB) for compressed members, keep existing for STORE.

### Import in `rar50/write/mod.rs`

```rust
use crate::codec::rar29_encoder::{Unpack29Encoder, options_for_level};
```

### `codec/mod.rs` — add module declaration

```rust
pub(crate) mod rar29_encoder;
```

---

## Step 3: Roundtrip test

Add a test in `crates/rar/tests/` that:
1. Creates a RAR4 archive with `-m3` (or equivalent) containing known files
2. Extracts with `unrar` (or the existing rar-rs reader)
3. Diffs extracted content against originals
4. Tests multiple content types (text, binary, small, large)

---

## Step 4: Verification

- `cargo fmt --all`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test` — all existing tests pass
- New roundtrip test passes
- Manual: create RAR4 with `rar-cli`, extract with WinRAR/unrar, verify byte-identical

---

## Key design decisions

1. **Self-contained module** — the encoder lives in its own file with its own BitWriter and match finder, like rars does. This avoids polluting the shared codec layer with RAR4-specific types.

2. **No PPMd in Phase 1** — the `Unpack29Encoder` struct has no `ppmd` field. Phase 2 adds `ppmd: Option<PpmdDecoder>` and `encode_member_with_engine()`.

3. **No filter search in Phase 1** — plain LZSS only. Filter search (E8/E8E9 etc.) is a separate concern that can be added later by calling `encoder.encode_member_with_filter()`.

4. **STORE fallback** — if compressed output ≥ input size, fall back to STORE (method 0x30). This is what rars does at `encode_rar29_lz_member()`.

5. **Reuse existing Huffman builder** — `build_code_lengths_from_freqs` from `codec/huffman.rs` handles the main/offset/low_offset/length tables. The level alphabet uses a small inline helper (`lengths_for_frequency_array`) ported from rars.

6. **History window** — the encoder maintains `history: Vec<u8>` capped at `MAX_HISTORY` (4 MiB), matching the decoder's window semantics. For Phase 1 (non-solid), each member starts fresh.
