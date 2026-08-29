import { test } from 'node:test'
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createReadStream } from 'node:fs'
import {
  mkdtempSync,
  readFileSync,
  writeFileSync,
  rmSync,
  readdirSync,
  mkdirSync,
  openSync,
  writeSync,
  closeSync,
  existsSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createArchive } from '../index.js'

const RAR5_SIG = Buffer.from([0x52, 0x61, 0x72, 0x21, 0x1a, 0x07, 0x01, 0x00])

// Regression fixture from rar-rs tests/fixtures/tail-match-362.bin: a 362-byte
// JSON file whose final two bytes match an earlier position at a cached
// distance. The rar5 prefilter used to read past the end of the buffer here
// and abort the whole process (SIGABRT), killing the VS Code extension host.
const TAIL_MATCH_FIXTURE = Buffer.from(`{
  "rules": {
    "no-control-regex": "off",
    "new-cap": "off",
    "no-underscore-dangle": "off",
    "unicorn/require-post-message-target-origin": "off",
    "unicorn/no-array-sort": "off"
  },
  "overrides": [
    {
      "files": ["media/**/*.js"],
      "rules": {
        "no-unused-vars": "off",
        "no-useless-escape": "off"
      }
    }
  ]
}
`)

test('regression: tail-match fixture compresses without aborting', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'tail.rar')
    const res = await createArchive({
      outPath: out,
      level: 3,
      entries: [{ kind: 'bytes', name: 'tail.json', data: TAIL_MATCH_FIXTURE }],
    })
    assert.deepEqual(res.files, [out])
    assert.equal(TAIL_MATCH_FIXTURE.length, 362)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

function tempDir() {
  return mkdtempSync(join(tmpdir(), 'sar-test-'))
}

function readFileHead(path, n = 8) {
  return new Promise((resolve, reject) => {
    const chunks = []
    const s = createReadStream(path, { start: 0, end: n - 1 })
    s.on('data', (c) => chunks.push(c))
    s.on('end', () => resolve(Buffer.concat(chunks)))
    s.on('error', reject)
  })
}

