# Changelog

Notable user-visible and release-engineering changes are recorded here. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Version history predating this file remains available in Git history and
`PLAN.md`.

## [0.7.0] - 2026-09-05

### Changed

- Expanded workspace metadata and documentation to describe the full supported
  RAR range rather than only RAR5.
- Hardened CI with locked root-workspace dependency resolution, all-target
  Clippy, default and no-default-feature checks, and an independent
  fuzz-workspace check.
- Added the security policy, third-party source inventory, full identified
  license-family texts, and audit/ADR/spec entries to the documentation index.
- Declared Rust 1.88 as the workspace MSRV and made all packages inherit it.
- Moved blocking N-API read, test, and listing operations onto asynchronous
  tasks, with validated JavaScript numeric options and stable error classes.
- Made CLI update and freshen operations transactional and recursive for
  directory inputs.

### Fixed

- Rejected malformed or overflowing RAR5 header lengths and enforced RAR4
  packed/unpacked size invariants without blocking unlimited streamed STORE
  extraction.
- Preserved quick-open locator compatibility, independent legacy solid-chain
  starts, archive-local automatic thread selection, and the exact RAR7
  dictionary encoding limit.
- Made archive staging collision-safe and replaced destinations atomically
  without deleting the original after an unrelated rename failure.
- Fixed CLI updates that silently skipped new members, selector suffix
  overmatching, unsafe bare-password behavior, switch-prefix ambiguity, size
  overflow, and non-streaming stdout extraction.
- Preserved progress, cancellation, and host-path mapping across native and
  WASI bindings, and returned multi-volume paths in natural order.
- Made release assembly fail when any contracted native or WASI asset is absent.
- Required generated JavaScript and TypeScript entrypoints from build artifacts
  to be identical before selecting them for a release.
- Excluded `SHA-256SUMS` from its own manifest and included `LICENSE`, `NOTICE`,
  the third-party source inventory, and identified license-family texts in
  release assets.
- Required release tags to match both the N-API Cargo and package versions.

## [Unreleased]
