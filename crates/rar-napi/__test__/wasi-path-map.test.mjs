import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import {
  toGuestPath,
  toHostPath,
  wasiPreopens,
  mapCreateArchiveOptions,
  mapPathsToHost,
  mapCreateResult,
  mapRepairArgs,
  mapAppendOptions,
  mapDeleteArgs,
  mapListArgs,
  mapExtractMemberArgs,
  mapRenameArgs,
  mapCommentArgs,
  mapRecoveryArgs,
  mapLockArgs,
} from '../wasi-path-map.cjs'

test('win32 absolute paths map to guest /<DRIVE>:/ paths', () => {
  assert.equal(toGuestPath('C:\\Users\\me\\out.rar', 'win32'), '/C:/Users/me/out.rar')
  assert.equal(toGuestPath('C:/Users/me/out.rar', 'win32'), '/C:/Users/me/out.rar')
  assert.equal(toGuestPath('D:\\tmp\\x', 'win32'), '/D:/tmp/x')
  assert.equal(toGuestPath('c:\\lower\\x', 'win32'), '/C:/lower/x')
  assert.equal(toGuestPath('C:\\', 'win32'), '/C:')
  assert.equal(toGuestPath('C:', 'win32'), '/C:')
})

test('non-Windows absolute paths pass through; relative paths resolve', () => {
  assert.equal(toGuestPath('/tmp/x', 'linux'), '/tmp/x')
  assert.equal(toGuestPath('C:\\x', 'linux'), 'C:\\x')
  assert.equal(toGuestPath('out.rar', 'linux', '/work/proj'), '/work/proj/out.rar')
  assert.equal(
    toGuestPath('./sub/../x', 'linux', '/work/proj'),
    '/work/proj/x',
  )
})

test('win32 relative paths resolve against the cwd and then map', () => {
  const cwd = 'C:\\work\\proj'
  assert.equal(toGuestPath('out.rar', 'win32', cwd), '/C:/work/proj/out.rar')
  assert.equal(
    toGuestPath('.\\sub\\out.rar', 'win32', cwd),
    '/C:/work/proj/sub/out.rar',
  )
  assert.equal(toGuestPath('..\\out.rar', 'win32', cwd), '/C:/work/out.rar')
  assert.equal(
    toGuestPath('C:relative', 'win32', cwd),
    '/C:/work/proj/relative',
  )
  // Absolute drive paths do not depend on the cwd.
  assert.equal(toGuestPath('D:\\tmp\\x', 'win32', cwd), '/D:/tmp/x')
  // Rooted and guest-style paths keep their existing pass-through.
  assert.equal(toGuestPath('/tmp/x', 'win32', cwd), '/tmp/x')
})

test('guest paths map back to host Windows paths', () => {
  assert.equal(toHostPath('/C:/Users/me/out.rar', 'win32'), 'C:\\Users\\me\\out.rar')
  assert.equal(toHostPath('/D:/tmp/x', 'win32'), 'D:\\tmp\\x')
  assert.equal(toHostPath('/C:', 'win32'), 'C:\\')
  assert.equal(toHostPath('/tmp/x', 'win32'), '/tmp/x')
})

test('resolved relative paths round-trip back to host paths', () => {
  const cwd = 'C:\\work\\proj'
  assert.equal(
    toHostPath(toGuestPath('..\\out.rar', 'win32', cwd), 'win32'),
    'C:\\work\\out.rar',
  )
  assert.equal(
    toHostPath(toGuestPath('out.rar', 'win32', cwd), 'win32'),
    'C:\\work\\proj\\out.rar',
  )
  assert.equal(
    toHostPath(toGuestPath('out.rar', 'linux', '/work'), 'linux'),
    '/work/out.rar',
  )
})

test('relative paths default to the Node process cwd', () => {
  const resolved = resolve(process.cwd(), 'out.rar')
  if (process.platform === 'win32') {
    // The guest spelling is drive-mapped, so check through the inverse.
    assert.equal(toHostPath(toGuestPath('out.rar')), resolved)
  } else {
    assert.equal(toGuestPath('out.rar'), resolved)
  }
  assert.equal(
    toGuestPath('out.rar', process.platform),
    toGuestPath('out.rar', process.platform, process.cwd()),
  )
})

