# rar-rs

Pure-Rust RAR archive library and tools. Creates, reads, and extracts RAR5
archives with native LZSS+Huffman compression, and reads/extracts RAR4
archives — no external binaries required.

**License:** BSD-2-Clause — see [NOTICE](NOTICE) for legal details.

---

## Features

| Feature                              | Status |
|--------------------------------------|--------|
| **RAR5 (v5.0)**                      |        |
| Create RAR5 archives                 |   done |
| Extract RAR5 archives                |   done |
| Native LZSS+Huffman compression      |   done |
| Compression levels 0-5               |   done |
| CRC32 integrity verification         |   done |
| Directory entries                    |   done |
| Timestamp preservation               |   done |
| Solid archive decompression          |   done |
| Solid archive creation               |   done |
| Delete members without full rebuild  |   done |
| Rename members (`rn`)                |   done |
| Append members to existing archives  |   done |
| Update members (newer only, `u`)     |   done |
| Freshen members (existing only, `f`) |   done |
| Move files into archive (`m`)        |   done |
| Find string in members (`i`)         |   done |
| Lock archives (`k`)                  |   done |
| Add recovery record (`rr`)           |   done |
| Repair with recovery record (`r`)    |   done |
| Rebuild missing volumes (`rc`)       |   done |
| Archive comments (`c`/`cw`)          |   done |
| SFX archives (read + `s`/`s-`)       |   done |
| Symlinks/hardlinks (redirects)       |   done |
| RAR7 wide vints / `-si` streams      |   done |
| Multi-volume member deletion/rename  |   done |
| Parallel rewrite pipeline            |   done |
| Quick-open records (`-qo+`)          |   done |
| BLAKE2sp hash records (`-htb`)       |   done |
| Streamed extraction (bounded memory) |   done |
| File-level AES-256 encryption        |   done |
| File-level AES-256 decryption        |   done |
| Header-encrypted archive decryption  |   done |
| Encrypted-data integrity (hash-key MAC) | done |
| Multi-volume archive reading         |   done |
| Multi-volume archive creation        |   done |
| Recovery volumes (`.rev`, WinRAR-compatible) | done |
| **RAR4 (v1.5–v3.x)**                |        |
| Extract RAR4 archives               |   done |
| LZSS+Huffman decompression (m3)      |   done |
| VM filters (E8, E8E9, Delta, RGB, Audio) | done |
| Unicode filename support             |   done |
| Large file support (>2 GB)           |   done |

RAR5 archives produced by rar-rs are fully interoperable with WinRAR and unrar.
RAR4 archives created by other tools (WinRAR, 7-Zip, etc.) can be listed,
tested, and extracted.

---

## CLI Tools

### rar

```
rar a [-m0..-m5] [-p<password>] [-v<size>] archive.rar files...   Create archive
rar d [-p<password>] archive.rar names...   Delete members without rebuilding
rar l archive.rar                          List contents
rar i archive.rar                          Show info
```

The `-v` flag creates multi-volume archives (e.g. `-v1m` for 1 MB volumes, `-v100k` for 100 KB).

`rar d` removes members without recompressing the rest: kept file blocks
(header + compressed payload) are copied byte-for-byte, so the operation
scales with the archive size — not with the remaining data. Solid archives
recompress only the chain affected by the deletion; inline recovery
records are dropped and the quick-open record is rebuilt, matching the
official `rar d`.

### unrar

```
unrar x [-p<password>] archive.rar [dest/]    Extract with full paths
unrar e [-p<password>] archive.rar [dest/]    Extract flat
unrar l [-p<password>] archive.rar            List contents
unrar t [-p<password>] archive.rar            Test integrity
unrar p [-p<password>] archive.rar [file]     Print to stdout
```

---

## Library Usage

```rust
use rar5::RarArchive;

// Create
let mut rar = RarArchive::create("backup.rar")?;
rar.add("src/", 3)?;
rar.add_bytes("notes.txt", b"Some notes", 3)?;
rar.close()?;

// Extract
let mut rar = RarArchive::open("backup.rar")?;
rar.extract_all("/tmp/output/")?;

// Read a single file
let mut rar = RarArchive::open("backup.rar")?;
let data = rar.read("notes.txt")?;

// Create an encrypted archive
let mut rar = RarArchive::create_with_password("secret.rar", "mypassword")?;
rar.add("classified.txt", 3)?;
rar.close()?;

// Open an encrypted archive
let mut rar = RarArchive::open_with_password("secret.rar", "mypassword")?;
let data = rar.read("classified.txt")?;

// Create a multi-volume archive (1 MB per volume)
let mut rar = RarArchive::create_multivolume("backup.rar", 1048576)?;
rar.add("large_file.bin", 3)?;
rar.close()?;

// Open a multi-volume archive (auto-discovers all volumes)
let mut rar = RarArchive::open("backup.part1.rar")?;
rar.extract_all("/tmp/output/")?;
```

### Advanced options

`RarArchive::create_with_options` is the full-featured constructor; the
dedicated `create*` constructors are thin wrappers around it:

