# Third-Party Source Inventory

Where the non-original code and fixtures in this repository came from, and under
what terms. Not legal advice, and not a substitute for [`LICENSE`](LICENSE),
[`NOTICE`](NOTICE) or the upstream license texts ([`LICENSES/`](LICENSES/)).

The repository-wide SPDX expression is `BSD-2-Clause`: the `Cargo.toml`
`[workspace.package] license` (inherited by all three crates and mirrored in
`crates/rar-napi/package.json`) states the project's **own** contributions.
Portions derived from other projects keep their own terms — the `rars`-derived
files below are `MIT OR Apache-2.0` — and are documented here and in `NOTICE`
rather than folded into that field, so a redistributor has to satisfy both.

## `rars` ports

Ported from [bitplane/rars](https://github.com/bitplane/rars) at revision
`c08a17b`, which declared `MIT OR Apache-2.0` (as does every published release).
Each file states this in its own header, and the table records which upstream
file it came from. The upstream files have **not** been diffed against a
checked-out `rars` repository, so treat a row as the in-tree claim.

| Our file                                                 | Upstream (`rars` @ `c08a17b`)                                             |
| -------------------------------------------------------- | ------------------------------------------------------------------------- |
| `crates/rar/src/codec/legacy/rar15.rs`                   | decode half, `codec/rar13.rs`                                             |
| `crates/rar/src/codec/legacy/rar15_encoder.rs`           | encode half, `codec/rar13.rs`                                             |
| `crates/rar/src/codec/legacy/rar20.rs`                   | decode half, `codec/rar20.rs`                                             |
| `crates/rar/src/codec/legacy/rar20_encoder.rs`           | encode half                                                               |
| `crates/rar/src/codec/legacy/rar29.rs`                   | decode half, `codec/rar29.rs`                                             |
| `crates/rar/src/codec/legacy/rar29_encoder.rs`           | encode half                                                               |
| `crates/rar/src/codec/legacy/ppmd.rs`                    | `codec/ppmd.rs`                                                           |
| `crates/rar/src/codec/legacy/rarvm.rs`                   | `codec/rarvm.rs` (generic filter bytecode interpreter)                    |
| `crates/rar/src/format/rar13/mod.rs`                     | `rar13.rs` (decode half: container read)                                  |
| `crates/rar/src/format/rar13/write.rs`                   | `rar13.rs` (encode half: header layout, member write)                     |
| `crates/rar/src/crypto/rar13.rs`                         | `crypto/rar13.rs`                                                         |
| `crates/rar/src/recovery/rev3/rs8.rs`                    | `recovery/rar3.rs` (GF(2^8) RS codec)                                     |
| `crates/rar/src/recovery/rev3/mod.rs`                    | `recovery/rar3.rs` + `.rev` layout knowledge                              |
| `crates/rar/src/codec/common/match_finder.rs`            | `codec/match_finder.rs` (LZMA BT4)                                        |
| `crates/rar/src/codec/common/filters.rs`                 | `codec/filters.rs`, `x86_filter_scan.rs`, RAR4 audio gate                 |
| `crates/rar/src/codec/modern/lzss_huff/encoder/parse.rs` | `codec/rar50.rs` (`optimal_tokens` / `TokenPrices`)                       |
| `crates/rar/src/format/rar5/blake2sp.rs`                 | `crates/rars/src/rar50/blake2sp.rs`                                       |
| `crates/rar/src/crypto/rar50.rs`                         | RAR5 KDF / hash-key MAC patterns                                          |
| `crates/rar/src/recovery/rar50/`                         | inline recovery-record codec (`codec/rar50.rs`; ported core in `gf16.rs`) |
| `crates/rar/src/recovery/legacy.rs`                      | `repair_protect_head_bytes`                                               |

## Other sources

- **libarchive RAR5 reader** (Grzegorz Antoniak, 2018, BSD-2-Clause): analysis
  reference for the RAR5 bitstream. BSD-2-Clause notices: `codec/mod.rs`,
  `codec/modern/lzss_huff/{mod,encoder,decoder}`, and `codec/common/huffman.rs`
  (stated as a basis rather than a license line).
- **`rars` test corpus**: the legacy fixtures under
  `crates/rar/tests/fixtures/rar13` and `rar40/`. Test inputs, not source; their
  own redistribution review is still outstanding.
- **`smart-archive-rar`**: origin of the binding now under `crates/rar-napi`.
- **RAR/WinRAR tools**: black-box interoperability references only; no code or
  binaries from them are included.

Registry, npm and build-time dependencies are pinned in `Cargo.lock`,
`fuzz/Cargo.lock` and `crates/rar-napi/package-lock.json`, which are the
authoritative lists; `cargo deny` checks their licenses against `deny.toml`.

When adding copied code, fixtures or a dependency: record the source and exact
revision, keep the upstream notices, and update the lockfile.
