//! On-the-fly archive generation with the real WinRAR/RAR builds.
//!
//! The test suite no longer ships pre-built archives. Every test that needs an
//! archive written by official software asks this module instead, and the
//! module drives the matching `Rar.exe` from the local tool cache:
//!
//! * `$SA_WINRAR_CACHE` when set,
//! * otherwise `<workspace>/.cache/winrar/` (the development cache).
//!
//! Layout of the cache: one directory per release (`2-90`, `3-00`, `4-20`,
//! `5-91`, `6-23`, `7-23`, …), each holding the release's tools. Windows
//! installers ship `Rar.exe`, `UnRAR.exe`, `Default.SFX`; the Linux tarballs
//! ship `rar`, `unrar`, `default.sfx`, and both spellings are accepted.
//! `scripts/fetch-winrar.sh` fills the cache from rarlab.
//!
//! Generation is deliberately boring: one fresh directory per request, fixed
//! deterministic payloads, no shell, stdout captured into panic messages. The
//! same request served twice inside one test binary reuses the first result.
//!
//! When the cache is absent the helper returns `None` and the test returns
//! early — except under `SA_REQUIRE_WINRAR=1`, where the missing tool is a hard
//! failure so CI can prove the interop suite actually ran.

#![allow(dead_code)] // each test binary links the whole module and uses a subset

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

/// A cached official release, addressed by its directory name (e.g. `"3-00"`).
pub struct Tool {
    pub version: String,
    pub dir: PathBuf,
    rar: PathBuf,
    unrar: PathBuf,
}