```rust
use rar5::{CreateOptions, RarArchive};

// Solid + quick-open + BLAKE2sp + password + recovery record.
let mut rar = RarArchive::create_with_options(
    "backup.rar",
    CreateOptions {
        solid: true,
        quick_open: true,
        blake2: true,
        password: Some("secret".into()),
        recovery_percent: Some(10),
        ..Default::default()
    },
)?;
rar.add("src/", 3)?;
rar.close()?;
```

Solid archives share one LZ window across consecutive compressed members
(better ratio; single-volume only for now). Quick-open records copy every
file header into one block at the end of the archive (`-qo+` semantics),
which WinRAR uses for fast listing. BLAKE2sp hash records match WinRAR's
`-htb` and are verified on read (also for archives created by other tools).

### Safe extraction

Extraction is safe by default: member names are sanitized (path traversal,
absolute paths, drive components and NUL bytes are rejected), resolved
paths are checked to stay inside the destination, per-file and total output
sizes are capped, and files are written to a temporary sibling and renamed
only after integrity checks pass. Encrypted members verify their MAC'd
checksums, so corrupted ciphertext is always detected.

```rust
use rar5::{ExtractOptions, RarArchive};

let opts = ExtractOptions {
    max_unpacked_bytes: Some(4 * 1024 * 1024 * 1024), // 4 GiB per file
    max_total_unpacked_bytes: Some(32 * 1024 * 1024 * 1024), // 32 GiB total
    ..Default::default()
};
let mut rar = RarArchive::open("backup.rar")?;
rar.extract_all_with_options("/tmp/output/", opts)?;
```

Relax these defaults only for trusted archives.

### Streaming and memory

Large members are processed in bounded chunks: STORE members stream from
disk, compressed members use a 4 MiB chunked encoder with a shared LZ
window, and extraction streams decoded output to the destination instead
of materializing whole files. The worst-case memory for a compressed
member is roughly the packed size plus one chunk, instead of a symbol
table proportional to the whole file.

---

## Module Layout

```
src/
+-- lib.rs              Public API
+-- archive.rs          RarArchive high-level interface
+-- headers.rs          Block/header structs
+-- compression.rs      Compress/decompress dispatch
+-- encryption.rs       AES-256-CBC + PBKDF2 key derivation
+-- constants.rs        RAR5 format constants
+-- vint.rs             Variable-length integer codec
+-- error.rs            Error types
+-- codec/              RAR5 compression codec
|   +-- mod.rs          Codec public API
|   +-- decoder.rs      Block decoder + symbol stream
|   +-- encoder.rs      Block encoder + match finder
|   +-- bitstream.rs    MSB-first bit reader/writer
|   +-- huffman.rs      Canonical Huffman tables
|   +-- window.rs       Sliding window buffer
|   +-- filters.rs      Delta, E8, E8E9, ARM filters
|   +-- lz_match.rs     Hash-chain match finder
|   +-- tables.rs       Symbol/table constants
+-- rar4/               RAR4 read/extract support
|   +-- mod.rs          Module root
|   +-- constants.rs    RAR4 header types and flags
|   +-- headers.rs      RAR4 header parsing
|   +-- decoder.rs      LZSS+Huffman decompressor + VM filters
+-- bin/
    +-- rar.rs          CLI archive creator
    +-- unrar.rs        CLI archive extractor
```

---

## Building

```bash
cargo build --release
```

Binaries are at `target/release/rar` and `target/release/unrar`.

## Interop testing

The official RAR/UNRAR 7.x binaries are used as black-box references:
UNRAR tests every feature combination we produce (solid, quick-open,
BLAKE2sp, encryption, recovery records, `.rev` volumes), we read official
archives byte-identically, `rar r` repairs our recovery records,
`rar rc` reconstructs deleted volumes from our `.rev` files, and every
modification command is cross-validated: deleted archives (plain, solid,
encrypted, header-encrypted, with quick-open, multi-volume), appended
archives and locked archives are tested by UNRAR, while `rar d`, `rar a`
and `rar k` on rar-rs archives must stay readable.

```bash
SA_OFFICIAL_RAR=/path/to/rar SA_OFFICIAL_UNRAR=/path/to/unrar \
  cargo test --release --test interop official_
```

The tests skip automatically when these variables are not set.

## Known limitations

- RAR4 PPMd and RAR4 encryption are not implemented (read-only LZSS +
  filters, no compression).
- Solid archives cannot be combined with multi-volume output yet.
- Encrypted STORE members are buffered (CBC padding is applied to the
  whole member); encrypted compressed members only buffer the packed
  output.
- Inline recovery records buffer the archive prefix (max 32 GiB); `.rev`
  generation streams.
- Quick-open stores headers for every file (WinRAR's default only caches
  large files); dictionaries are accepted up to 1 GiB (WinRAR 5.x max).
- Appending to multi-volume archives is not supported (the official `rar`
  refuses too); deleting from them is supported.
- Solid archives cannot be combined with multi-volume output yet.

---

## Legal

This is a clean-room implementation for software conservancy and educational
purposes. See [NOTICE](NOTICE) for the full legal notice, including trademark
attribution and scope limitations. Licensed under BSD-2-Clause — see
[LICENSE](LICENSE).
