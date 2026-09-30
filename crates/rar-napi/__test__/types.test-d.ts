/**
 * Compile-time check of the public TypeScript surface.
 *
 * Nothing here runs: `npm run typecheck` compiles this file with `tsc
 * --noEmit`, so a change that breaks a caller fails CI instead of surfacing in
 * a consumer. Prefer an assertion here over a runtime test for anything that
 * is purely a typing rule.
 */

import {
  RAR_ERROR_CODES,
  RarError,
  appendEntries,
  createArchive,
  deleteEntries,
  extractArchive,
  extractMember,
  isCancellationError,
  isRarError,
  listEntries,
  listEntriesDetailed,
  listEntriesQuick,
  lockArchive,
  readMember,
  rebuildMissingVolumes,
  renameEntries,
  repairArchive,
  setComment,
  setMemberComment,
  setRecovery,
  testArchive,
  type CreateResult,
  type EntryInfo,
  type ExtractionResult,
  type MemberFailure,
  type ProgressCallback,
  type ProgressData,
  type RarErrorCode,
} from '../rar-rs.js'

/** Fails the build when `T` is not exactly `Expected`. */
type Exact<T, Expected> = [T] extends [Expected] ? ([Expected] extends [T] ? true : false) : false
const assertExact = <T, Expected>(_ok: Exact<T, Expected>): void => {}

// The error class is what a rejection is, and its code is the closed union.
assertExact<RarError['rarCode'], RarErrorCode>(true)
assertExact<ExtractionResult['failures'], MemberFailure[]>(true)
assertExact<Awaited<ReturnType<typeof testArchive>>, number[]>(true)
assertExact<Awaited<ReturnType<typeof listEntries>>, string[]>(true)
assertExact<Awaited<ReturnType<typeof listEntriesDetailed>>, EntryInfo[]>(true)
assertExact<Awaited<ReturnType<typeof listEntriesQuick>>, EntryInfo[]>(true)
assertExact<Awaited<ReturnType<typeof extractArchive>>, ExtractionResult>(true)

// Progress callbacks take the event, not napi-rs's leading `err`.
const onProgress: ProgressCallback = (progress: ProgressData) => {
  assertExact<typeof progress.done, number>(true)
  assertExact<typeof progress.total, number>(true)
}

export async function typeSurface(): Promise<void> {
  const created: CreateResult = await createArchive(
    { outPath: 'a.rar', entries: [{ kind: 'bytes', name: 'a', data: Buffer.from('x') }] },
    onProgress,
  )
  assertExact<typeof created.files, string[]>(true)

  await appendEntries({ archivePath: 'a.rar', entries: [] }, onProgress)

  await listEntries('a.rar')
  await listEntries('a.rar', 'password')
  await listEntriesDetailed('a.rar')
  await listEntriesQuick('a.rar', null)

  const data: Buffer = await readMember('a.rar', 'a')
  assertExact<typeof data, Buffer>(true)

  // Extraction is structured: the result names what was written and what
  // failed, and the option that makes failures non-fatal is typed.
  const extracted: ExtractionResult = await extractArchive(
    'a.rar',
    { destPath: 'out', collectErrors: true, threads: 2, maxUnpackedBytes: 1024 },
    onProgress,
  )
  for (const failure of extracted.failures) {
    assertExact<typeof failure.name, string>(true)
    assertExact<typeof failure.index, number>(true)
    assertExact<typeof failure.message, string>(true)
  }

  const path: string = await extractMember('a.rar', 'a', 'out', 'pw')
  assertExact<typeof path, string>(true)

  await deleteEntries('a.rar', ['a'], 'pw', onProgress)
  await renameEntries('a.rar', [{ from: 'a', to: 'b' }], 'pw')
  await setComment('a.rar', 'note')
  await setComment('a.rar', null)
  await setMemberComment('a.rar', 'a', null)
  await setRecovery('a.rar', 10)
  await lockArchive('a.rar')
  const repaired: boolean = await repairArchive('in.rar', 'out.rar', onProgress)
  assertExact<typeof repaired, boolean>(true)
  const rebuilt: string[] = await rebuildMissingVolumes('set.part1.rar', onProgress)
  assertExact<typeof rebuilt, string[]>(true)
}

/** Narrowing an unknown rejection is the documented caller pattern. */
export function narrow(error: unknown): RarErrorCode | 'not-rar' {
  if (isRarError(error)) return error.rarCode
  if (isCancellationError(error)) return 'cancelled'
  return 'not-rar'
}

/** The runtime code list is exhaustive against the type. */
const codes: readonly RarErrorCode[] = RAR_ERROR_CODES
assertExact<typeof codes, readonly RarErrorCode[]>(true)