/// First of `names` that exists in `dir`.
///
/// The Windows installers and the Linux tarballs spell the binaries
/// differently, so every lookup has to try both.
fn find_tool(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

impl Tool {
    pub fn rar(&self) -> &Path {
        &self.rar
    }

    pub fn unrar(&self) -> &Path {
        &self.unrar
    }

    /// `-sfx` needs the matching stub next to the tool.
    pub fn sfx(&self) -> Option<PathBuf> {
        find_tool(&self.dir, &["Default.SFX", "default.sfx"])
    }
}

/// A generated archive plus the directory that holds it.
pub struct Generated {
    pub dir: PathBuf,
    pub archive: PathBuf,
    /// Every file the tool wrote, e.g. the volumes of a multi-volume set.
    pub files: Vec<PathBuf>,
}

impl Generated {
    pub fn path(&self) -> &Path {
        &self.archive
    }

    /// Volume names produced by the run, sorted (`arc.rar`, `arc.r00`, …).
    pub fn names(&self) -> Vec<String> {
        self.files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    pub fn read(&self) -> Vec<u8> {
        std::fs::read(&self.archive).expect("read generated archive")
    }
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<workspace>/crates/rar`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

pub fn cache_root() -> PathBuf {
    match std::env::var_os("SA_WINRAR_CACHE") {
        Some(p) => PathBuf::from(p),
        None => workspace_root().join(".cache").join("winrar"),
    }
}

/// Every cached release with a usable tool pair, oldest first.
///
/// The version directory name is the key everywhere (`2-90`, `6-23`, …), so this
/// is what suites iterate over instead of scanning the filesystem themselves
/// and hard-coding a binary name that only exists on one platform.
pub fn cached_versions() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(cache_root()) else {
        return Vec::new();
    };
    let mut versions: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            let dir = entry.path();
            find_tool(&dir, &["Rar.exe", "rar"]).is_some()
                && find_tool(&dir, &["UnRAR.exe", "unrar"]).is_some()
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    versions.sort_by_key(|version| {
        let mut parts = version.split('-');
        let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        (major, minor)
    });
    versions
}

/// Look up a cached release; `None` when the cache (or that version) is absent.
pub fn tool(version: &str) -> Option<Tool> {
    let dir = cache_root().join(version);
    let rar = find_tool(&dir, &["Rar.exe", "rar"])?;
    let unrar = find_tool(&dir, &["UnRAR.exe", "unrar"])?;
    Some(Tool {
        version: version.to_owned(),
        dir,
        rar,
        unrar,
    })
}

fn require_mode() -> bool {
    std::env::var("SA_REQUIRE_WINRAR")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Skip policy for the "generation failed" case. `Rar.exe` refusing a switch is
/// normal for old releases, so it skips by default and fails only on demand.
fn skip_fail(version: &str, why: &str) {
    if require_mode() {
        panic!("SA_REQUIRE_WINRAR=1 but {version} could not generate the archive: {why}");
    }
    eprintln!("SKIP: {version} could not generate the requested archive: {why}");
}

fn skip(version: &str, why: &str) -> Option<Generated> {
    if require_mode() {
        panic!(
            "SA_REQUIRE_WINRAR=1 but the {version} tool is unusable ({why}); \
             populate {} or unset SA_REQUIRE_WINRAR",
            cache_root().display()
        );
    }
    eprintln!("SKIP: no {version} in {} ({why})", cache_root().display());
    None
}

/// Per-run scratch directory shared by every generator in the binary.
pub fn scratch_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "rar-rs-generated-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create scratch root");
        dir
    })
    .clone()
}

/// File-system-safe prefix for a generation key.
pub fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Deterministic payload bytes for a member name.
///
/// Tests assert extracted bytes against the same call, so the payloads are the
/// single source of truth for "what the archive contains".
pub fn payload(name: &str) -> Vec<u8> {
    match name {
        "tiny.txt" => b"hello\n".to_vec(),
        "empty.txt" => Vec::new(),
        "text.txt" => text(64 * 1024),
        "noise.bin" => noise(0x1234_5678, 32 * 1024),
        "code.exe" => code(24 * 1024),
        "chan.bin" => channels(64 * 1024),
        "audio.raw" => audio(80 * 1024),
        "img.bmp" => bitmap(64, 64),
        "itan.bin" => itanium(16 * 1024),
        "comment.txt" => b"archive comment\n".to_vec(),
        // Not representable in an OEM code page, so the writer has to store the
        // comment as UTF-16LE.
        "comment_zh.txt" => "第二段注释 ünï\n".as_bytes().to_vec(),
        // Corpus members whose logical content the read suites pin.
        "repeat.txt" => repeat(4000),
        "random.bin" => noise(0x1234_5678, 64 * 1024),
        "a.txt" => b"text-only content line one\n".to_vec(),
        "b.txt" => b"second file content here\n".to_vec(),
        "hello.txt" => b"store hello content".to_vec(),
        "longpw.txt" => b"rar-rs long password fixture\n".to_vec(),
        "README.md" => text(4 * 1024),
        "bigtext_64k.bin" => text(64 * 1024),
        // RAR5 inline-recovery-record cross-validation members: a long text
        // with a recognisable header line and a fixed-size binary blob.
        "doc.txt" => {
            let mut doc =
                b"RAR5 recovery record cross-validation fixture generated by WinRAR RAR 7.23. "
                    .to_vec();
            doc.extend(text(304_000));
            doc.truncate(304_000);
            doc
        }
        "data.bin" => noise(0x5EED_1234, 64 * 1024),
        "lorem_127k.txt" => text(127 * 1024),
        "audio.wav" => audio(80 * 1024),
        "grad.bmp" => bitmap(256, 256),
        "FILE1.TXT" => b"file1\r\n".to_vec(),
        "FILE2.TXT" => b"file2\r\n".to_vec(),
        "SOLID1.TXT" => text(2_700),
        "SOLID2.TXT" => text(2_700),
        "binary_64k.bin" => noise(0x9E37_79B9, 64 * 1024),
        // Incompressible and larger than the split size, so volume sets and
        // recovery volumes really split.
        "big.bin" => noise(0x1234_5678, 200_000),
        // Four distinguishable members for the RAR5 read checks: two
        // compressible, two noise-like.
        "test1.bin" => noise(0x0000_0001, 40_000),
        "test2.bin" => text(40_000),
        "test3.bin" => noise(0x0000_0003, 24_000),
        "test4.bin" => phrase(24_000),
        // The NTFS alternate data stream `-os` picks up; it only exists where
        // the filesystem supports one, so the recipe is Windows-only.
        "data.txt" => b"plain member carrying an NTFS stream\n".to_vec(),
        "data.txt:meta" => b"stream-payload-abcdefghij\r\n".to_vec(),
        "escape_64k.bin" => channels(64 * 1024),
        "repeated_phrase_64k.txt" => phrase(64 * 1024),
        "中文.txt" => "中文内容，用于头加密测试。\n".repeat(64).into_bytes(),
        name if doc_member(name) => text(3 * 1024),
        other => panic!("no deterministic payload named {other:?}"),
    }
}

/// Word-soup text: repetitive enough for PPMd/LZ, varied enough to be non-trivial.
fn text(len: usize) -> Vec<u8> {
    const WORDS: [&str; 12] = [
        "the ",
        "quick ",
        "brown ",
        "fox ",
        "jumps ",
        "over ",
        "lazy ",
        "dog ",
        "0123456789 ",
        "rar ",
        "archive ",
        "compression ",
    ];
    let mut out = Vec::with_capacity(len + 64);
    let mut x: u32 = 0x2545_F491;
    let mut linenum = 1;
    while out.len() < len {
        out.extend_from_slice(linenum.to_string().as_bytes());
        out.push(b' ');
        for _ in 0..8 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            out.extend_from_slice(WORDS[((x >> 16) as usize) % WORDS.len()].as_bytes());
        }
        out.push(b'\n');
        linenum += 1;
    }
    out.truncate(len);
    out
}

/// The same sentence repeated: highly repetitive, so LZ wins over PPMd.
fn repeat(lines: usize) -> Vec<u8> {
    b"The quick brown fox jumps over the lazy dog. 0123456789\n".repeat(lines)
}

/// The 17-file documentation set the legacy suites read as one group.
fn doc_member(name: &str) -> bool {
    name.strip_prefix("doc_")
        .and_then(|r| r.strip_suffix(".txt"))
        .is_some_and(|n| n.parse::<u8>().is_ok())
}

/// One phrase repeated with a varying tail: PPMd's LZ-match pressure point.
fn phrase(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 64);
    let mut i = 0u32;
    while out.len() < len {
        out.extend_from_slice(b"repeat this phrase ");
        out.extend_from_slice(i.to_string().as_bytes());
        out.push(b'\n');
        i += 1;
    }
    out.truncate(len);
    out
}

fn noise(seed: u32, len: usize) -> Vec<u8> {
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.push((x >> 8) as u8);
    }
    out
}

/// x86-ish stream: dense `E8 rel32` calls interleaved with ASCII runs, which is
/// what makes WinRAR emit the x86 filter and its distance-conversion records.
fn code(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut x: u32 = 0x0BAD_F00D;
    while out.len() < len {
        out.extend_from_slice(b"\x55\x8b\xec\x90");
        out.extend_from_slice(b"\xe8");
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        out.extend_from_slice(&(x >> 4 | 0x1000).to_le_bytes());
        out.extend_from_slice(b"\x90\x90\x5d\xc3");
        out.extend_from_slice(b"pointer %d and string data follow ");
        out.extend_from_slice(b"\xe8\x00\x00\x00\x00\x90");
        out.extend_from_slice(b"ANOTHER TEXT BLOCK with mixed case and digits 42 ");
    }
    out.truncate(len);
    out
}

/// 4-byte interleaved channels: the delta filter's favourite shape.
fn channels(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i / 4) + (i % 4) * 7) as u8).collect()
}

/// 16-bit stereo with slowly drifting saw-like channels: audio filter input.
fn audio(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 4);
    let mut l: i16 = 0;
    let mut r: i16 = 0;
    for i in 0..len / 4 {
        l = l.wrapping_add(((i % 220) as i16) * 3 - 300);
        r = r.wrapping_sub(((i % 180) as i16) * 2 - 150);
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out
}

/// 24-bit BMP-like raster with flat regions and gradients.
fn bitmap(w: usize, h: usize) -> Vec<u8> {
    let row = w * 3;
    let mut out = Vec::with_capacity(54 + row * h);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + row * h) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    out.extend_from_slice(&(w as u32).to_le_bytes());
    out.extend_from_slice(&(h as u32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    for _ in 0..2 {
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&row.to_le_bytes());
        out.extend_from_slice(&[0; 8]);
    }
    for y in 0..h {
        for x in 0..w {
            let b = ((x * 4) % 256) as u8;
            let g = ((y * 3) % 256) as u8;
            let r = (((x / 8 + y / 8) % 16) * 16) as u8;
            out.extend_from_slice(&[b, g, r]);
        }
    }
    out
}

/// Itanium-shaped stream: `0f 01`/`0f 02` bundles, for the Itanium filter.
fn itanium(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 16);
    let mut i: u32 = 0;
    while out.len() < len {
        out.extend_from_slice(b"\x0f\x01");
        out.extend_from_slice(&(i * 4).to_le_bytes());
        out.extend_from_slice(b"\x0f\x02\x00\x00\x00\x00\x90\x90");
        i += 1;
    }
    out.truncate(len);
    out
}

// ── generation ─────────────────────────────────────────────────────────────

/// Run `Rar.exe` from `version` over `members` (deterministic payloads) with
/// `switches`, returning the produced archive.
///
/// `None` means "this checkout cannot generate that": the tool is missing, or
/// the payloads do not exist. Callers return early.
pub fn archive(version: &str, switches: &[&str], members: &[&str]) -> Option<Generated> {
    let Some(tool) = tool(version) else {
        return skip(version, "tool directory not present");
    };
    match generate(&tool, switches, members, None, &[]) {
        Ok(made) => Some(made),
        Err(e) => {
            skip_fail(version, &e);
            None
        }
    }
}

/// Like [`archive`], then replay each `post` verb against the first volume the
/// add step produced — that is how the recovery-volume sets (`.rev`), which are
/// a second `Rar.exe rv3` pass, get built.
pub fn archive_then(
    version: &str,
    switches: &[&str],
    members: &[&str],
    post: &[&[&str]],
) -> Option<Generated> {
    let Some(tool) = tool(version) else {
        return skip(version, "tool directory not present");
    };
    match generate(&tool, switches, members, None, post) {
        Ok(made) => Some(made),
        Err(e) => {
            skip_fail(version, &e);
            None
        }
    }
}

/// Like [`archive`] but reports every failure instead of skipping, for tests
/// that *assert* a combination is unsupported.
pub fn attempt(version: &str, switches: &[&str], members: &[&str]) -> Result<Generated, String> {
    let tool = tool(version).ok_or_else(|| format!("no {version} tool"))?;
    generate(&tool, switches, members, None, &[])
}

/// Same, but the archive is written next to `extra` files copied into the
/// working directory first (e.g. a comment file consumed by `-z`).
pub fn archive_with(
    version: &str,
    switches: &[&str],
    members: &[&str],
    extra: &[&str],
) -> Option<Generated> {
    let Some(tool) = tool(version) else {
        return skip(version, "tool directory not present");
    };
    match generate(&tool, switches, members, Some(extra), &[]) {
        Ok(made) => Some(made),
        Err(e) => {
            skip_fail(version, &e);
            None
        }
    }
}

fn generate(
    tool: &Tool,
    switches: &[&str],
    members: &[&str],
    extra: Option<&[&str]>,
    post: &[&[&str]],
) -> Result<Generated, String> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    static CACHE: OnceLock<Mutex<HashMap<String, Generated>>> = OnceLock::new();
    let key = format!(
        "{}|{}|{}|{}",
        tool.version,
        switches.join(" "),
        members.join(","),
        post.iter()
            .map(|cmd| cmd.join(" "))
            .collect::<Vec<_>>()
            .join(";")
    );
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if extra.is_none()
        && let Some(hit) = cache.lock().unwrap().get(&key)
    {
        return Ok(clone_archive(hit));
    }
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = scratch_root().join(format!("{}-{:04}", slug(&key), n));
    std::fs::create_dir_all(&dir).expect("create generation dir");
    for m in members {
        std::fs::write(dir.join(m), payload(m)).expect("write payload");
    }
    for m in extra.unwrap_or(&[]) {
        std::fs::write(dir.join(m), payload(m)).expect("write extra payload");
    }
    let before: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    let mut cmd = Command::new(tool.rar());
    cmd.arg("a").arg("-y");
    cmd.args(switches);
    cmd.arg("arc.rar");
    cmd.args(members);
    cmd.current_dir(&dir);
    let out = cmd.output().expect("run Rar.exe");
    if !out.status.success() {
        return Err(format!(
            "Rar.exe {} {switches:?} failed with {}: {}",
            tool.version,
            out.status,
            String::from_utf8_lossy(&out.stdout).trim()
        ));
    }
    if !post.is_empty() {
        // The add step produced the volume set; `rv`/`rc` act on its first
        // volume, and the split size has to be repeated on the command line.
        let mut created: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !before.contains(&name)
                    && e.path()
                        .extension()
                        .is_some_and(|x| x.eq_ignore_ascii_case("rar"))
                {
                    created.push(e.path());
                }
            }
        }
        created.sort();
        let first = created
            .into_iter()
            .next()
            .ok_or_else(|| format!("{} produced no volume to post-process", tool.version))?;
        for verb in post {
            let step = Command::new(tool.rar())
                .args(*verb)
                .arg(&first)
                .current_dir(&dir)
                .output()
                .expect("run Rar.exe post step");
            if !step.status.success() {
                return Err(format!(
                    "Rar.exe {} {verb:?} failed with {}: {}",
                    tool.version,
                    step.status,
                    String::from_utf8_lossy(&step.stdout).trim()
                ));
            }
        }
    }
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !before.contains(&name) {
                files.push(e.path());
            }
        }
    }
    files.sort();
    let archive = files
        .iter()
        .find(|p| {
            let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase());
            matches!(ext.as_deref(), Some("rar") | Some("r00") | Some("exe"))
        })
        .cloned();
    let archive = archive.ok_or_else(|| {
        format!(
            "Rar.exe {version} {switches:?} produced no archive: {}",
            String::from_utf8_lossy(&out.stdout).trim(),
            version = tool.version
        )
    })?;
    let made = Generated {
        dir,
        archive,
        files,
    };
    // `extra` files are not part of the key, so only cache keyless runs.
    if extra.is_none() {
        cache.lock().unwrap().insert(key, clone_archive(&made));
    }
    Ok(made)
}

/// The cached release that covers each legacy archive generation.
///
/// Policy: each archive version is produced by the *newest* cached release that
/// still supports it. That keeps the fixtures on the most-maintained writer of
/// each generation instead of freezing one arbitrary old build, and it makes the
/// version list authoritative — when a future release drops a generation, only
/// these constants move.
///
/// | archive version | writer (newest that supports it)                 |
/// |-----------------|--------------------------------------------------|
/// | v14 (RAR 1.3/1.4, `RE~^`) | our writer (`-ma14`); no release ever wrote it |
/// | v15 (RAR 1.5)   | our writer (`-ma15`); no release ever wrote it   |
/// | v20 (RAR 2.0)   | `-ma4 -m0` on 6-23; compressed v20 only from 2-90 |
/// | v26             | v20 bytes with `unp_ver` 26 ([`patch_unp_ver`])  |
/// | v29 (RAR 3.x/4.x) | `-ma4` on 6-23 (7-23 dropped `-ma4`)           |
/// | v36             | v29 bytes with `unp_ver` 36                      |
/// | v50 (RAR5)      | `-ma5` on 7-23                                   |
/// | v70 (RAR7 dict) | our writer forces `comp_version 1`               |
///
/// v26/v36 are read-only aliases of the v20/v29 codec (`version.rs`), so no
/// release ever wrote them; declaring the byte on a same-codec archive is the
/// only faithful way to exercise the alias.
///
/// v70 is out of tool reach at test scale: a release only sets `comp_version 1`
/// once the member dictionary passes 4 GiB, and the dictionary request is capped
/// at twice the member size, so WinRAR 7.23 would need a >2 GiB input to emit
/// one.
/// `6-23` is the newest release with RAR4 output; `-m0` is the only route to a
/// declared `unp_ver 20` there, so a compressed v20 stream still comes from the
/// 2.x line (see [`TOOL_V20_COMPRESSED`]).
pub const TOOL_RAR20: &str = "6-23";
/// Newest release that still compresses the RAR 2.0 codec (no `-ma` support).
pub const TOOL_V20_COMPRESSED: &str = "2-90";
pub const TOOL_RAR29: &str = "6-23";
pub const TOOL_RAR50: &str = "7-23";

/// Rewrite a legacy member's `unp_ver` byte, re-stamping the header CRC so the
/// archive stays valid. Used for the read-only version aliases (v26/v36).
pub fn patch_unp_ver(bytes: &[u8], to: u8) -> Vec<u8> {
    let mut out = bytes.to_vec();
    if out.len() < 7 || &out[..7] != b"Rar!\x1a\x07\x00" {
        panic!("patch_unp_ver needs a RAR 1.5-4.x archive");
    }
    let mut p = 7;
    while p + 7 <= out.len() {
        let size = usize::from(u16::from_le_bytes([out[p + 5], out[p + 6]]));
        if size < 7 || p + size > out.len() {
            break;
        }
        if out[p + 2] == 0x74 {
            assert!(size > 24, "file header too small for unp_ver");
            out[p + 24] = to;
            let crc = (crc32(&out[p + 2..p + size]) & 0xffff) as u16;
            out[p..p + 2].copy_from_slice(&crc.to_le_bytes());
            return out;
        }
        p += size;
    }
    panic!("no file header found to patch");
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}

fn clone_archive(g: &Generated) -> Generated {
    Generated {
        dir: g.dir.clone(),
        archive: g.archive.clone(),
        files: g.files.clone(),
    }
}

/// Cross-check an archive with the official extractor of `version`; `Err` carries
/// the tool's output so a failing assertion explains itself, `None` means the
/// tool is not cached here (the caller skips).
pub fn unrar_test(version: &str, archive: &Path) -> Option<Result<(), String>> {
    let tool = tool(version)?;
    let out = Command::new(tool.unrar())
        .arg("t")
        .arg("-idq")
        .arg(archive)
        .output()
        .expect("run UnRAR.exe");
    if out.status.success() {
        Some(Ok(()))
    } else {
        Some(Err(format!(
            "UnRAR.exe {version} rejected {}: {}\n{}",
            archive.display(),
            out.status,
            String::from_utf8_lossy(&out.stdout)
        )))
    }
}
