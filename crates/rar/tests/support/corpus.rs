#![allow(dead_code)] // each test binary links the whole module and uses a subset
//! The legacy (RAR 1.5–4.x) corpus, generated on demand.
//!
//! Every archive the read suites need is produced here by the newest release
//! that still supports the feature, using the deterministic payloads from
//! [`super::generate::payload`]. Nothing is vendored, so the assertions pin
//! *behaviour* (decode, CRC, member shape, encryption boundary) rather than a
//! particular build's byte output.
//!
//! Which release, and why — the alternates are not interchangeable:
//!
//! | family | release | reason |
//! |---|---|---|
//! | RAR 3.x/4.x format (`unp_ver 29`) | 6-23 `-ma4` | newest release with `-ma4`; 7-23 dropped it |
//! | RAR 2.0 format (`unp_ver 20`) | 2-90 | the only line that *compresses* v20; 6-23 can only store it |
//! | per-file comments (`cf`) | ≤ 4-20 | the command was removed in 5-00, see [`per_file_comments`] |
//! | RAR 1.5 (`unp_ver 15`) | our writer | no cached release ever wrote it |

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rar_rs::{ArchiveVersion, ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

use super::generate::{self, Generated};

/// Newest release that still writes the RAR4 container (`-ma4`).
const RAR4: &str = generate::TOOL_RAR29;
/// The RAR 2.x line: the only one that compresses `unp_ver 20`.
const RAR2: &str = generate::TOOL_V20_COMPRESSED;
/// Newest release that still has the `cf` (file comment) command.
pub const RAR_FILE_COMMENTS: &str = "4-20";
/// Newest release that still writes RAR4 recovery volumes (`rv3`); RAR5 dropped
/// the feature, so this cannot be the 6-23 used for the rest of RAR4.
pub const REV3: &str = "4-20";
/// Newest release that writes the *legacy* recovery-volume layout: the 4.x line
/// already used the modern `.partN.rev` trailer.
pub const REV3_OLD: &str = "3-00";
/// Oldest release rarlab ships for Linux. Its recovery volumes are the third
/// naming shape (`<base><N>.rev`), so it covers the legacy layout on Linux too.
pub const REV3_LATE: &str = "3-93";
/// RAR 3.00: the release line that still writes PPMd members carrying a VM
/// filter record on PPMd escape 3.
pub const PPMD_REFERENCE: &str = REV3_OLD;

/// 49 characters: long enough that the member-encryption KDF must take its
/// mutate-in-place slow branch (salted, SHA-1 block boundary at 28 chars).
pub const LONG_PASSWORD: &str = "0123456789012345678901234567890123456789012345678";

// ── RAR4 format: store, compression levels, solid ───────────────────────────

/// `-ma4 -m0`: one stored member, no compression path involved.
pub fn store() -> Option<Generated> {
    generate::archive(RAR4, &["-ma4", "-m0"], &["hello.txt"])
}

/// `-ma4 -m3`: LZSS+Huffman over a repetitive text and an incompressible blob.
pub fn compressed_m3() -> Option<Generated> {
    generate::archive(RAR4, &["-ma4", "-m3"], &["repeat.txt", "random.bin"])
}

/// `-ma4 -m5`: the same members at the top level, where LZ still beats PPMd.
pub fn compressed_m5() -> Option<Generated> {
    generate::archive(RAR4, &["-ma4", "-m5"], &["repeat.txt", "random.bin"])
}

/// `-ma4 -m3 -s`: a solid chain whose middle members decode through the
/// shared window.
pub fn solid_m3() -> Option<Generated> {
    generate::archive(
        RAR4,
        &["-ma4", "-m3", "-s"],
        &["a.txt", "b.txt", "repeat.txt", "random.bin"],
    )
}

// ── RAR4 format: encryption boundaries ──────────────────────────────────────

/// `-p`: member data encrypted, names in the clear.
pub fn member_password() -> Option<Generated> {
    generate::archive(
        RAR4,
        &["-ma4", "-m3", "-ppass123"],
        &["repeat.txt", "random.bin"],
    )
}

/// `-hp`: names encrypted too, so `open` needs the password.
pub fn header_password() -> Option<Generated> {
    generate::archive(RAR4, &["-ma4", "-m3", "-hppassword"], &["hello.txt"])
}

/// `-hp` with a non-ASCII member name inside the encrypted header.
pub fn header_password_unicode() -> Option<Generated> {
    generate::archive(RAR4, &["-ma4", "-m3", "-hp1234"], &["中文.txt", "a.txt"])
}

/// `-p` with [`LONG_PASSWORD`], hitting the salted slow KDF branch.
pub fn long_password_member() -> Option<Generated> {
    let flag = format!("-p{LONG_PASSWORD}");
    generate::archive(RAR4, &["-ma4", "-m3", &flag], &["longpw.txt"])
}

/// `-hp` with [`LONG_PASSWORD`]: same KDF without a salt, so the slow branch
/// starts at 32 UTF-16 characters instead of 28.
pub fn long_password_header() -> Option<Generated> {
    let flag = format!("-hp{LONG_PASSWORD}");
    generate::archive(RAR4, &["-ma4", "-m3", &flag], &["longpw.txt"])
}

/// `-hp` plus volume splitting: every volume header is encrypted. The
/// incompressible member guarantees the set actually splits.
pub fn header_password_multivol() -> Option<Generated> {
    generate::archive(
        RAR4,
        &["-ma4", "-m3", "-hppassword", "-v32k"],
        &["bigtext_64k.bin", "random.bin"],
    )
}

// ── RAR4 format: multi-volume sets ──────────────────────────────────────────

/// `arc.partN.rar` naming, compressed.
pub fn multivol_new_names() -> Option<Generated> {
    generate::archive(
        RAR4,
        &["-ma4", "-m3", "-v32k"],
        &["repeat.txt", "random.bin"],
    )
}

/// `arc.partN.rar` naming, stored: the split never touches the codec.
pub fn multivol_stored() -> Option<Generated> {
    generate::archive(
        RAR4,
        &["-ma4", "-m0", "-v32k"],
        &["repeat.txt", "random.bin"],
    )
}

/// `arc.partN.rar` naming with `-p`: each segment decrypts separately.
pub fn multivol_encrypted() -> Option<Generated> {
    generate::archive(
        RAR4,
        &["-ma4", "-m3", "-ppassword", "-v32k"],
        &["repeat.txt", "random.bin"],
    )
}

/// Legacy `arc.rar`/`arc.r00`/`arc.r01` numbering (`-vn`), stored.
pub fn multivol_old_names() -> Option<Generated> {
    generate::archive(RAR4, &["-ma4", "-m0", "-vn", "-v32k"], &["random.bin"])
}

// ── RAR4 format: PPMd members ───────────────────────────────────────────────

/// `-m5` over natural-language text: PPMd pays off here.
pub fn ppmd_text() -> Option<Generated> {
    generate::archive(RAR4, &["-ma4", "-m5"], &["lorem_127k.txt"])
}

/// PPMd over a binary blob: the escape-heavy path.
pub fn ppmd_mixed() -> Option<Generated> {
    generate::archive(RAR4, &["-ma4", "-m5"], &["binary_64k.bin", "text.txt"])
}

/// PPMd over a repeated phrase with a varying tail: LZ-match pressure.
pub fn ppmd_phrase() -> Option<Generated> {
    generate::archive(RAR4, &["-ma4", "-m5"], &["repeated_phrase_64k.txt"])
}

/// Two members in one solid PPMd chain: the second references the first.
pub fn ppmd_solid() -> Option<Generated> {
    generate::archive(
        RAR4,
        &["-ma4", "-m5", "-s"],
        &["lorem_127k.txt", "text.txt"],
    )
}

// ── RAR4 format: standard VM filters ────────────────────────────────────────

/// One member per standard filter shape WinRAR picks on its own: audio
/// (`audio.wav`), RGB bitmap (`grad.bmp`), x86 (`code.exe`).
pub fn vm_filters() -> Option<Generated> {
    generate::archive(
        RAR4,
        &["-ma4", "-m3"],
        &["audio.wav", "grad.bmp", "code.exe"],
    )
}

/// PPMd member whose VM filter record rides PPMd escape 3 instead of an LZ
/// symbol: `-mct`/`-mce+` on the RAR 3.00 line, the only writers that pick that
/// shape. Generated at 3.00 because 6-23 no longer emits the escape-3 path.
pub fn ppmd_vm_filter() -> Option<Generated> {
    generate::archive(
        PPMD_REFERENCE,
        &["-m5", "-mc10:16t+", "-mce+"],
        &["code.exe"],
    )
}

/// Solid chain whose second member is an `E8`-dense executable: the filter
/// offsets of a later solid member are member-relative, so decoding it after a
/// non-filtered lead is the real test. Solid chains group by extension
/// (`.bin` before `.exe`), so `random.bin` really is stored first.
pub fn solid_e8() -> Option<Generated> {
    generate::archive(RAR4, &["-ma4", "-m3", "-s"], &["random.bin", "code.exe"])
}

// ── RAR 2.x (`unp_ver 20`) ──────────────────────────────────────────────────

/// RAR 2.0 members: audio-table blocks (WAV) and plain LZ text.
pub fn rar20_mixed() -> Option<Generated> {
    generate::archive(RAR2, &["-m3"], &["audio.wav", "text.txt"])
}

/// RAR 2.0 solid pair: one main-header solid flag, shared 64 KiB window.
pub fn rar20_solid() -> Option<Generated> {
    generate::archive(RAR2, &["-m3", "-s"], &["SOLID1.TXT", "SOLID2.TXT"])
}

/// RAR 2.0 member encryption (`-p`), two uppercase members.
pub fn rar20_password() -> Option<Generated> {
    generate::archive(RAR2, &["-m3", "-ppassword"], &["FILE1.TXT", "FILE2.TXT"])
}

// ── RAR 1.5 (`unp_ver 15`) ──────────────────────────────────────────────────

/// RAR 1.5 member written by our own writer: 2.90's codec, no cached release
/// can produce this container.
pub fn rar154_normal() -> Option<Generated> {
    write_own(ArchiveVersion::V15, &["README.md"], None, None)
}

/// RAR 1.5 member with `-p`, identical content to [`rar154_normal`] so the
/// decrypted bytes can be compared against the plain archive.
pub fn rar154_password() -> Option<Generated> {
    write_own(ArchiveVersion::V15, &["README.md"], Some("password"), None)
}

/// The 17-file documentation set as one RAR 1.5 archive.
pub fn rar154_docset() -> Option<Generated> {
    let names: Vec<String> = (1..=17).map(|i| format!("doc_{i:02}.txt")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    write_own(ArchiveVersion::V15, &refs, None, None)
}

/// RAR 1.5 old-numbered split set (`random.bin` across `arc.part1.rar`/…).
/// The payload is incompressible on purpose: a compressible one would fit in
/// the first volume and never exercise the split.
pub fn rar154_split_old() -> Option<Generated> {
    write_own(ArchiveVersion::V15, &["random.bin"], None, Some(32 * 1024))
}

/// Official UnRAR's verdict on a generated archive: `Ok` only if the release
/// extracts every member without a checksum complaint. `None` when that release
/// is not cached, so the caller can skip.
pub fn official_accepts(version: &str, generated: &Generated) -> Option<Result<(), String>> {
    generate::unrar_test(version, generated.path())
}

// ── RAR5 with an inline recovery record ────────────────────────────────────

/// RAR5 multi-member baseline: the byte seed the repair and streaming suites
/// mutate.
pub fn rar5_basic() -> Option<Generated> {
    generate::archive(
        generate::TOOL_RAR50,
        &["-ma5", "-m3"],
        &["text.txt", "noise.bin"],
    )
}

/// One archive per legacy container generation (v14, v20, v20-store, v29,
/// v29 header protection), used as block-envelope mutation seeds.
pub fn legacy_repair_seeds() -> Option<Vec<Vec<u8>>> {
    let v14 = write_own(ArchiveVersion::V14, &["text.txt", "noise.bin"], None, None)?;
    let v20 = generate::archive(RAR2, &["-m3"], &["text.txt", "noise.bin"])?;
    let v20_stored = generate::archive(RAR2, &["-m0"], &["text.txt"])?;
    let v29 = generate::archive(RAR4, &["-ma4", "-m3"], &["text.txt", "noise.bin"])?;
    let v29_rr1 = generate::archive(RAR4, &["-ma4", "-m3", "-rr1"], &["text.txt"])?;
    Some(vec![
        v14.read(),
        v20.read(),
        v20_stored.read(),
        v29.read(),
        v29_rr1.read(),
    ])
}

/// RAR5 archive with a real WinRAR inline recovery record (`-rr5%`): the shape
/// the repair engine has to rebuild for a third-party archive, not our own.
pub fn rar5_recovery_record() -> Option<Generated> {
    generate::archive(
        generate::TOOL_RAR50,
        &["-rr5%", "-ma5", "-s-", "-m5", "-ep"],
        &["doc.txt", "data.bin"],
    )
}

/// RAR 2.x archive whose comment is embedded in the main header, the layout
/// writers used before the standalone `CMT` block existed.
pub fn rar2_main_comment() -> Option<Generated> {
    generate::archive_with(
        RAR2,
        &["-m0", "-zcomment.txt"],
        &["a.txt", "b.txt"],
        &["comment.txt"],
    )
}

/// RAR4 archive with a Chinese comment: the writer must store it as UTF-16LE
/// inside the compressed `CMT` block, which is the encoding path the reader
/// has to undo.
pub fn unicode_comment() -> Option<Generated> {
    generate::archive_with(
        RAR4,
        &["-ma4", "-m3", "-zcomment_zh.txt"],
        &["text.txt"],
        &["comment_zh.txt"],
    )
}

// ── Plain RAR5 sets ────────────────────────────────────────────────────────

/// Four-member RAR5 archive, the shape a "does the reader agree with the real
/// tool" smoke test wants.
pub fn rar5_multiple_files() -> Option<Generated> {
    generate::archive(
        generate::TOOL_RAR50,
        &["-ma5", "-m3"],
        &["test1.bin", "test2.bin", "test3.bin", "test4.bin"],
    )
}

// ── RAR5 NTFS alternate data streams (`-os`) ───────────────────────────────

/// RAR5 member carrying one `:meta` alternate data stream, plain.
pub fn ntfs_stream() -> Option<Generated> {
    generate::archive_with(
        generate::TOOL_RAR50,
        &["-ma5", "-m3", "-os"],
        &["data.txt"],
        &["data.txt:meta"],
    )
}
/// Same archive with `-p`, so the stream record sits behind member encryption.
pub fn ntfs_stream_password() -> Option<Generated> {
    generate::archive_with(
        generate::TOOL_RAR50,
        &["-ma5", "-m3", "-os", "-ppw"],
        &["data.txt"],
        &["data.txt:meta"],
    )
}

// ── Legacy recovery volumes (`.rev`) ────────────────────────────────────

/// `.partN.rar` set plus `rv3` recovery volumes, from 4.20: the modern
/// trailer layout (`set.partN.rev`).
pub fn rev3_newstyle() -> Option<Generated> {
    generate::archive_then(
        REV3,
        &["-m0", "-v64k"],
        &["big.bin"],
        &[&["rv3", "-y", "-v64k"]],
    )
}

/// Old-numbered set (`set.rar`/`set.r00`…) plus `rv3` from 3.00: the legacy
/// `<base>4_3_N.rev` naming and its own trailer layout.
pub fn rev3_oldstyle() -> Option<Generated> {
    generate::archive_then(
        REV3_OLD,
        &["-m0", "-vn", "-v64k"],
        &["big.bin"],
        &[&["rv3", "-y", "-v64k"]],
    )
}

/// Third shape, from 3.93: still the old `.r00` volumes, but the recovery
/// volumes are named `<base><N>.rev` with no `4_3` infix. 3.93 is also the
/// oldest release rarlab ships for Linux, so this recipe still runs on Linux
/// where [`rev3_oldstyle`] (3.00) cannot.
pub fn rev3_oldstyle_late() -> Option<Generated> {
    generate::archive_then(
        REV3_LATE,
        &["-m0", "-vn", "-v64k"],
        &["big.bin"],
        &[&["rv3", "-y", "-v64k"]],
    )
}

// ── our own writer, for the containers no cached release produces ──────────

fn write_own(
    version: ArchiveVersion,
    names: &[&str],
    password: Option<&str>,
    volume_size: Option<u64>,
) -> Option<Generated> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = generate::scratch_root().join(format!("own-{version:?}-{n}").to_lowercase());
    std::fs::create_dir_all(&dir).expect("create writer dir");
    let mut options = WriterOptions::new().compression(version);
    if let Some(password) = password {
        options = options.password(password);
    }
    if let Some(size) = volume_size {
        options = options.volume_size(size);
    }
    let path = dir.join("arc.rar");
    let mut writer = ArchiveWriter::create_with(&path, options).ok()?;
    let level = EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3).ok()?);
    for name in names {
        let data = generate::payload(name);
        if writer.add_bytes(name, &data, level).is_err() {
            return None;
        }
    }
    writer.finish().ok()?;
    let files = collect_files(&dir);
    let archive = files
        .iter()
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(".rar"))
        })
        .cloned()?;
    Some(Generated {
        dir,
        archive,
        files,
    })
}

fn collect_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read writer dir")
        .flatten()
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

impl Generated {
    /// Member names the archive holds, in stored order.
    pub fn member_names(&self) -> Vec<String> {
        rar_rs::ArchiveReader::open(self.path())
            .expect("open generated archive")
            .entries()
            .map(|e| e.name().to_string())
            .collect()
    }
}
