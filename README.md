# rar-rs

> Last verified: 2026-09-22 @ `b4b8e04`; implementation details are
> authoritative in the source.

**Pure-Rust RAR archive library and command-line tools.** Create, read, extract,
and modify RAR5/RAR7 archives, read legacy RAR 1.3–4.x archives, and create RAR
1.3 / 1.4 / 1.5 / 2.x / 4.x archives with native Rust codecs. No external
RAR/UNRAR binary is required at runtime.

## Features

- **Formats** — RAR5 (v50) and RAR7 (v70) create / read / write; RAR 1.3–4.x
  read / extract; RAR 1.3 / 1.4 / 1.5 / 2.x / 4.x creation (`-ma13` / `-ma14` /
  `-ma15` / `-ma2` / `-ma4`), including legacy codecs, solid chains and volumes.
- **Compression** — native LZSS+Huffman and PPMd, levels 0–5, dictionary
  controls, delta/x86 filters, solid archives, parallel compression.
- **Archive operations** — append, update, delete, rename, freshen, move, lock,
  comments, SFX handling, string search, symlinks and hardlinks.
- **Encryption** — file-level AES-256 with a chained HMAC-SHA256 KDF, plus
  header-level encryption (`-hp`).
- **Recovery** — inline recovery records and `.rev` recovery volumes (RAR5 REV5
  and legacy RAR 1.5–4.x), with bounded-memory repair paths. When an archive has
  no record at all, `rar r` reconstructs `rebuilt.<name>` from the members that
  still decode and verify, resyncing past corrupt RAR5/RAR4 headers (plaintext
  headers) and exiting 3 (RAR5) or 0 (legacy) when a header was lost, like
  WinRAR.
- **Integrity** — CRC32, BLAKE2sp (`-htb`) and encrypted-data MACs.
- **Safe extraction** — path sanitization, size limits (opt-in `--max-unpacked`
  / `--max-total-unpacked`), atomic staging, cooperative cancellation.
- **Bindings** — Node.js native and WASI bindings under `crates/rar-napi`.

Behavior is validated against WinRAR/UnRAR; current status and accepted
divergences live in [PLAN.md](PLAN.md).

## Build

Current stable Rust (`edition = "2024"`); no fixed MSRV is declared and CI runs
the full stable matrix.

```bash
cargo build --release --locked
# Binaries: target/release/rar and target/release/unrar
```

On Windows the binding crate (`crates/rar-napi`) needs an **MSVC** target — Node
is MSVC-built, and `napi-build`'s `windows-gnu` path wants a `libnode.dll` that
no Node distribution ships — so a host whose rustup default is GNU should build
it as `cargo build -p rar-rs-napi --target x86_64-pc-windows-msvc`, or produce
the `.node` with `npx napi build --platform` (see
[docs/testing.md](docs/testing.md)).

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

Full command and switch reference: [docs/CLI.md](docs/CLI.md).

### Library

```rust
use rar_rs::{ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions};

// Create — `finish()` consumes the writer and commits the archive
let mut writer = ArchiveWriter::create("backup.rar")?;
let opts = EntryWriteOptions::new().compression_level(CompressionLevel::NORMAL);
writer.add_path("src/", opts)?;
writer.add_bytes("notes.txt", b"Some notes", opts)?;
writer.finish()?;

// Extract
let mut reader = ArchiveReader::open("backup.rar")?;
reader.extract_all("/tmp/output/")?;

// Read a single member
let mut reader = ArchiveReader::open("backup.rar")?;
let id = reader.unique_entry("notes.txt")?;
let data = reader.read_entry(id)?;
```

Per-archive and per-run tuning follows the same shape: `WriterOptions::threads`
for the writer, `ExtractOptions::threads` for one extraction (both fall back to
`set_compression_threads` / `set_extraction_threads`), `set_cancel_flag` for
cancellation and `set_progress_callback` for progress.

Runnable examples: `cargo run --example create_and_extract` (the ordinary create
/ list / read / extract / verify flow) and `cargo run --example edit_and_repair`
(editing, a recovery record, then repair and rebuild after damage).

Module map and design invariants: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Limitations

- **RAR4 editing** repacks solid archives whole (`d` / `u` / `f` / `a`) and
  surgically rewrites non-solid ones
  ([ADR 0005](docs/adr/0005-rar4-edit-architecture.md)). Multi-volume `d` / `a`
  and appending to any multi-volume set are refused, as the official `rar` does.
- **RAR5 header-encrypted (`-hp`) sets**: rename, `ch`, archive comments and
  delete (single- and multi-volume) all work — rewritten headers are
  re-encrypted and the comment rides as a plaintext data area. `-hp` still
  cannot be combined with inline recovery records (use `.rev` volumes), and
  `-k`/lock stays refused until the encrypted main-header patch can grow.
- **RAR4 solid chains** stay sequential; only RAR5 gets chunk-level MT.
- Filter types 4–7 are rejected; KDF strength is capped at 2²⁴ iterations
  (default 2¹⁵).

The complete list of deliberate refusals and known interop differences is in
[PLAN.md](PLAN.md)「一致拒绝」「已知小差异」.

## Documentation

Index: **[docs/README.md](docs/README.md)**. Reading order:

1. This file — build and quick start.
2. [docs/CLI.md](docs/CLI.md) — commands and switches.
3. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — module map and design notes.
4. [CONTEXT.md](CONTEXT.md) — domain vocabulary.
5. [PLAN.md](PLAN.md) — status, next steps, limitations.
6. [docs/FORMAT_RAR5_RAR7.html](docs/FORMAT_RAR5_RAR7.html) — byte-level format
   reference.

## Legal

Independent implementation for software conservancy and education, with
separately identified upstream portions. The `license` field (`Cargo.toml`
`[workspace.package]`, inherited by all three crates and mirrored in
`crates/rar-napi/package.json`) stays `BSD-2-Clause`: it states the project's
**own** contributions. Portions derived from other projects keep their own terms
instead of being folded into it — the `rars`-derived files are
`MIT OR Apache-2.0`, the libarchive-derived ones BSD-2-Clause — so a
redistributor has to satisfy both sets. [NOTICE](NOTICE) and
[THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) record which files came from
where; [`LICENSES/`](LICENSES/) holds the standard texts. Full text:
[LICENSE](LICENSE).

All Markdown is formatted with `dprint` at 80 columns; conventions in
[docs/README.md](docs/README.md).
