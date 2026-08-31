# rar-rs

**Pure-Rust RAR5 / RAR7 archive library and command-line tools.** Create,
read, extract, and modify RAR archives with native LZSS+Huffman
compression — no external RAR/UNRAR binaries required.

> Licensed under BSD-2-Clause. This is a clean-room implementation for
> software conservancy and education — see [NOTICE](NOTICE) for legal
> details and trademark attribution.

---

## Why rar-rs

- **Pure Rust, zero external dependencies** for the core library and tools.
- **Full RAR5 (v50) + RAR7 (v70)** read/write, byte-for-byte interoperable
  with WinRAR 7.x and UnRAR.
- **Modify in place** — delete or rename members without recompressing the
  whole archive; append, update, freshen, move, lock, repair.
- **Recovery** — inline recovery records and `.rev` recovery volumes, with
  streaming repair that holds only the recovery data in memory.
- **Safe by default** — path sanitization, size caps, atomic staging, and
  AES-256 encryption (file-level and header-level with a hash-key MAC).
- **Multi-volume** archives, **BLAKE2sp** hashes, **quick-open** fast
  listing, multi-threaded compression, and a Node/WASI binding.

## Build

```bash
cargo build --release
# Binaries: target/release/rar and target/release/unrar
```

## Quick start

### Command line

```bash
# Create an archive (-m0..-m5; -ma5 default, -ma7 forces RAR7)
rar a -m5 backup.rar src/ notes.txt

# List / extract
rar l backup.rar
unrar x --dest out/ backup.rar

# Delete a member without rebuilding
rar d backup.rar old.log

# Encrypt (file-level, or -hp for header encryption)
rar a -pSecret secret.rar docs/
```

All official commands are implemented: `a c ch cw d e f i k l[t][b] m p r
rc rn rr rv s s- t u v[t][b] x`. Full reference in
[docs/CLI.md](docs/CLI.md).

### Library

```rust
use rar5::RarArchive;

// Create
let mut rar = RarArchive::create_with_options("backup.rar", Default::default())?;
rar.add("src/", 3)?;
rar.add_bytes("notes.txt", b"Some notes", 3)?;
rar.close()?;

// Extract
let mut rar = RarArchive::open("backup.rar")?;
rar.extract_all("/tmp/output/")?;

// Read a single member
let mut rar = RarArchive::open("backup.rar")?;
let data = rar.read("notes.txt")?;
```

The crate is `rar5`; see `crates/rar` for the full API and
[docs/ARCHITECTURE.html](docs/ARCHITECTURE.html) for advanced usage
(solid/quick-open/BLAKE2sp, safe extraction, `open_quick`, cancellation,
streaming repair, multi-volume).

## Feature highlights

- **Formats:** RAR5 (v50) and RAR7 (v70) create + extract. **RAR4 is
  explicitly unsupported** (rejected with a clear error — use 7-Zip).
- **Compression:** native LZSS + Huffman, levels 0–5, dictionaries up to
  4 GiB (RAR5) / beyond (RAR7), multi-threaded (`-mt`).
- **Integrity:** CRC32, BLAKE2sp (`-htb`), and encrypted-data MAC.
- **Operations:** create, append, delete (no rebuild), rename, update (`u`),
  freshen (`f`), move (`m`), lock (`k`), comment (`c`/`cw`), SFX (`s`/`s-`),
  string search (`i`), symlinks/hardlinks.
- **Solid archives, quick-open (`-qo+`), and multi-volume** create / read /
  modify.
- **Recovery:** inline recovery record (`rr`/`r`) and recovery volumes
  (`.rev`, `rv`/`rc`).
- **Encryption:** file-level AES-256 (CBC + chained HMAC-SHA256 KDF) and
  header-level (`-hp`).
- **Streaming, bounded-memory** extraction with cooperative cancellation.

The complete feature matrix lives in
[docs/ARCHITECTURE.html](docs/ARCHITECTURE.html).

## Limitations

RAR4 is not supported (use 7-Zip), and appending to multi-volume archives is not
supported either (the official `rar` refuses too). Other notes: inline recovery
records stream on repair; `-hp` multi-volume sets can't use inline RR (only `.rev`);
solid and multithreaded compression are mutually exclusive; filter types 4–7 are
rejected; KDF strength is capped at 2²⁴ iterations (default 2¹⁵).

## Documentation

- **Format reference** — [visual diagram (HTML)](docs/FORMAT_RAR5_RAR7.html)
- **Architecture & module layout** — [docs/ARCHITECTURE.html](docs/ARCHITECTURE.html)
- **CLI reference** — [docs/CLI.md](docs/CLI.md)

## Legal

Clean-room implementation for software conservancy and educational purposes.
See [NOTICE](NOTICE) for the full legal notice. Licensed under
BSD-2-Clause — see [LICENSE](LICENSE).
