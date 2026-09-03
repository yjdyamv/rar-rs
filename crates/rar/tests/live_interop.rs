//! Live interoperability tests against *genuine reference tools* fetched at
//! test time — no committed fixture archives.
//!
//! The goal (see PLAN.md "老版本 RAR"): parity with WinRAR 5.x for legacy
//! RAR exactly like the RAR5 ↔ WinRAR 7.23 parity. WinRAR 7.x cannot create
//! RAR4 (`-ma4` was removed), so old-RAR *generation* needs WinRAR 5.91
//! (Windows, or wine); on other platforms those scenarios are skipped and
//! the RAR5-parity ones run against whatever RAR/UnRAR the environment
//! provides.
//!
//! Tool resolution (in order):
//!   1. `RAR_TOOLS_DIR` — a directory containing `winrar591/`, `winrar7/`
//!      and/or `7z/` (each with the platform's binaries, e.g. `Rar.exe` +
//!      `UnRAR.exe` on Windows, `rar` + `unrar` on unix);
//!   2. known local installs (7-Zip ZS / 7-Zip in PATH or common dirs);
//!   3. when `RAR_LIVE_DOWNLOAD=1`, fetch on demand: 7-Zip ZS latest from
//!      the mcmilk GitHub release (silent-install the Windows exe into the
//!      cache), then WinRAR 5.91 from rarlab and extract it with 7-Zip.
//!
//! Everything caches under `$RAR_CACHE_DIR` (default: system temp +
//! `rar-rs-interop`). Tests self-skip with a clear reason when a tool is
//! unavailable; set `RAR_LIVE_INTEROP=1` to run (tests stay off by default
//! so plain `cargo test` needs no network).
//!
//! Platform notes: generation of RAR4 archives is Windows-only with the
//! official 5.91 build; extraction/listing parity is cross-platform.

use sha2::Digest;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── gating ────────────────────────────────────────────────────────────────

fn live_enabled() -> bool {
    std::env::var("RAR_LIVE_INTEROP").is_ok_and(|v| v != "0" && v != "false")
}

fn skip(reason: &str) {
    eprintln!("live_interop: SKIP: {reason}");
}

// ── tool resolution ────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Tools {
    /// Directory containing `Rar.exe`/`UnRAR.exe` (win) or `rar`/`unrar`.
    rar591: Option<PathBuf>,
    /// RAR 7.x binaries (same layout). Reserved for RAR5-parity scenarios.
    #[allow(dead_code)]
    rar7: Option<PathBuf>,
    /// `7z`/`7zz` executable (used to self-extract installers).
    #[allow(dead_code)]
    seven_zip: Option<PathBuf>,
}

fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RAR_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    std::env::temp_dir().join("rar-rs-interop")
}

fn which(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.is_file()).cloned()
}

fn bin_names() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("Rar.exe", "UnRAR.exe")
    } else {
        ("rar", "unrar")
    }
}

fn common_7z_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            for name in ["7z", "7zz", "7za", "7z.exe", "7zz.exe"] {
                out.push(dir.join(name));
            }
        }
    }
    for fixed in [
        "C:\\Program Files\\7-Zip-Zstandard\\7z.exe",
        "C:\\Program Files\\7-Zip\\7z.exe",
        "/usr/bin/7z",
        "/usr/local/bin/7z",
        "/opt/homebrew/bin/7z",
    ] {
        out.push(PathBuf::from(fixed));
    }
    out
}

