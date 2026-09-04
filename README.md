# rar-rs

**Pure-Rust RAR archive library and command-line tools.** Create, read,
extract, and modify RAR5/RAR7 archives, read legacy RAR 1.5–4.x archives,
and create RAR4 archives with native Rust codecs. No external RAR/UNRAR
binary is required at runtime.

> Licensed under BSD-2-Clause for original project portions. This is an
> independent implementation with separately identified upstream portions —
> see [NOTICE](NOTICE) and the provenance-oriented
> [third-party source inventory](THIRD_PARTY_LICENSES.md).

---

## Why rar-rs

- **Pure Rust** implementation with no external RAR/UNRAR runtime binary.
- **RAR5 (v50) and RAR7 (v70)** read/write support with WinRAR/UnRAR
  interoperability testing.
- **Legacy support** for reading RAR 1.5–4.x archives and creating RAR4
  archives, including legacy codecs, encryption, solid chains, and volumes.
- **Archive operations** — append, update, delete, rename, freshen, move, lock,
  repair, comments, SFX handling, and multi-volume processing.
- **Recovery** — inline recovery records and `.rev` recovery volumes, with
  bounded-memory repair paths.
- **Safe extraction** — path sanitization, size limits, atomic staging, and
  cooperative cancellation.
- **Compression and integrity** — LZSS+Huffman, PPMd, CRC32, BLAKE2sp,
  encrypted-data MACs, filters, solid archives, and parallel compression.
- **Bindings** — Node.js native and WASI bindings under `crates/rar-napi`.

## Build

The workspace requires Rust 1.88, matching the standard-library APIs used by
the implementation. CI validates the current stable toolchain; release checks
should also keep the declared MSRV buildable as dependencies evolve.

```bash
cargo build --release --locked
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

The CLI implements the documented command set, including archive creation,
listing, extraction, modification, repair, recovery, and SFX operations. See
the full reference in
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
[docs/ARCHITECTURE.html](docs/ARCHITECTURE.html) for advanced usage,
including solid archives, quick-open, BLAKE2sp, safe extraction,
`open_quick`, cancellation, streaming repair, and multi-volume processing.

## Feature highlights

- **Formats:** RAR5 (v50) and RAR7 (v70) create/read/write; RAR 1.5–4.x
  read/extract; and RAR4 archive creation.
- **Compression:** native LZSS+Huffman and PPMd codecs, levels 0–5,
  dictionary controls, filters, solid archives, and parallel compression.
- **Integrity:** CRC32, BLAKE2sp (`-htb`), recovery records, recovery volumes,
  and encrypted-data MACs.
- **Operations:** create, append, delete, rename, update, freshen, move, lock,
  comments, SFX handling, string search, symlinks, and hardlinks.
- **Encryption:** file-level AES-256 with chained HMAC-SHA256 KDF and
  header-level encryption (`-hp`).
- **Streaming and safety:** bounded-memory extraction, path sanitization,
  atomic writes, and cooperative cancellation.

The complete feature matrix lives in
[docs/ARCHITECTURE.html](docs/ARCHITECTURE.html).

## Limitations

Legacy RAR4 creation and extraction have feature-specific limitations.
Appending to multi-volume archives is not supported (the official `rar` refuses
too). Inline recovery records have streaming limitations during repair; encrypted
multi-volume sets cannot combine `-hp` with inline RR and must use `.rev` recovery
volumes. Solid and multithreaded compression are mutually exclusive; filter types
4–7 are rejected; KDF strength is capped at 2²⁴ iterations (default 2¹⁵).

## Documentation

Full index in **[docs/README.md](docs/README.md)**. Highlights:

- **Format reference** — [visual diagram (HTML)](docs/FORMAT_RAR5_RAR7.html) (authoritative)
- **Architecture & module layout** — [docs/ARCHITECTURE.html](docs/ARCHITECTURE.html)
- **CLI reference** — [docs/CLI.md](docs/CLI.md)
- **Domain vocabulary** — [CONTEXT.md](CONTEXT.md)
- **Roadmap / status** — [PLAN.md](PLAN.md)
- **Security policy** — [SECURITY.md](SECURITY.md)
- **Release history** — [CHANGELOG.md](CHANGELOG.md)
- **Code audit baseline** — [docs/CODE_AUDIT_2026-09-05.md](docs/CODE_AUDIT_2026-09-05.md)

## Legal

Independent implementation for software conservancy and educational
purposes, with separately identified upstream portions. See [NOTICE](NOTICE)
for attribution and license boundaries,
[THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) for the source inventory,
and [`LICENSES/`](LICENSES/) for standard texts of the identified third-party
license families. Original project portions are licensed under BSD-2-Clause —
see [LICENSE](LICENSE).

The workspace's current `BSD-2-Clause` Cargo metadata is not intended to
supersede terms attached to third-party portions. A final repository-wide SPDX
expression remains pending the per-file audit documented in the code-audit
baseline; the metadata is deliberately unchanged until that review is
complete.
