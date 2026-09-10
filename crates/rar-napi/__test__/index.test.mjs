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

test('creates 10+ volumes in natural discovery order', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'vol.rar')
    const res = await createArchive({
      outPath: out,
      volumeSize: 100_000,
      level: 0,
      entries: [{ kind: 'bytes', name: 'big.bin', data: Buffer.alloc(1_200_000, 9) }],
    })
    assert.ok(res.files.length >= 12, `expected >=12 volumes, got ${res.files.length}`)
    const width = String(res.files.length).length
    assert.deepEqual(
      res.files,
      res.files.map((_, index) =>
        join(dir, `vol.part${String(index + 1).padStart(width, '0')}.rar`),
      ),
    )
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

test('rejects invalid JS numeric options with InvalidArg', async () => {
  const dir = tempDir()
  try {
    const invalidOptions = [
      ['level', -1],
      ['level', 6],
      ['level', 1.5],
      ['level', Number.NaN],
      ['threads', 0],
      ['threads', 65],
      ['threads', Number.POSITIVE_INFINITY],
      ['recoveryPercent', -1],
      ['recoveryPercent', 101],
      ['recoveryPercent', 1.5],
      ['recoveryVolumeCount', -1],
      ['recoveryVolumeCount', 2 ** 32],
      ['volumeSize', 0],
      ['volumeSize', -1],
      ['volumeSize', 1.5],
      ['volumeSize', Number.MAX_SAFE_INTEGER + 1],
      ['maxTotalBytes', -1],
      ['maxTotalBytes', 1.5],
      ['maxTotalBytes', Number.MAX_SAFE_INTEGER + 1],
    ]

    for (const [field, value] of invalidOptions) {
      await assert.rejects(
        createArchive({
          outPath: join(dir, `${field}-${String(value)}.rar`),
          entries: [{ kind: 'bytes', name: 'a.bin', data: Buffer.from([1]) }],
          [field]: value,
        }),
        (error) => {
          assert.equal(error.code, 'InvalidArg', `${field}=${String(value)}`)
          return true
        },
      )
    }

    const archive = join(dir, 'valid.rar')
    await createArchive({
      outPath: archive,
      entries: [{ kind: 'bytes', name: 'a.bin', data: Buffer.from([1]) }],
    })
    const { extractArchive } = await import('../index.js')
    for (const value of [-1, 1.5, Number.POSITIVE_INFINITY, Number.MAX_SAFE_INTEGER + 1]) {
      await assert.rejects(
        extractArchive(archive, {
          destPath: join(dir, `extract-${String(value)}`),
          maxDictSize: value,
        }),
        (error) => {
          assert.equal(error.code, 'InvalidArg', `maxDictSize=${String(value)}`)
          return true
        },
      )
    }

    // This passes the JS-number validation and is rejected by the core as
    // LimitExceeded because it is below the member's required dictionary.
    await assert.rejects(
      extractArchive(archive, {
        destPath: join(dir, 'extract-limited'),
        maxDictSize: 1,
      }),
      (error) => {
        assert.equal(error.code, 'InvalidArg')
        return true
      },
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

    const pendingNames = listEntries(out)
    assert.equal(typeof pendingNames.then, 'function', 'listEntries must be async')
    let names = await pendingNames
    assert.deepEqual(names.sort(), ['a.txt', 'dir/b.txt'])

    const deleted = await deleteEntries(out, ['a.txt'])
    assert.equal(deleted, 1)
    names = await listEntries(out)
    assert.deepEqual(names, ['dir/b.txt'])

    await assert.rejects(listEntries(join(dir, 'missing.rar')), (error) => {
      assert.equal(error.code, 'GenericFailure')
      return true
    })
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
    const pendingEntries = listEntriesDetailed(out)
    assert.equal(
      typeof pendingEntries.then,
      'function',
      'listEntriesDetailed must be async',
    )
    const entries = await pendingEntries
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
    const entries = await listEntriesDetailed(out)
    assert.equal(entries.length, 1)

    // Values above 4 GiB are accepted (RAR7 path); for a small file the
    // 2x-file-size cap falls back to RAR5, so it still creates fine.
    const big = join(dir, 'big.rar')
    await createArchive({
      outPath: big,
      dictSize: '8g',
      entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('data') }],
    })
    assert.equal((await listEntriesDetailed(big)).length, 1)

    // Non-power-of-two values up to 4 GiB are rejected.
    await assert.rejects(
      createArchive({
        outPath: join(dir, 'bad.rar'),
        dictSize: '3m',
        entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('data') }],
      }),
      /powers of two|dictionary/,
    )
    // Garbage is rejected by the binding parser.
    await assert.rejects(
      createArchive({
        outPath: join(dir, 'bad2.rar'),
        dictSize: 'banana',
        entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('data') }],
      }),
      /invalid dictionary size/,
    )
    // A syntactically valid dictionary above the core maximum reaches
    // RarError::InvalidOption and must retain the InvalidArg code.
    await assert.rejects(
      createArchive({
        outPath: join(dir, 'bad3.rar'),
        dictSize: '256g',
        entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('data') }],
      }),
      (error) => {
        assert.equal(error.code, 'InvalidArg')
        return true
      },
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
    const full = await listEntriesDetailed(out)
    const quick = await listEntriesQuick(out)
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
    const pendingRead = readMember(rar, 'preview.txt')
    assert.equal(typeof pendingRead.then, 'function', 'readMember must be async')
    const data = Buffer.from(await pendingRead)
    assert.deepEqual(data, content, 'member must read back byte-exact')
    // Encrypted member with password.
    const enc = join(dir, 'enc.rar')
    await createArchive({ outPath: enc, password: 'pw', entries: [{ kind: 'file', path: src }] })
    const dec = Buffer.from(await readMember(enc, 'preview.txt', 'pw'))
    assert.deepEqual(dec, content)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('core errors expose stable napi codes and archive testing is async', async () => {
  const dir = tempDir()
  try {
    const { listEntries, readMember, testArchive } = await import('../index.js')
    const plain = join(dir, 'plain.rar')
    await createArchive({
      outPath: plain,
      entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('alpha') }],
    })

    await assert.rejects(readMember(plain, 'missing.txt'), (error) => {
      assert.equal(error.code, 'InvalidArg')
      return true
    })

    const encrypted = join(dir, 'encrypted.rar')
    await createArchive({
      outPath: encrypted,
      password: 'correct',
      entries: [{ kind: 'bytes', name: 'secret.txt', data: Buffer.from('secret') }],
    })
    await assert.rejects(readMember(encrypted, 'secret.txt', 'wrong'), (error) => {
      assert.equal(error.code, 'InvalidArg')
      return true
    })

    const malformed = join(dir, 'malformed.rar')
    writeFileSync(malformed, Buffer.from('not a rar archive'))
    await assert.rejects(listEntries(malformed), (error) => {
      assert.equal(error.code, 'InvalidArg')
      return true
    })

    // This option combination is rejected by the typed writer's option
    // validation (InvalidOption) and must retain the InvalidArg code.
    await assert.rejects(
      createArchive({
        outPath: join(dir, 'unsupported.rar'),
        volumeSize: 100_000,
        recoveryPercent: 10,
        entries: [{ kind: 'bytes', name: 'a.bin', data: Buffer.alloc(200_000) }],
      }),
      (error) => {
        assert.equal(error.code, 'InvalidArg')
        return true
      },
    )

    const pendingTest = testArchive(plain)
    assert.equal(typeof pendingTest.then, 'function', 'testArchive must be async')
    assert.deepEqual(await pendingTest, [1, 0])
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
    const quick = await listEntriesQuick(out)
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

test('renameEntries renames members by name (like rar rn)', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'rn.rar')
    await createArchive({
      outPath: out,
      entries: [
        { kind: 'bytes', name: 'alpha.txt', data: Buffer.from('alpha ') },
        { kind: 'bytes', name: 'sub/beta.txt', data: Buffer.from('beta ') },
      ],
    })

    const { renameEntries, listEntries, readMember } = await import('../index.js')
    const pendingRename = renameEntries(out, [{ from: 'alpha.txt', to: 'renamed.txt' }])
    assert.equal(typeof pendingRename.then, 'function', 'renameEntries must be async')
    assert.equal(await pendingRename, 1)
    let names = (await listEntries(out)).sort()
    assert.deepEqual(names, ['renamed.txt', 'sub/beta.txt'])
    assert.deepEqual(await readMember(out, 'renamed.txt'), Buffer.from('alpha '))

    // Duplicate target names are allowed (duplicates are first-class).
    assert.equal(await renameEntries(out, [{ from: 'renamed.txt', to: 'sub/beta.txt' }]), 1)

    // A name matching nothing fails the plan before any rewrite.
    await assert.rejects(
      renameEntries(out, [{ from: 'missing.txt', to: 'x.txt' }]),
      (error) => error.message.includes('missing.txt'),
    )
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('setComment sets and removes the archive comment (like rar c)', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'cmt.rar')
    await createArchive({
      outPath: out,
      entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('alpha') }],
    })

    const { setComment } = await import('../index.js')
    const pendingComment = setComment(out, 'my-comment-marker-42')
    assert.equal(typeof pendingComment.then, 'function', 'setComment must be async')

    // The RAR5 archive comment is stored plaintext in a CMT block, so its
    // presence is structurally checkable in the raw bytes.
    await pendingComment
    let bytes = Buffer.from(readFileSync(out))
    assert.ok(bytes.includes(Buffer.from('my-comment-marker-42')), 'comment bytes present')

    // Null removes the comment.
    await setComment(out, null)
    bytes = Buffer.from(readFileSync(out))
    assert.ok(!bytes.includes(Buffer.from('my-comment-marker-42')), 'comment bytes gone')
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('setRecovery rebuilds the inline recovery record (like rar rr)', async () => {
  const dir = tempDir()
  try {
    const src = join(dir, 'data.bin')
    const out = Buffer.alloc(1 << 20)
    let x = 0x9e3779b9
    for (let i = 0; i < out.length; i++) {
      x ^= x << 13; x ^= x >>> 17; x ^= x << 5
      out[i] = x & 0xff
    }
    writeFileSync(src, out)
    const good = join(dir, 'good.rar')
    // No recovery record yet.
    await createArchive({ outPath: good, entries: [{ kind: 'file', path: src }] })

    const { setRecovery, repairArchive } = await import('../index.js')
    await setRecovery(good, 10)

    // Corrupt protected data and repair: must now be byte-exact.
    const bytes = Buffer.from(readFileSync(good))
    bytes[500] ^= 0xff
    const damaged = join(dir, 'damaged.rar')
    writeFileSync(damaged, bytes)
    const fixed = join(dir, 'fixed.rar')
    assert.equal(await repairArchive(damaged, fixed), true, 'damage must be repairable')
    assert.deepEqual(readFileSync(fixed), readFileSync(good), 'byte-exact restore')

    // An out-of-range percent is rejected up front.
    await assert.rejects(setRecovery(good, 101), (error) => {
      assert.equal(error.code, 'InvalidArg')
      return true
    })
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('lockArchive makes the archive read-only (like rar k)', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'lk.rar')
    await createArchive({
      outPath: out,
      entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('alpha') }],
    })

    const { lockArchive, deleteEntries } = await import('../index.js')
    await lockArchive(out)
    await assert.rejects(deleteEntries(out, ['a.txt']), (error) => {
      assert.equal(error.code, 'GenericFailure')
      return true
    })
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('listEntriesDetailed reports extended metadata (crc32, dates, version, solid)', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'meta.rar')
    await createArchive({
      outPath: out,
      solid: true,
      entries: [
        { kind: 'bytes', name: 'a.txt', data: Buffer.from('hello '.repeat(500)) },
        { kind: 'bytes', name: 'd.txt', data: Buffer.from('world '.repeat(500)) },
      ],
    })

    const { listEntriesDetailed } = await import('../index.js')
    const entries = await listEntriesDetailed(out)
    assert.equal(entries.length, 2)
    const a = entries.find((e) => e.name === 'a.txt')
    assert.equal(typeof a.crc32, 'number', 'crc32 present for a computed member')
    assert.ok(a.crc32 >= 0)
    assert.equal(a.version, 'v50')
    assert.equal(a.compVersion, 0)
    assert.ok(
      entries.some((e) => e.solid),
      'a solid archive marks at least one member as solid',
    )
    assert.equal(typeof a.hostOs, 'number')
    assert.equal(typeof a.attributes, 'number')
    assert.ok(
      a.dictSizeBytes == null || typeof a.dictSizeBytes === 'number',
      'dictSizeBytes is null/undefined or a number',
    )
    assert.equal(a.comment, undefined, 'no member comment was written')
    for (const field of ['ctime', 'atime']) {
      assert.ok(
        a[field] == null || typeof a[field] === 'number',
        `${field} is null/undefined or a number`,
      )
    }
    assert.ok(a.mtime > 0, 'mtime set')
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('createArchive format selects rar4, rar5, and rar7 containers', async () => {
  const dir = tempDir()
  try {
    const { listEntriesDetailed, readMember } = await import('../index.js')

    // RAR4 (legacy 7-byte signature, unp_ver 29 from the RAR4 pipeline).
    const r4 = join(dir, 'legacy.rar')
    await createArchive({
      outPath: r4,
      format: 'rar4',
      entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('legacy body') }],
    })
    const r4head = await readFileHead(r4, 7)
    assert.deepEqual(
      r4head,
      Buffer.from([0x52, 0x61, 0x72, 0x21, 0x1a, 0x07, 0x00]),
      'RAR4 7-byte signature',
    )
    assert.deepEqual(await readMember(r4, 'a.txt'), Buffer.from('legacy body'))
    const r4meta = await listEntriesDetailed(r4)
    assert.equal(r4meta[0].version, 'v29')

    // RAR4 rejects RAR5-only options up front.
    await assert.rejects(
      createArchive({
        outPath: join(dir, 'x.rar'),
        format: 'rar4',
        dictSize: '64m',
        entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('x') }],
      }),
      (error) => error.message.includes('dictionary'),
    )
    await assert.rejects(
      createArchive({
        outPath: join(dir, 'x.rar'),
        format: 'nope',
        entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('x') }],
      }),
      (error) => error.message.includes('nope'),
    )

    // RAR5 default stays RAR5.
    const r5 = join(dir, 'five.rar')
    await createArchive({
      outPath: r5,
      entries: [{ kind: 'bytes', name: 'a.txt', data: Buffer.from('five') }],
    })
    assert.equal((await listEntriesDetailed(r5))[0].version, 'v50')

    // Explicit rar7 forces v70 members at a small dictionary. Use
    // compressible data so the member takes the LZSS path (stored members
    // carry no compression and stay comp_version 0).
    const sevenPayload = Buffer.from('seven '.repeat(1000))
    const r7 = join(dir, 'seven.rar')
    await createArchive({
      outPath: r7,
      format: 'rar7',
      dictSize: '64m',
      entries: [{ kind: 'bytes', name: 'a.txt', data: sevenPayload }],
    })
    const r7meta = await listEntriesDetailed(r7)
    assert.equal(r7meta[0].version, 'v70')
    assert.equal(r7meta[0].dictSizeBytes, 128 * 1024, 'v70 declares a floor dict of 128 KiB')
    assert.deepEqual(await readMember(r7, 'a.txt'), sevenPayload)

    // rar7 accepts non-power-of-two dictionaries through 4 GiB (`-ma7
    // -md6m` semantics): the member must clear the 2x-file-size cap so the
    // full 6 MiB is declared.
    const bigPayload = Buffer.alloc(4 * 1024 * 1024, 0x61)
    const r7six = join(dir, 'seven6m.rar')
    await createArchive({
      outPath: r7six,
      format: 'rar7',
      dictSize: '6m',
      entries: [{ kind: 'bytes', name: 'a.bin', data: bigPayload }],
    })
    const r7sixMeta = await listEntriesDetailed(r7six)
    assert.equal(r7sixMeta[0].version, 'v70')
    assert.equal(r7sixMeta[0].dictSizeBytes, 6 * 1024 * 1024, '6 MiB declared exactly')
    assert.deepEqual(await readMember(r7six, 'a.bin'), bigPayload)

    // rar5 keeps the strict power-of-two rule (WinRAR rejects -md6m).
    await assert.rejects(
      createArchive({
        outPath: join(dir, 'x.rar'),
        format: 'rar5',
        dictSize: '6m',
        entries: [{ kind: 'bytes', name: 'a.bin', data: bigPayload }],
      }),
      (error) => error.message.includes('dictionary'),
    )
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('extractMember streams one member to a directory', async () => {
  const dir = tempDir()
  try {
    const payload = Buffer.from('extract-me '.repeat(1000))
    const out = join(dir, 'mm.rar')
    await createArchive({
      outPath: out,
      entries: [
        { kind: 'bytes', name: 'sub/target.txt', data: payload },
        { kind: 'bytes', name: 'other.txt', data: Buffer.from('unrelated') },
      ],
    })

    const { extractMember } = await import('../index.js')
    const dest = join(dir, 'out')
    const pending = extractMember(out, 'sub/target.txt', dest)
    assert.equal(typeof pending.then, 'function', 'extractMember must be async')
    const written = await pending
    const normalized = (p) => p.replaceAll('\\', '/')
    assert.equal(
      normalized(written),
      normalized(join(dest, 'sub', 'target.txt')),
      'returns the resolved path',
    )
    assert.deepEqual(readFileSync(written), payload, 'content round-trips')
    assert.equal(existsSync(join(dest, 'other.txt')), false, 'only the requested member')

    await assert.rejects(
      extractMember(out, 'missing.txt', dest),
      (error) => error.message.includes('missing.txt'),
    )

    // Cancellation aborts mid-extract with a cancellation error.
    const ctrl = new AbortController()
    const cancelled = extractMember(out, 'sub/target.txt', dest, undefined, ctrl.signal)
    ctrl.abort()
    await assert.rejects(cancelled, (error) => error.code === 'Cancelled')
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('extractArchive honors overwrite policies (skipExisting, autoRename)', async () => {
  const dir = tempDir()
  try {
    const out = join(dir, 'ow.rar')
    await createArchive({
      outPath: out,
      entries: [{ kind: 'bytes', name: 'notes/a.txt', data: Buffer.from('new conent ') }],
    })

    const { extractArchive, readMember } = await import('../index.js')

    // flat: true so members land directly in the destination (basename) and
    // the pre-seeded files below collide with the extraction targets.
    const flat = { flat: true }

    // Default: existing file is overwritten.
    const d1 = join(dir, 'd1')
    mkdirSync(d1)
    writeFileSync(join(d1, 'a.txt'), 'old content here')
    await extractArchive(out, { destPath: d1, ...flat })
    assert.equal(readFileSync(join(d1, 'a.txt'), 'utf8'), 'new conent ')
    await assert.rejects(
      readMember(out, 'notes/missing.txt'),
      (error) => error.message.includes('missing'),
    )

    // skipExisting: the existing file is left untouched.
    const d2 = join(dir, 'd2')
    mkdirSync(d2)
    writeFileSync(join(d2, 'a.txt'), 'keep me')
    await extractArchive(out, { destPath: d2, skipExisting: true, ...flat })
    assert.equal(readFileSync(join(d2, 'a.txt'), 'utf8'), 'keep me')

    // autoRename: collision produces a(1).txt next to the original.
    const d3 = join(dir, 'd3')
    mkdirSync(d3)
    writeFileSync(join(d3, 'a.txt'), 'original')
    await extractArchive(out, { destPath: d3, autoRename: true, ...flat })
    assert.equal(readFileSync(join(d3, 'a.txt'), 'utf8'), 'original')
    assert.equal(existsSync(join(d3, 'a(1).txt')), true, 'colliding member renamed')
    assert.equal(readFileSync(join(d3, 'a(1).txt'), 'utf8'), 'new conent ')
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})