fn resolve_tools() -> Tools {
    let (rar_bin, _unrar_bin) = bin_names();

    // Explicit single-root layout: RAR_TOOLS_DIR/{winrar591,winrar7,7z}.
    let mut rar591 = None;
    let mut rar7 = None;
    let mut seven_zip = None;
    if let Some(root) = std::env::var_os("RAR_TOOLS_DIR") {
        let root = PathBuf::from(root);
        rar591 = which(&[root.join("winrar591").join(rar_bin)]);
        rar7 = which(&[root.join("winrar7").join(rar_bin)]);
        seven_zip = which(&[
            root.join("7z").join("7z.exe"),
            root.join("7z").join("7z"),
            root.join("7z").join("7zz"),
        ]);
    }

    if rar591.is_none() {
        rar591 = std::env::var_os("RAR_591_DIR")
            .map(PathBuf::from)
            .and_then(|d| which(&[d.join(rar_bin)]));
    }
    if rar591.is_none() {
        // Local dev convenience (this repo's reference setup).
        for home in [std::env::var("USERPROFILE"), std::env::var("HOME")]
            .into_iter()
            .flatten()
        {
            let cand = PathBuf::from(&home).join("Desktop").join("winrar591");
            if let Some(exe) = which(&[cand.join(rar_bin)]) {
                rar591 = Some(exe);
                break;
            }
        }
    }
    if rar7.is_none() {
        rar7 = std::env::var_os("RAR_7_DIR")
            .map(PathBuf::from)
            .and_then(|d| which(&[d.join(rar_bin)]));
    }
    if seven_zip.is_none() {
        seven_zip = which(&common_7z_paths());
    }
    if seven_zip.is_none() && std::env::var("RAR_LIVE_DOWNLOAD").is_ok_and(|v| v == "1") {
        seven_zip = fetch_7z_zs();
    }
    if rar591.is_none()
        && std::env::var("RAR_LIVE_DOWNLOAD").is_ok_and(|v| v == "1")
        && let Some(sz) = &seven_zip
    {
        rar591 = fetch_winrar591(sz);
    }

    Tools {
        rar591,
        rar7,
        seven_zip,
    }
}

// ── on-demand fetching (Windows-oriented; other platforms should point the
//    env vars at local or wine-provided tools) ─────────────────────────────

fn http_get(url: &str, dest: &Path) -> bool {
    let mut cmd = Command::new("curl");
    cmd.args(["-sSL", "--max-time", "300", "-o"])
        .arg(dest)
        .arg(url);
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Download 7-Zip ZS (mcmilk) latest Windows build and silent-install it
/// into the cache so it can self-extract WinRAR installers.
fn fetch_7z_zs() -> Option<PathBuf> {
    let cache = cache_dir();
    std::fs::create_dir_all(&cache).ok()?;
    let listing = Command::new("curl")
        .args(["-sSL", "--max-time", "60"])
        .arg("https://api.github.com/repos/mcmilk/7-Zip-zstd/releases/latest")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&listing.stdout);
    let asset = text
        .lines()
        .find(|l| l.contains("\"name\":") && l.contains("zstd-x64.exe"))?
        .split('"')
        .nth(3)?;
    let exe = cache.join(asset);
    if !exe.exists()
        && !http_get(
            &format!("https://github.com/mcmilk/7-Zip-zstd/releases/latest/download/{asset}"),
            &exe,
        )
    {
        return None;
    }
    let dir = cache.join("7z-zs-install");
    let status = Command::new(&exe)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        .arg(format!("/DIR={}", dir.display()))
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    which(&[dir.join("7z.exe")])
}

/// Download WinRAR 5.91 (the last release able to create RAR4) and extract
/// the portable build from its installer with 7-Zip.
fn fetch_winrar591(seven_zip: &Path) -> Option<PathBuf> {
    let cache = cache_dir();
    std::fs::create_dir_all(&cache).ok()?;
    let exe = cache.join("winrar-x64-591.exe");
    if !exe.exists()
        && !exe.exists()
        && !http_get("https://www.rarlab.com/rar/winrar-x64-591.exe", &exe)
    {
        return None;
    }
    let out = cache.join("winrar591");
    std::fs::create_dir_all(&out).ok()?;
    let _ = Command::new(seven_zip)
        .args(["x", "-y", "-o"])
        .arg(&out)
        .arg(&exe)
        .status();
    which(&[out.join("Rar.exe")])
}