test('preopens map / plus each existing drive on win32', () => {
  const exists = (p) => p === 'C:\\' || p === 'D:\\'
  const pre = wasiPreopens('D:\\', 'win32', exists)
  assert.equal(pre['/'], 'D:\\')
  assert.equal(pre['/C:'], 'C:\\')
  assert.equal(pre['/D:'], 'D:\\')
  assert.equal(pre['/E:'], undefined)
  assert.deepEqual(wasiPreopens('/', 'linux', exists), { '/': '/' })
})

test('createArchive options map paths but preserve other fields', () => {
  const options = {
    outPath: 'C:\\o.rar',
    entries: [
      { kind: 'file', path: 'C:\\a.txt', name: 'a.txt' },
      { kind: 'bytes', name: 'b.bin', data: Buffer.from([1]) },
    ],
  }
  const mapped = mapCreateArchiveOptions(options, 'win32')
  assert.equal(mapped.outPath, '/C:/o.rar')
  assert.equal(mapped.entries[0].path, '/C:/a.txt')
  assert.equal(mapped.entries[0].name, 'a.txt')
  assert.equal(mapped.entries[1].name, 'b.bin')
  assert.equal(mapped.entries[1].path, undefined)
  assert.equal(options.outPath, 'C:\\o.rar', 'input options must not mutate')
})

test('create/append mappers inherit cwd resolution for relative paths', () => {
  const mapped = mapCreateArchiveOptions(
    {
      outPath: 'out.rar',
      entries: [{ kind: 'file', path: 'in.txt', name: 'in.txt' }],
    },
    process.platform,
  )
  assert.equal(mapped.outPath, toGuestPath('out.rar', process.platform))
  assert.equal(mapped.entries[0].path, toGuestPath('in.txt', process.platform))
  assert.equal(
    mapAppendOptions({ archivePath: 'a.rar' }, process.platform).archivePath,
    toGuestPath('a.rar', process.platform),
  )
})

test('create result maps files back to host paths', () => {
  assert.deepEqual(
    mapCreateResult(
      { files: ['/C:/o.rar', '/C:/o.part1.rar'] },
      'win32',
    ).files,
    ['C:\\o.rar', 'C:\\o.part1.rar'],
  )
})

test('repair args map paths and preserve progress and signal', () => {
  const progress = () => {}
  const signal = new AbortController().signal
  assert.deepEqual(
    mapRepairArgs(
      'C:\\in.rar',
      'C:\\out.rar',
      progress,
      signal,
      'win32',
    ),
    ['/C:/in.rar', '/C:/out.rar', progress, signal],
  )
})

test('append options map archive path and entries but preserve other fields', () => {
  const options = {
    archivePath: 'C:\\existing.rar',
    entries: [{ kind: 'file', path: 'C:\\a.txt', name: 'a.txt' }],
    level: 3,
  }
  const mapped = mapAppendOptions(options, 'win32')
  assert.equal(mapped.archivePath, '/C:/existing.rar')
  assert.equal(mapped.entries[0].path, '/C:/a.txt')
  assert.equal(mapped.entries[0].name, 'a.txt')
  assert.equal(mapped.level, 3)
  assert.equal(options.archivePath, 'C:\\existing.rar', 'input options must not mutate')
})

test('delete args preserve password, progress, and signal', () => {
  const progress = () => {}
  const signal = new AbortController().signal
  assert.deepEqual(
    mapDeleteArgs(
      'C:\\del.rar',
      ['a.txt', 'b.txt'],
      'pw',
      progress,
      signal,
      'win32',
    ),
    ['/C:/del.rar', ['a.txt', 'b.txt'], 'pw', progress, signal],
  )
})

test('list args map archive path and pass password through', () => {
  assert.deepEqual(mapListArgs('C:\\a.rar', 'pw', 'win32'), ['/C:/a.rar', 'pw'])
})

