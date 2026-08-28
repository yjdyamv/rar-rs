'use strict'

// Host <-> WASI guest path mapping for the generated napi-rs loader.
//
// wasi-libc and uvwasi only understand absolute guest paths that start with
// '/'. The napi-rs default preopens the host drive root under its Windows
// spelling (e.g. 'C:\'), which the guest cannot match, so every absolute
// Windows path fails with ENOENT. We instead expose '/' (mapped to the
// current drive root) plus one '/X:' alias per existing drive, and translate
// host paths at the API boundary:
//
//   C:\Users\me\out.rar  ->  /C:/Users/me/out.rar
//
// On non-Windows hosts paths already start with '/', so they pass through
// unchanged and the loader keeps the default '/' -> '/' preopen.

const path = require('node:path')
const fs = require('node:fs')

const DRIVE_RE = /^([A-Za-z]):(?:([\\/].*))?$/
const DRIVE_HOST_RE = /^\/([A-Za-z]):(?:\/(.*))?$/

function toGuestPath(p, platform = process.platform) {
  if (typeof p !== 'string' || platform !== 'win32') return p
  const m = DRIVE_RE.exec(p)
  if (!m) return p
  const drive = m[1].toUpperCase()
  const rest = (m[2] || '').replace(/[\\/]+/g, '/').replace(/^\/+/, '')
  return rest ? `/${drive}:/${rest}` : `/${drive}:`
}

function toHostPath(p, platform = process.platform) {
  if (typeof p !== 'string' || platform !== 'win32') return p
  const m = DRIVE_HOST_RE.exec(p)
  if (!m) return p
  const drive = m[1].toUpperCase()
  const rest = (m[2] || '').replace(/\//g, '\\')
  return `${drive}:\\${rest}`
}

function wasiPreopens(
  rootDir,
  platform = process.platform,
  existsSync = fs.existsSync,
) {
  if (platform !== 'win32') {
    return { '/': '/' }
  }
  const preopens = { '/': rootDir }
  for (let i = 0; i < 26; i++) {
    const letter = String.fromCharCode(65 + i)
    const drive = `${letter}:\\`
    try {
      if (existsSync(drive)) {
        preopens[`/${letter}:`] = drive
      }
    } catch {
      // Skip drives that cannot be probed.
    }
  }
  return preopens
}

function mapCreateArchiveOptions(options, platform = process.platform) {
  if (!options || typeof options !== 'object') return options
  const mapped = { ...options }
  if (typeof mapped.outPath === 'string') {
    mapped.outPath = toGuestPath(mapped.outPath, platform)
  }
  if (Array.isArray(mapped.entries)) {
    mapped.entries = mapped.entries.map((entry) => {
      if (!entry || typeof entry !== 'object') return entry
      const e = { ...entry }
      if (typeof e.path === 'string') {
        e.path = toGuestPath(e.path, platform)
      }
      return e
    })
  }
  return mapped
}

function mapCreateResult(result, platform = process.platform) {
  if (!result || typeof result !== 'object' || !Array.isArray(result.files)) {
    return result
  }
  return {
    ...result,
    files: result.files.map((f) =>
      typeof f === 'string' ? toHostPath(f, platform) : f,
    ),
  }
}

function mapRepairArgs(
  inputPath,
  outputPath,
  platform = process.platform,
) {
  return [
    typeof inputPath === 'string' ? toGuestPath(inputPath, platform) : inputPath,
    typeof outputPath === 'string'
      ? toGuestPath(outputPath, platform)
      : outputPath,
  ]
}

function mapAppendOptions(options, platform = process.platform) {
  if (!options || typeof options !== 'object') return options
  const mapped = { ...options }
  if (typeof mapped.archivePath === 'string') {
    mapped.archivePath = toGuestPath(mapped.archivePath, platform)
  }
  if (Array.isArray(mapped.entries)) {
    mapped.entries = mapped.entries.map((entry) => {
      if (!entry || typeof entry !== 'object') return entry
      const e = { ...entry }
      if (typeof e.path === 'string') {
        e.path = toGuestPath(e.path, platform)
      }
      return e
    })
  }
  return mapped
}

function mapDeleteArgs(archivePath, names, password, platform = process.platform) {
  return [
    typeof archivePath === 'string'
      ? toGuestPath(archivePath, platform)
      : archivePath,
    names,
    password,
  ]
}

function mapListArgs(archivePath, password, platform = process.platform) {
  return [
    typeof archivePath === 'string'
      ? toGuestPath(archivePath, platform)
      : archivePath,
    password,
  ]
}

function mapExtractArgs(archivePath, options, platform = process.platform) {
  const mapped = { ...options }
  if (typeof mapped.destPath === 'string') {
    mapped.destPath = toGuestPath(mapped.destPath, platform)
  }
  return [
    typeof archivePath === 'string'
      ? toGuestPath(archivePath, platform)
      : archivePath,
    mapped,
  ]
}

module.exports = {
  toGuestPath,
  toHostPath,
  wasiPreopens,
  mapCreateArchiveOptions,
  mapCreateResult,
  mapRepairArgs,
  mapAppendOptions,
  mapDeleteArgs,
  mapListArgs,
  mapExtractArgs,
}
