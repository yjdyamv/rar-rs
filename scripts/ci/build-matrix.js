// The `build` job's matrix — one entry per published native addon.
//
// It is a file rather than a literal `matrix:` block because pull requests run
// a single entry (the one marked `smoke`) and tags run all nine, and the two selections
// have to come from the same list: a smoke target that drifted away from the
// one the release builds would rehearse a build nobody ships, and a target
// added here only (not to the list below) would be discovered by the release.
// The workflow picks the entries with `node -e` in the `build-matrix` job.
//
// Contract with the release: `scripts/ci/collect-release-assets.sh` lists the
// same nine `addon` names — and, on a `--cli` run, the `cli` subset — and fails
// if the downloaded artifacts do not match them exactly, so adding a platform
// means adding it here *and* there, which `scripts/ci/test-release-scripts.sh`
// keeps honest.
//
// `cli` names the per-platform archive that carries the `rar` and `unrar`
// executables from crates/rar-cli, and is empty on the rows whose target needs a
// fresh cross linker to build it: `aarch64-unknown-linux-gnu` and
// `armv7-unknown-linux-gnueabihf` are addon-only. The workflow's build and
// upload steps are gated on `cli != ''`, so the field must be present (empty,
// not absent) for that comparison to be a plain string test. The release job
// downloads the archives beside the addons and `collect-release-assets.sh
// --cli` copies them into the payload.
//
// Keep each `build` command identical to what a tag run must execute; this is
// the command, not a paraphrase of it.
module.exports = [
  {
    host: 'macos-latest',
    target: 'aarch64-apple-darwin',
    addon: 'rar-rs-napi.darwin-arm64.node',
    cli: 'rar-rs-cli.darwin-arm64.tar.gz',
    build: 'npx napi build --platform --release --target aarch64-apple-darwin --js binding.js --dts binding.d.ts',
  },
  {
    host: 'windows-latest',
    target: 'x86_64-pc-windows-msvc',
    addon: 'rar-rs-napi.win32-x64-msvc.node',
    cli: 'rar-rs-cli.win32-x64-msvc.tar.gz',
    build: 'npx napi build --platform --release --target x86_64-pc-windows-msvc --js binding.js --dts binding.d.ts',
  },
  {
    host: 'windows-latest',
    target: 'i686-pc-windows-msvc',
    addon: 'rar-rs-napi.win32-ia32-msvc.node',
    cli: 'rar-rs-cli.win32-ia32-msvc.tar.gz',
    build: 'npx napi build --platform --release --target i686-pc-windows-msvc --js binding.js --dts binding.d.ts',
  },
  {
    host: 'windows-latest',
    target: 'aarch64-pc-windows-msvc',
    addon: 'rar-rs-napi.win32-arm64-msvc.node',
    cli: 'rar-rs-cli.win32-arm64-msvc.tar.gz',
    build: 'npx napi build --platform --release --target aarch64-pc-windows-msvc --js binding.js --dts binding.d.ts',
  },
  {
    // The smoke entry: it runs on every pull request, so it is the napi
    // entry that is cheapest to build (a native target, no zig) and the one
    // that hosts the `tsc` type gate — `binding.d.ts` is generated for it and
    // compared against the hand-written `rar-rs.d.ts`.
    host: 'ubuntu-latest',
    target: 'x86_64-unknown-linux-gnu',
    addon: 'rar-rs-napi.linux-x64-gnu.node',
    cli: 'rar-rs-cli.linux-x64-gnu.tar.gz',
    build: 'npx napi build --platform --release --target x86_64-unknown-linux-gnu --use-napi-cross --js binding.js --dts binding.d.ts',
    smoke: true,
  },
  {
    host: 'ubuntu-latest',
    target: 'x86_64-unknown-linux-musl',
    addon: 'rar-rs-napi.linux-x64-musl.node',
    cli: 'rar-rs-cli.linux-x64-musl.tar.gz',
    build: 'npx napi build --platform --release --target x86_64-unknown-linux-musl -x --js binding.js --dts binding.d.ts',
  },
  {
    host: 'ubuntu-latest',
    target: 'aarch64-unknown-linux-gnu',
    addon: 'rar-rs-napi.linux-arm64-gnu.node',
    cli: '',
    build: 'npx napi build --platform --release --target aarch64-unknown-linux-gnu --use-napi-cross --js binding.js --dts binding.d.ts',
  },
  {
    host: 'ubuntu-latest',
    target: 'aarch64-unknown-linux-musl',
    addon: 'rar-rs-napi.linux-arm64-musl.node',
    cli: 'rar-rs-cli.linux-arm64-musl.tar.gz',
    build: 'npx napi build --platform --release --target aarch64-unknown-linux-musl -x --js binding.js --dts binding.d.ts',
  },
  {
    host: 'ubuntu-latest',
    target: 'armv7-unknown-linux-gnueabihf',
    addon: 'rar-rs-napi.linux-arm-gnueabihf.node',
    cli: '',
    build: 'npx napi build --platform --release --target armv7-unknown-linux-gnueabihf --use-napi-cross --js binding.js --dts binding.d.ts',
  },
];