test('rebuilt volume paths map back to the host in original order', () => {
  assert.deepEqual(
    mapPathsToHost(
      ['/C:/v.part1.rar', '/C:/v.part10.rar', '/C:/v.part2.rar'],
      'win32',
    ),
    ['C:\\v.part1.rar', 'C:\\v.part10.rar', 'C:\\v.part2.rar'],
  )
})

test('create/append entry mappers pass redirect entries through untouched', () => {
  const redirect = {
    kind: 'redirect',
    name: 'lnk.txt',
    redirType: 5,
    target: 'target.txt',
  }
  // Redirect entries carry no host path, so the win32 mapper must not
  // touch them (only `path` string fields are translated).
  assert.deepEqual(
    mapCreateArchiveOptions(
      {
        outPath: 'C:\\a.rar',
        entries: [{ kind: 'bytes', name: 't.txt', data: [1] }, redirect],
      },
      'win32',
    ),
    {
      outPath: '/C:/a.rar',
      entries: [{ kind: 'bytes', name: 't.txt', data: [1] }, redirect],
    },
  )
  assert.deepEqual(
    mapAppendOptions({ archivePath: 'C:\\a.rar', entries: [redirect] }, 'win32'),
    { archivePath: '/C:/a.rar', entries: [redirect] },
  )
})

test('extractMember args map archive, dest dir, and preserve name/password/signal', () => {
  const signal = new AbortController().signal
  assert.deepEqual(
    mapExtractMemberArgs(
      'C:\\a.rar',
      'sub/file.txt',
      'C:\\out',
      'pw',
      signal,
      'win32',
    ),
    ['/C:/a.rar', 'sub/file.txt', '/C:/out', 'pw', signal],
  )
})

test('rename args map the archive path and pass renames through', () => {
  const renames = [{ from: 'a.txt', to: 'b.txt' }]
  const signal = new AbortController().signal
  assert.deepEqual(
    mapRenameArgs('C:\\a.rar', renames, 'pw', signal, 'win32'),
    ['/C:/a.rar', renames, 'pw', signal],
  )
  assert.deepEqual(
    mapRenameArgs('/tmp/a.rar', renames, null, null, 'linux'),
    ['/tmp/a.rar', renames, null, null],
  )
})

test('comment, recovery, and lock args map the archive path only', () => {
  assert.deepEqual(
    mapCommentArgs('C:\\a.rar', 'note', 'pw', 'win32'),
    ['/C:/a.rar', 'note', 'pw'],
  )
  assert.deepEqual(
    mapRecoveryArgs('C:\\a.rar', 10, 'pw', 'win32'),
    ['/C:/a.rar', 10, 'pw'],
  )
  assert.deepEqual(mapLockArgs('C:\\a.rar', 'pw', 'win32'), ['/C:/a.rar', 'pw'])
})

test('WASI patch templates keep async operation contracts', () => {
  const source = readFileSync(
    new URL('../scripts/patch-wasi-loader.mjs', import.meta.url),
    'utf8',
  )
  assert.match(
    source,
    /mapDeleteArgs\([\s\S]*?onProgress,[\s\S]*?signal,[\s\S]*?\)/,
  )
  assert.match(
    source,
    /mapRepairArgs\([\s\S]*?onProgress,[\s\S]*?signal,[\s\S]*?\)/,
  )
  assert.match(
    source,
    /rebuildMissingVolumes[\s\S]*?\.then\(\(paths\) => __wasiPathMap\.mapPathsToHost\(paths\)\)/,
  )
  // The editor-op + member-extract exports added since the loader patch
  // was first written must keep their own path mapping wrappers.
  assert.match(
    source,
    /mapExtractMemberArgs[\s\S]*?destDir,[\s\S]*?password,[\s\S]*?signal,[\s\S]*?\)[\s\S]*?\.then\(\(path\) => __wasiPathMap\.toHostPath\(path\)\)/,
  )
  for (const fn of ['renameEntries', 'setComment', 'setRecovery', 'lockArchive']) {
    assert.match(
      source,
      new RegExp(`module\\.exports\\.${fn} = function __wasi${fn
        .charAt(0)
        .toUpperCase()}${fn.slice(1)}Wrapper`),
      `${fn} must be wrapped`,
    )
  }
})
