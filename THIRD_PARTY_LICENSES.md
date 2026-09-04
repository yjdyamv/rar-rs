# Third-Party Source Inventory

This document is a provenance-oriented source inventory. It is not legal
advice, a complete software bill of materials, or a conclusion about which
license terms apply to a particular binary or source file. It does not replace
`LICENSE`, `NOTICE`, file-level notices, or upstream license texts. Verify the
exact source revision, copied files, modifications, and applicable terms before
redistribution.

The root Cargo `license = "BSD-2-Clause"` metadata remains unchanged while the
per-file audit described in `docs/CODE_AUDIT_2026-09-05.md` is open. That field
must not be read as overriding terms attached to third-party portions. A final
repository-wide SPDX expression requires maintainer or legal review.

## Identified license-family texts

The `LICENSES/` directory contains complete standard reference texts for the
MIT, Apache-2.0, and WTFPL license families identified in `NOTICE`. These files
are provided so source and release distributions carry the relevant license
language; they do not assign a license to any particular file. Exact
file-by-file applicability, upstream revisions, copyright notices, and any
license variations remain subject to the open audit. No copyright attribution
should be inferred from the reference-text filenames or placeholders.

- [`LICENSES/MIT.txt`](LICENSES/MIT.txt)
- [`LICENSES/Apache-2.0.txt`](LICENSES/Apache-2.0.txt)
- [`LICENSES/WTFPL.txt`](LICENSES/WTFPL.txt)

## Ported, derived, and reference sources

| Source | Upstream location | Recorded use |
|---|---|---|
| `rars` | <https://github.com/bitplane/rars> | Portions of recovery, BLAKE2sp, cryptographic patterns, match finding, parsing, filtering, and legacy codecs; `NOTICE` records revisions and unresolved differences between workspace metadata and file-level notices. |
| libarchive RAR5 reader | <https://github.com/libarchive/libarchive> | Independent implementation reference used during analysis of RAR5 structures; see `NOTICE` for the stated boundary. |
| `smart-archive-rar` | <https://github.com/yjdyamv/smart-archive-rar> | Origin of the N-API/native/WASI binding now under `crates/rar-napi`; see `NOTICE`. |
| `rars` test corpus | <https://github.com/bitplane/rars> | Source of legacy binary fixtures identified in `NOTICE`; fixtures are test inputs and require their own redistribution review. |
| RAR/WinRAR tools | <https://www.rarlab.com/> | Black-box interoperability references only; their binaries and source are not included by this project. |

The `rars` entries need file-by-file verification against the exact upstream
revision. In particular, do not infer that workspace-level metadata and a
later repository-level copying file apply interchangeably; `NOTICE` records
that issue without resolving it.

## Registry dependencies

Exact resolved versions and transitive sources are recorded in `Cargo.lock`,
`fuzz/Cargo.lock`, and `crates/rar-napi/package-lock.json`. The direct source
families currently include:

- Rust crates from <https://crates.io/>: `crc32fast`, RustCrypto `aes`, `hmac`,
  `sha1`, `sha2`, and `zeroize`, plus `rand`, `clap`, `rayon`, `wide`,
  `windows-sys`, `tempfile`, `napi`, `napi-derive`, and `napi-build`.
- Node/WASI packages from <https://www.npmjs.com/>: `@napi-rs/cli`,
  `@napi-rs/wasm-runtime`, `@emnapi/core`, `@emnapi/runtime`, and `typescript`,
  together with their lockfile-resolved transitive dependencies.
- Fuzz-only tooling: `libfuzzer-sys` from crates.io.

Listing a package here identifies its source; it does not assert that every
package is included in every distributed artifact or make a legal compatibility
judgment. Consult each locked package's own metadata and license files.

## Build and release tooling

CI installs `cargo-zigbuild` 0.23.4 from
<https://crates.io/crates/cargo-zigbuild> for musl cross-builds and uses the
GitHub Actions named in `.github/workflows/CI.yml`. These tools participate in
the build process; this inventory does not assert that their code is bundled in
release artifacts.

## Audit maintenance

When adding copied code, fixtures, generated runtime files, or a dependency:

1. record the exact source and revision;
2. preserve upstream and file-level notices;
3. update the relevant lockfile;
4. determine whether release artifacts need an additional license text; and
5. request legal review before changing the root SPDX metadata.
