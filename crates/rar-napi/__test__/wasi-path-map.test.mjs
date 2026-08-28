import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  toGuestPath,
  toHostPath,
  wasiPreopens,
  mapCreateArchiveOptions,
  mapCreateResult,
  mapRepairArgs,
  mapAppendOptions,
  mapDeleteArgs,
  mapListArgs,
} from '../wasi-path-map.cjs'

test('win32 absolute paths map to guest /<DRIVE>:/ paths', () => {
  assert.equal(toGuestPath('C:\\Users\\me\\out.rar', 'win32'), '/C:/Users/me/out.rar')
  assert.equal(toGuestPath('C:/Users/me/out.rar', 'win32'), '/C:/Users/me/out.rar')
  assert.equal(toGuestPath('D:\\tmp\\x', 'win32'), '/D:/tmp/x')
  assert.equal(toGuestPath('c:\\lower\\x', 'win32'), '/C:/lower/x')
  assert.equal(toGuestPath('C:\\', 'win32'), '/C:')
  assert.equal(toGuestPath('C:', 'win32'), '/C:')
})

test('non-Windows and relative paths pass through unchanged', () => {
  assert.equal(toGuestPath('/tmp/x', 'linux'), '/tmp/x')
  assert.equal(toGuestPath('C:\\x', 'linux'), 'C:\\x')
  assert.equal(toGuestPath('tmp\\x', 'win32'), 'tmp\\x')
  assert.equal(toGuestPath('C:relative', 'win32'), 'C:relative')
})

test('guest paths map back to host Windows paths', () => {
  assert.equal(toHostPath('/C:/Users/me/out.rar', 'win32'), 'C:\\Users\\me\\out.rar')
  assert.equal(toHostPath('/D:/tmp/x', 'win32'), 'D:\\tmp\\x')
  assert.equal(toHostPath('/C:', 'win32'), 'C:\\')
  assert.equal(toHostPath('/tmp/x', 'win32'), '/tmp/x')
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

test('create result maps files back to host paths', () => {
  assert.deepEqual(
    mapCreateResult(
      { files: ['/C:/o.rar', '/C:/o.part1.rar'] },
      'win32',
    ).files,
    ['C:\\o.rar', 'C:\\o.part1.rar'],
  )
})

test('repair args map to guest paths', () => {
  assert.deepEqual(mapRepairArgs('C:\\in.rar', 'C:\\out.rar', 'win32'), [
    '/C:/in.rar',
    '/C:/out.rar',
  ])
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

test('delete args map archive path and pass names and password through', () => {
  assert.deepEqual(
    mapDeleteArgs('C:\\del.rar', ['a.txt', 'b.txt'], 'pw', 'win32'),
    ['/C:/del.rar', ['a.txt', 'b.txt'], 'pw'],
  )
})

test('list args map archive path and pass password through', () => {
  assert.deepEqual(mapListArgs('C:\\a.rar', 'pw', 'win32'), ['/C:/a.rar', 'pw'])
})