test('creates a RAR5 archive from bytes and disk files', async () => {
  const dir = tempDir()
  try {
    writeFileSync(join(dir, 'disk.txt'), 'from disk')
    const out = join(dir, 'out.rar')
    const res = await createArchive({
      outPath: out,
      level: 5,
      entries: [
        { kind: 'bytes', name: 'notes/a.bin', data: Buffer.alloc(100_000, 7) },
        { kind: 'file', path: join(dir, 'disk.txt'), name: 'docs/disk.txt' },
      ],
    })
    assert.deepEqual(res.files, [out])
    const head = await readFileHead(out)
    assert.deepEqual(head, RAR5_SIG)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('reports progress from 0 to 100%', async () => {
  const dir = tempDir()
  try {
    const events = []
    const out = join(dir, 'prog.rar')
    await createArchive(
      {
        outPath: out,
        entries: [{ kind: 'bytes', name: 'data.bin', data: Buffer.alloc(1_000_000, 3) }],
      },
      (_err, p) => events.push(p.done / p.total),
    )
    // Progress callbacks are delivered on the event loop; the last one can
    // arrive a tick after the promise resolves.
    await new Promise((resolve) => setTimeout(resolve, 50))
    assert.ok(events.length > 0, 'no progress events')
    assert.equal(events.at(-1), 1)
    for (const [a, b] of events.slice(1).map((v, i) => [events[i], v])) {
      assert.ok(a <= b, 'progress went backwards')
    }
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('reports folder progress without double-counting directory trees', async () => {
  const dir = tempDir()
  try {
    const src = join(dir, 'src')
    mkdirSync(src)
    // 24 small members -> parallel wave path; the plugin passes the folder
    // itself as a dir entry plus every child as an explicit file entry.
    for (let i = 0; i < 24; i++) {
      writeFileSync(join(src, `small-${i}.bin`), Buffer.alloc(256 * 1024, i % 251))
    }
    const entries = [
      { kind: 'dir', path: src, name: 'src' },
      ...readdirSync(src).map((f) => ({
        kind: 'file',
        path: join(src, f),
        name: `src/${f}`,
      })),
    ]
    const events = []
    const out = join(dir, 'out.rar')
    await createArchive(
      { outPath: out, entries, level: 3 },
      (_err, p) => events.push(p.done / p.total),
    )
    // Progress callbacks are delivered on the event loop; the last one can
    // arrive a tick after the promise resolves.
    await new Promise((resolve) => setTimeout(resolve, 50))

    assert.ok(events.length > 0, 'no progress events')
    for (const [a, b] of events.slice(1).map((v, i) => [events[i], v])) {
      assert.ok(a <= b, 'progress went backwards')
    }
    assert.ok(events.at(-1) >= 0.99, 'must end at 100%')
    // Regression: the dir tree used to be counted again in `total`, so the
    // per-member reports stalled around 50% until the terminal event.
    assert.ok(
      events.at(-2) >= 0.9,
      `progress stalled mid-way: second-to-last ratio=${events.at(-2)}`,
    )
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('never reports done > total for a >64MiB sequential file', async () => {
  const dir = tempDir()
  try {
    const big = join(dir, 'big.bin')
    const chunk = Buffer.alloc(4 * 1024 * 1024, 7)
    const fd = openSync(big, 'w')
    for (let i = 0; i < 17; i++) writeSync(fd, chunk)
    closeSync(fd)

    const events = []
    const out = join(dir, 'out.rar')
    await createArchive(
      {
        outPath: out,
        level: 3,
        entries: [{ kind: 'file', path: big, name: 'big.bin' }],
      },
      (_err, p) => events.push(p.done / p.total),
    )
    await new Promise((resolve) => setTimeout(resolve, 50))

    assert.ok(events.length > 1, 'expected multiple progress events')
    for (const ratio of events) {
      assert.ok(ratio <= 1, `done exceeded total: ${ratio}`)
    }
    assert.ok(events.at(-1) >= 0.99, 'must end at 100%')
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('creates multi-volume archives', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'vol.rar')
    const res = await createArchive({
      outPath: out,
      volumeSize: 100_000,
      level: 0,
      entries: [{ kind: 'bytes', name: 'big.bin', data: Buffer.alloc(250_000, 9) }],
    })
    assert.ok(res.files.length >= 3, `expected >=3 volumes, got ${res.files.length}`)
    for (const f of res.files) {
      assert.equal((await readFileHead(f)).subarray(0, 7).toString(), 'Rar!\x1a\x07\x01')
    }
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('creates archives via parallel batch with mixed entries', async () => {
  const dir = tempDir()
  try {
    writeFileSync(join(dir, 'disk.bin'), Buffer.alloc(300_000, 5))
    const out = join(dir, 'batch.rar')
    const res = await createArchive({
      outPath: out,
      level: 3,
      entries: [
        { kind: 'dir', path: dir, name: 'folder' },
        { kind: 'bytes', name: 'a.bin', data: Buffer.alloc(200_000, 1) },
        { kind: 'file', path: join(dir, 'disk.bin'), name: 'docs/disk.bin' },
        { kind: 'bytes', name: 'b.bin', data: Buffer.alloc(150_000, 2) },
      ],
    })
    assert.deepEqual(res.files, [out])
    const head = await readFileHead(out)
    assert.deepEqual(head, RAR5_SIG)
    // Official UNRAR validates the batch-produced archive when available.
    const unrar = process.env.SA_OFFICIAL_UNRAR || '/home/yuan/下载/rar/unrar'
    try {
      execFileSync(unrar, ['t', out], { stdio: 'pipe' })
    } catch (err) {
      if (err.code === 'ENOENT') return // unrar not installed: skip validation
      throw err
    }
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('rejects when maxTotalBytes is exceeded', async () => {
  const dir = tempDir()
  try {
    await assert.rejects(
      createArchive({
        outPath: join(dir, 'x.rar'),
        maxTotalBytes: 1000,
        entries: [{ kind: 'bytes', name: 'a.bin', data: Buffer.alloc(2000, 1) }],
      }),
      /exceeds limit/,
    )
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('rejects missing file paths', async () => {
  const dir = tempDir()
  try {
    await assert.rejects(
      createArchive({
        outPath: join(dir, 'x.rar'),
        entries: [{ kind: 'file', path: join(dir, 'nope.txt') }],
      }),
      /cannot stat/,
    )
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('rejects unknown entry kinds', async () => {
  const dir = tempDir()
  try {
    await assert.rejects(
      createArchive({
        outPath: join(dir, 'x.rar'),
        entries: [{ kind: 'gzip', name: 'a' }],
      }),
      /unknown entry kind/,
    )
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('appendEntries keeps existing members and listEntries/deleteEntries work', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'm.rar')
    await createArchive({
      outPath: out,
      entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('alpha') }],
    })

    const { appendEntries, listEntries, deleteEntries } = await import('../index.js')
    const res = await appendEntries({
      archivePath: out,
      level: 3,
      entries: [{ kind: 'bytes', name: 'dir/b.txt', data: Buffer.from('beta') }],
    })
    assert.deepEqual(res.files, [out])

    let names = listEntries(out)
    assert.deepEqual(names.sort(), ['a.txt', 'dir/b.txt'])

    const deleted = await deleteEntries(out, ['a.txt'])
    assert.equal(deleted, 1)
    names = listEntries(out)
    assert.deepEqual(names, ['dir/b.txt'])

    assert.throws(() => listEntries(join(dir, 'missing.rar')), /repair|open|read|rar5/i)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('extractArchive restores members byte-identically (incl. flat and password)', async () => {
  const dir = tempDir()
  try {
    const payload = Buffer.from('extract me please '.repeat(500))
    const out = join(dir, 'x.rar')
    await createArchive({
      outPath: out,
      password: 'pw',
      entries: [{ kind: 'bytes', name: 'sub/data.txt', data: payload }],
    })

    const { extractArchive } = await import('../index.js')
    // Wrong password fails.
    await assert.rejects(
      extractArchive(out, { destPath: join(dir, 'bad'), password: 'nope' }),
      /password|decrypt|rar5/i,
    )
    // Correct password restores the tree.
    const dest = join(dir, 'out')
    await extractArchive(out, { destPath: dest, password: 'pw' })
    assert.deepEqual(readFileSync(join(dest, 'sub', 'data.txt')), payload)
    // Flat extraction lands under the basename.
    const flatDest = join(dir, 'flat')
    await extractArchive(out, { destPath: flatDest, password: 'pw', flat: true })
    assert.deepEqual(readFileSync(join(flatDest, 'data.txt')), payload)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('listEntriesDetailed reports sizes and methods', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'd.rar')
    await createArchive({
      outPath: out,
      entries: [
        { kind: 'bytes', name: 'a.txt', data: Buffer.from('hello '.repeat(500)) },
        { kind: 'bytes', name: 'b.bin', data: Buffer.alloc(4096, 7) },
      ],
    })
    const { listEntriesDetailed } = await import('../index.js')
    const entries = listEntriesDetailed(out)
    assert.equal(entries.length, 2)
    const a = entries.find((e) => e.name === 'a.txt')
    assert.equal(a.size, 3000)
    assert.ok(a.packedSize < a.size, `a.txt should compress (${a.packedSize})`)
    assert.equal(a.method, 3)
    const b = entries.find((e) => e.name === 'b.bin')
    assert.equal(b.size, 4096)
    assert.ok(b.packedSize < 4096, 'repeated bytes must compress')
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('dictSize accepts powers of two up to 4 GiB and rejects invalid values', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'd.rar')
    await createArchive({
      outPath: out,
      dictSize: '64m',
      entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('data') }],
    })
    const { listEntriesDetailed } = await import('../index.js')
    const entries = listEntriesDetailed(out)
    assert.equal(entries.length, 1)

    // Values above 4 GiB are accepted (RAR7 path); for a small file the
    // 2x-file-size cap falls back to RAR5, so it still creates fine.
    const big = join(dir, 'big.rar')
    await createArchive({
      outPath: big,
      dictSize: '8g',
      entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('data') }],
    })
    assert.equal(listEntriesDetailed(big).length, 1)

    // Non-power-of-two values up to 4 GiB are rejected.
    await assert.rejects(
      createArchive({
        outPath: join(dir, 'bad.rar'),
        dictSize: '3m',
        entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('data') }],
      }),
      /powers of two|dictionary/,
    )
    // Garbage is rejected.
    await assert.rejects(
      createArchive({
        outPath: join(dir, 'bad2.rar'),
        dictSize: 'banana',
        entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('data') }],
      }),
      /invalid dictionary size/,
    )
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('abort signal cancels a running create and leaves no partial archive', async () => {
  const dir = tempDir()
  try {
    const src = join(dir, 'big.bin')
    const chunk = Buffer.alloc(4 * 1024 * 1024, 7)
    const fd = openSync(src, 'w')
    for (let i = 0; i < 12; i++) writeSync(fd, chunk)
    closeSync(fd)
    const out = join(dir, 'out.rar')

    // Abort before the worker starts: the promise must reject with the
    // cancellation error and no staged archive may appear.
    const c1 = new AbortController()
    setTimeout(() => c1.abort(), 5)
    await assert.rejects(
      createArchive(
        { outPath: out, level: 3, entries: [{ kind: 'file', path: src }] },
        undefined,
        c1.signal,
      ),
      /operation cancelled/,
    )
    // The rejection must carry the cancellation contract the consumer's
    // isCancellationError matches: name AbortError (unstarted work) or
    // code "Cancelled" (mid-run cooperative cancellation).
    const c1b = new AbortController()
    setTimeout(() => c1b.abort(), 5)
    await createArchive(
      { outPath: out, level: 3, entries: [{ kind: 'file', path: src }] },
      undefined,
      c1b.signal,
    ).catch((e) => {
      assert.ok(
        e.name === 'AbortError' || e.code === 'Cancelled',
        `cancellation contract: name=${e.name} code=${e.code}`,
      )
    })
    assert.equal(existsSync(out), false, 'no partial archive after abort')

    // Abort mid-compression: same cooperative cancellation, still clean.
    const c2 = new AbortController()
    setTimeout(() => c2.abort(), 150)
    await assert.rejects(
      createArchive(
        { outPath: out, level: 5, entries: [{ kind: 'file', path: src }] },
        undefined,
        c2.signal,
      ),
      /operation cancelled/,
    )
    assert.equal(existsSync(out), false, 'no partial archive after mid-run abort')

    // Without an abort the same archive completes normally.
    const res = await createArchive({
      outPath: out,
      level: 3,
      entries: [{ kind: 'file', path: src }],
    })
    assert.ok(res.files.includes(out))
    assert.ok(existsSync(out))
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('listEntriesQuick matches listEntriesDetailed on a quickOpen archive', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'q.rar')
    await createArchive({
      outPath: out,
      quickOpen: true,
      entries: [
        { kind: 'bytes', name: 'a.txt', data: Buffer.from('quick-open '.repeat(400)) },
        { kind: 'bytes', name: 'b.bin', data: Buffer.alloc(8192, 3) },
      ],
    })
    const { listEntriesDetailed, listEntriesQuick } = await import('../index.js')
    const full = listEntriesDetailed(out)
    const quick = listEntriesQuick(out)
    assert.deepEqual(quick, full, 'QO fast path must list identically')
    assert.equal(quick.length, 2)
    const a = quick.find((e) => e.name === 'a.txt')
    assert.equal(a.size, 4400)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('readMember returns a single member byte-exact', async () => {
  const dir = tempDir()
  try {
    const src = join(dir, 'preview.txt')
    const content = Buffer.from('readMember preview content '.repeat(500))
    writeFileSync(src, content)
    const rar = join(dir, 'a.rar')
    await createArchive({ outPath: rar, entries: [{ kind: 'file', path: src }] })
    const { readMember } = await import('../index.js')
    const data = Buffer.from(readMember(rar, 'preview.txt'))
    assert.deepEqual(data, content, 'member must read back byte-exact')
    // Encrypted member with password.
    const enc = join(dir, 'enc.rar')
    await createArchive({ outPath: enc, password: 'pw', entries: [{ kind: 'file', path: src }] })
    const dec = Buffer.from(readMember(enc, 'preview.txt', 'pw'))
    assert.deepEqual(dec, content)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('listEntriesQuick falls back on an archive without quickOpen', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'p.rar')
    await createArchive({
      outPath: out,
      entries: [{ kind: 'bytes', name: 'x.txt', data: Buffer.from('plain') }],
    })
    const { listEntriesQuick } = await import('../index.js')
    const quick = listEntriesQuick(out)
    assert.equal(quick.length, 1)
    assert.equal(quick[0].name, 'x.txt')
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('repairArchive streams a damaged archive back to byte-exact', async () => {
  const dir = tempDir()
  try {
    const src = join(dir, 'data.bin')
    // 1 MiB pseudo-random payload (deterministic xorshift) so the member
    // spans enough volume and byte 500 lands in protected data.
    const out = Buffer.alloc(1 << 20)
    let x = 0x9e3779b9
    for (let i = 0; i < out.length; i++) {
      x ^= x << 13; x ^= x >>> 17; x ^= x << 5
      out[i] = x & 0xff
    }
    writeFileSync(src, out)
    const good = join(dir, 'good.rar')
    await createArchive({ outPath: good, recoveryPercent: 10, entries: [{ kind: 'file', path: src }] })

    const bytes = Buffer.from(readFileSync(good))
    bytes[500] ^= 0xff
    bytes[510] ^= 0x5a
    const damaged = join(dir, 'damaged.rar')
    writeFileSync(damaged, bytes)

    const { repairArchive } = await import('../index.js')
    const fixed = join(dir, 'fixed.rar')
    assert.equal(await repairArchive(damaged, fixed), true, 'damage must be reported')
    assert.equal(existsSync(fixed), true)
    assert.deepEqual(readFileSync(fixed), readFileSync(good), 'byte-exact restore')

    // Intact archive: no repair, no output file.
    const out2 = join(dir, 'out2.rar')
    assert.equal(await repairArchive(good, out2), false, 'intact archive must report no repair')
    assert.equal(existsSync(out2), false, 'no output written for an intact archive')
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})
