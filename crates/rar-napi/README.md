# rar-rs for Node.js

[`@yjdyamv/rar-rs`](https://www.npmjs.com/package/@yjdyamv/rar-rs) is the
Node.js binding of **rar-rs**, a pure-Rust RAR toolkit: it creates, reads,
extracts, tests, modifies and repairs **RAR5/RAR7** archives and legacy **RAR
1.3–4.x** archives with **no external `rar`/`unrar` binary**.

It is a [napi-rs](https://napi.rs) addon: a native `.node` on every supported
platform, plus a WebAssembly (WASI) build for bundlers.

This README covers the package and how to build it. The full library/CLI
documentation is the repository [README](../../README.md).

## Install

```sh
npm install @yjdyamv/rar-rs
```

- Node.js **>= 18**.
- One package ships every platform binary (~14 MB) plus the WASI build. The
  generated loader picks the matching `.node`; bundlers reach the `.wasm`
  through `browser.js`.

## Quick start

```js
import {
  createArchive, listEntries, listEntriesDetailed, readMember,
  extractMember, extractArchive, testArchive,
} from '@yjdyamv/rar-rs'
// CommonJS: const rar = require('@yjdyamv/rar-rs')

// Create an archive from files on disk and from in-memory bytes.
const { files } = await createArchive({
  outPath: 'out.rar',
  level: 5,
  entries: [
    { kind: 'file', path: 'src/index.ts' },                  // stored name = basename
    { kind: 'file', path: 'src/lib.ts', name: 'lib/index.ts' },
    { kind: 'bytes', name: 'hello.txt', data: Buffer.from('hi') },
    { kind: 'dir', path: 'empty' },
  ],
})

// List in archive order: bare names, or full metadata.
const names = await listEntries('out.rar')
const info = await listEntriesDetailed('out.rar')

// Read one member into memory (bounded by the 4 GiB per-member read limit)…
const buf = await readMember('out.rar', 'hello.txt')
// …or stream it to disk.
const written = await extractMember('out.rar', 'hello.txt', 'dest')

// Extract everything. With `collectErrors` the call resolves and names the
// members it could not extract instead of throwing on the first one.
const report = await extractArchive('out.rar', { destPath: 'dest', collectErrors: true })
// report.written / .skipped / .refused / .corrected / .failures

// Verify integrity: [checked, failed]. A damaged member is reported, not thrown.
const [checked, failed] = await testArchive('out.rar')
```

## Errors

Every rejected call throws a `RarError`. Branch on **`error.rarCode`** (the
stable category) rather than the message, which may change:

```js
import { isRarError, isCancellationError, RAR_ERROR_CODES } from '@yjdyamv/rar-rs'

try {
  await extractArchive('a.rar', { destPath: 'out', password: 'wrong' })
} catch (err) {
  if (isCancellationError(err)) return                 // aborted via AbortSignal
  if (isRarError(err) && err.rarCode === 'wrong_password') return retry()
  if (isRarError(err) && err.rarCode === 'format') return reportCorrupt(err)
  throw err
}
```

- `err.rarCode` — one of `RAR_ERROR_CODES`: `'format'`, `'invalid_state'`,
  `'invalid_option'`, `'crc_mismatch'`, `'hash_mismatch'`, `'encrypted'`,
  `'unsupported'`, `'security'`, `'limit_exceeded'`, `'member_not_found'`,
  `'ambiguous_member'`, `'stale_entry_id'`, `'archive_locked'`, `'cancelled'`,
  `'wrong_password'`, `'create'`, `'io'`, `'internal'`.
- `err.code` — the raw N-API status string (`'InvalidArg'`, `'GenericFailure'`,
  `'Cancelled'`), kept for code that checked it before.
- `isRarError` works across multiple loaded copies of the module (it checks
  `rarCode`, not only the class).

## Progress and cancellation

The long-running calls take an optional `onProgress` callback and an
`AbortSignal`:

```js
const ac = new AbortController()
await createArchive(opts, ({ done, total }) => {
  // `done`/`total` are bytes; `total` is 0 when it cannot be known up front.
}, ac.signal)
// elsewhere: ac.abort() — the call rejects with a cancellation error.
```

`extractMember` takes a `signal` but no progress callback (a single member
either lands or fails); use `extractArchive` when you need progress.

## API

| Function                                                      | Returns                                                                      |
| ------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| `createArchive(opts, onProgress?, signal?)`                   | `{ files: string[] }` — every file produced (the whole set for multi-volume) |
| `appendEntries(opts, onProgress?, signal?)`                   | `{ files: string[] }` — single-volume archives only                          |
| `listEntries(path, password?)`                                | `string[]` — names in archive order                                          |
| `listEntriesDetailed(path, password?)`                        | `EntryInfo[]`                                                                |
| `listEntriesQuick(path, password?)`                           | `EntryInfo[]` — uses the quick-open record when present                      |
| `readMember(path, name, password?)`                           | `Buffer` — uncompressed bytes of one member                                  |
| `extractMember(path, name, destDir, password?, signal?)`      | `string` — resolved output path                                              |
| `extractArchive(path, opts, onProgress?, signal?)`            | `ExtractionResult`                                                           |
| `testArchive(path, password?)`                                | `[checked, failed]`                                                          |
| `deleteEntries(path, names, password?, onProgress?, signal?)` | `number` deleted                                                             |
| `renameEntries(path, renames, password?, signal?)`            | `number` renamed                                                             |
| `setComment(path, comment?, password?)`                       | `void` — `null`/`undefined` clears it                                        |
| `setMemberComment(path, member, comment?, password?)`         | `void` — RAR 1.5–4.x only                                                    |
| `setRecovery(path, percent, password?)`                       | `void` — inline recovery record, 0–100                                       |
| `lockArchive(path, password?)`                                | `void` — refuses further modification                                        |
| `repairArchive(input, output, onProgress?, signal?)`          | `boolean` — `false` when already intact (nothing written)                    |
| `rebuildMissingVolumes(firstVolume, onProgress?, signal?)`    | `string[]` — `.rev`-rebuilt volumes, in order                                |

### Entries (`EntryInput`)

`kind` selects the shape:

- `{ kind: 'file', path, name? }` — `name` defaults to the basename.
- `{ kind: 'dir', path, name? }` — an explicit directory entry.
- `{ kind: 'bytes', name, data }` — in-memory payload; `name` is required.
- `{ kind: 'redirect', name, redirType, target }` — a link entry with no data;
  `redirType` is `1` Unix symlink, `2` Windows symlink, `3` Windows junction,
  `4` hardlink, `5` file copy. RAR5/RAR7 only.

### `CreateArchiveOptions` (highlights)

- `outPath`, `entries`, `level` (0–5, default 3).
- `format` — `'rar5'` (default), `'rar7'`, `'rar4'`, `'rar2'`, `'rar15'`,
  `'rar13'`. RAR5-only options are rejected for the legacy formats, and
  `'rar13'` also rejects header encryption and recovery records/volumes.
- `password`, `encryptHeaders` — AES-256 file/header encryption.
- `dictSize` — `-md` style (`'128k'`, `'1m'`, `'6m'`, or bytes).
- `solid`, `solidReset` — `'continuous'` | `'volume'` | `'extension'`.
- `volumeSize`, `oldNumbering` — multi-volume sets.
- `recoveryPercent` | `recoverySectors`, `recoveryVolumeCount` |
  `recoveryVolumesPercent` — inline recovery records and `.rev` volumes.
- `quickOpen`, `blake2`, `threads`, `maxTotalBytes`.
- Timestamps: `saveCtime`, `saveAtime`, `saveMtime`, `timePrecisionSeconds`;
  Unix `saveOwner`; Windows `saveStreams` (NTFS alternate data streams).

### `ExtractArchiveOptions` (highlights)

- `destPath` (required), `password`.
- `flat` — basenames only; `autoRename` — `name(1).ext` on collision;
  `skipExisting` / `freshen` / `update` — `-o-` / `-f` / `-u` semantics.
- Limits: `maxDictSize` (default 4 GiB), `maxMetadataBytes` (default 64 MiB),
  `maxUnpackedBytes`, `maxTotalUnpackedBytes`; `threads`.
- `keepBroken`, `setCreationTime`, `setAccessTime`.
- Links/names: `skipLinks`, `allowUnsafeLinks`, `allowIncompatibleNames`,
  `fileVersionSuffix`.
- `collectErrors` — resolve with failures instead of throwing on the first one
  (cancellation always rejects).

### `EntryInfo`

`name`, `size`, `packedSize`, `method`, `isDir`, `mtime`, `crc32`, `ctime`,
`atime`, `hostOs`, `attributes`, `compVersion`, `version`, `dictSizeBytes`,
`comment`, `solid`. Sizes and times are JS numbers (bytes / Unix seconds);
optional fields are `undefined` when the header does not record them.

## Platforms

Native targets: `x86_64`/`i686`/`aarch64` Windows (MSVC), `x86_64`/`aarch64`
macOS, and `x86_64`/`aarch64`/`armv7` Linux (glibc and musl), plus
`wasm32-wasip1-threads`. The target list in `package.json` (`napi.targets`) is
the single source of truth.

On Windows a **MSVC** target is required (Node is MSVC-built); a GNU-default
host should build with `--target x86_64-pc-windows-msvc` — see the repository
README.

## Building from source

```sh
npm install
npm run build         # native .node + binding.js/binding.d.ts (release)
npm run build:debug   # unoptimized
npm run build:wasi    # the WASI build
npm test              # node --test __test__/*.test.mjs
npm run typecheck     # tsc --noEmit (checks rar-rs.d.ts against the API)
```

`binding.js`/`binding.d.ts`/`*.node` are build artifacts (gitignored);
`rar-rs.js`/`rar-rs.d.ts` are the hand-written public surface and must not be
generated. Keep `RarErrorCode` (in `rar-rs.d.ts`), `RAR_ERROR_CODES` (in
`rar-rs.js`) and `STABLE_CODES` (in `src/error.rs`) in sync.

Publishing is done by CI (`scripts/ci/publish-npm.sh`), which assembles this
package from the release payload and the repository root README.

## License

`BSD-2-Clause`. See `LICENSE`, `NOTICE` and `THIRD_PARTY_LICENSES.md` in this
directory.