// ── command helpers ────────────────────────────────────────────────────────

struct RunResult {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run(mut cmd: Command) -> RunResult {
    let out = cmd.output().expect("spawn command");
    RunResult {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn sha256_hex(path: &Path) -> String {
    let data = std::fs::read(path).expect("read file for hash");
    let mut h = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut h, &data);
    let out = h.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

fn temp_dir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("rar-rs-live-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&base).expect("temp dir");
    base
}

/// Deterministic corpus: two text files, one random binary, one audio-like
/// ramp (delta-friendly). Returns (dir, {name: bytes}).
fn corpus(dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    use std::io::Write;
    let mut files = std::collections::BTreeMap::new();
    let mut prose = String::new();
    for i in 0..1200 {
        prose.push_str(&format!(
            "Paragraph {i}: the quick brown fox jumps over the lazy dog while \
             pack my box with five dozen liquor jugs. Sphinx of black quartz \
             judge my vow. {}\n",
            "x".repeat(i % 40)
        ));
    }
    files.insert("prose.txt".into(), prose.into_bytes());

    let mut audio = Vec::new();
    for i in 0..40_000u32 {
        let v = ((i * 7919) % 256) as u8;
        audio.push(v);
        audio.push(v.wrapping_sub(40));
    }
    files.insert("ramp.raw".into(), audio);

    let mut rng: u64 = 0x9e3779b97f4a7c15;
    let mut random = Vec::with_capacity(48 * 1024);
    for _ in 0..48 * 1024 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        random.push((rng >> 32) as u8);
    }
    files.insert("random.bin".into(), random);

    for (name, bytes) in &files {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
    }
    files
}

// ── assertion helpers shared by the scenarios ─────────────────────────────

/// Read a member from a rar-rs-opened archive and return its sha256.
fn our_member_sha(archive_path: &Path, member: &str, password: Option<&str>) -> String {
    let opened = match password {
        Some(pw) => rar5::RarArchive::open_with_password(archive_path, pw),
        None => rar5::RarArchive::open(archive_path),
    };
    let mut archive = opened.expect("our open");
    let data = archive.read(member).expect("our read");
    let mut h = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut h, &data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn unrar_extract_sha(unrar: &Path, archive: &Path, member: &str, password: Option<&str>) -> String {
    let work = temp_dir("unrar");
    let mut cmd = Command::new(unrar);
    cmd.arg("x").arg("-y").arg("-idq").arg("-o+");
    if let Some(pw) = password {
        cmd.arg(format!("-p{pw}"));
    }
    cmd.arg(archive).arg(member).current_dir(&work);
    let res = run(cmd);
    assert!(
        res.status == 0,
        "unrar failed: {} {}",
        res.stdout,
        res.stderr
    );
    sha256_hex(&work.join(member))
}

// ── scenarios ──────────────────────────────────────────────────────────────

fn scenario_rar591_created_rar4_reads_identically(tools: &Tools) {
    let rar591 = tools.rar591.as_ref().expect("winrar591 present");
    let unrar591 = rar591
        .parent()
        .unwrap()
        .join(if cfg!(windows) { "UnRAR.exe" } else { "unrar" });

    // (method, password, solid) grid — every combination we can generate
    // with -ma4 must be readable byte-identically by our RAR4 reader.
    let cases: &[(&str, Option<&str>, bool)] = &[
        ("m0", None, false),
        ("m3", None, false),
        ("m5", None, false),
        ("m3", None, true),
        ("m3", Some("secret"), false),
    ];
    for (method, password, solid) in cases {
        let work = temp_dir(&format!("gen-{method}"));
        let files = corpus(&work);
        let archive = work.join("out.rar");

        let mut cmd = Command::new(rar591);
        cmd.arg("a").arg("-ma4").arg(format!("-m{}", &method[1..]));
        if *solid {
            cmd.arg("-s");
        }
        if let Some(pw) = password {
            cmd.arg(format!("-p{pw}"));
        }
        cmd.arg("-ep1").arg("-idq").arg(&archive);
        cmd.arg(work.join("*"));
        let res = run(cmd);
        assert!(res.status == 0, "rar591 create failed: {}", res.stderr);

        // Every member: our decode == rar591's own unrar decode.
        for name in files.keys() {
            let expected = unrar_extract_sha(&unrar591, &archive, name, *password);
            let actual = our_member_sha(&archive, name, *password);
            assert_eq!(
                actual, expected,
                "rar4 {method} solid={solid} pw={password:?} member {name}"
            );
        }
        let _ = std::fs::remove_dir_all(&work);
    }
}

fn scenario_header_encrypted_rar4(tools: &Tools) {
    let rar591 = tools.rar591.as_ref().expect("winrar591");
    let work = temp_dir("hp");
    let files = corpus(&work);
    let archive = work.join("hp.rar");
    let mut cmd = Command::new(rar591);
    cmd.args(["a", "-ma4", "-m3", "-hpsecret", "-ep1", "-idq"])
        .arg(&archive)
        .arg(work.join("prose.txt"));
    let res = run(cmd);
    assert!(res.status == 0, "rar591 -hp create failed: {}", res.stderr);

    // Listing needs the password at open time.
    assert!(
        rar5::RarArchive::open(&archive).is_err(),
        "-hp must refuse to open without a password"
    );
    let expected = unrar_extract_sha(
        &rar591
            .parent()
            .unwrap()
            .join(if cfg!(windows) { "UnRAR.exe" } else { "unrar" }),
        &archive,
        "prose.txt",
        Some("secret"),
    );
    let actual = our_member_sha(&archive, "prose.txt", Some("secret"));
    assert_eq!(actual, expected, "-hp member bytes");
    let _ = files;
    let _ = std::fs::remove_dir_all(&work);
}

fn scenario_multivol_rar4(tools: &Tools) {
    let rar591 = tools.rar591.as_ref().expect("winrar591");
    let work = temp_dir("mvol");
    let files = corpus(&work);
    let archive = work.join("big.rar");
    // -v100k forces at least two volumes; keep it small & fast.
    let mut cmd = Command::new(rar591);
    cmd.args(["a", "-ma4", "-m3", "-ep1", "-idq", "-v100k"])
        .arg(&archive)
        .arg(work.join("prose.txt"));
    let res = run(cmd);
    assert!(res.status == 0, "rar591 -v create failed: {}", res.stderr);

    let mut first_vol = None;
    for entry in std::fs::read_dir(&work).unwrap() {
        let p = entry.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if name.ends_with(".rar") && (name.contains(".part1.") || !name.contains(".part")) {
            first_vol = Some(p);
            break;
        }
    }
    let first_vol = first_vol.expect("volume 1 exists");
    let expected = unrar_extract_sha(
        &rar591
            .parent()
            .unwrap()
            .join(if cfg!(windows) { "UnRAR.exe" } else { "unrar" }),
        &first_vol,
        "prose.txt",
        None,
    );
    let actual = our_member_sha(&first_vol, "prose.txt", None);
    assert_eq!(actual, expected, "multivol -ma4 member bytes");
    let _ = files;
    let _ = std::fs::remove_dir_all(&work);
}

// ── tests ──────────────────────────────────────────────────────────────────

#[test]
fn live_rar591_rar4_parity() {
    if !live_enabled() {
        skip("set RAR_LIVE_INTEROP=1 to run live tool tests");
        return;
    }
    let tools = resolve_tools();
    let Some(rar591) = &tools.rar591 else {
        skip("WinRAR 5.91 not found (set RAR_TOOLS_DIR or RAR_591_DIR)");
        return;
    };
    let _ = rar591;
    scenario_rar591_created_rar4_reads_identically(&tools);
    scenario_header_encrypted_rar4(&tools);
    scenario_multivol_rar4(&tools);
}
