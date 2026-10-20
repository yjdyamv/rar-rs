//! Format-level parity (对拍) against archives the official tools write.
//!
//! The two sides run on the same payload files with the same pinned mtime (see
//! [`generate::PAYLOAD_MTIME_SECS`]), so every metadata field they can both
//! express must agree. Two strengths of check:
//!
//! * **STORE**: nothing is compressed, so the whole archive is a pure product of
//!   the container rules — it must be byte-identical to WinRAR's.
//! * **Compressed**: only the packed sizes and the CRCs that cover them may
//!   differ (a different match finder is not a format difference), so the walk
//!   below compares the block sequence and every header field, ignoring just the
//!   packed length, the packed data area, and the header checksums that cover
//!   them.
//!
//! Deliberate divergences, all verified against the reference builds and all
//! narrowing our output rather than inventing it:
//!
//! * `RAR 1.5/2.x` output carries neither an `FHD_EXTTIME` area nor an
//!   end-of-archive block: WinRAR 2.90 writes neither (`-m0` and `-v` sets
//!   alike), and a sub-second mtime in a `20` member makes UnRAR 2.90 report a
//!   corrupt header.
//! * With quick-open disabled and no recovery record, the main header carries
//!   no locator record at all — see [`locator_parity`].
//! * The locator's reserved offset-field width follows
//!   `WriterOptions::estimated_size`, not WinRAR's internal size heuristic.

#[path = "support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};

use rar_rs::{
    ArchiveEditor, ArchiveVersion, ArchiveWriter, CompressionLevel, EditPlan, EntryWriteOptions,
    SolidMode, WriterOptions,
};

use support::generate;

const RAR4_SIG: &[u8] = b"Rar!\x1a\x07\x00";
const RAR5_SIG: &[u8] = b"Rar!\x1a\x07\x01\x00";

/// RAR4 `FILE_HEAD` flags used by the walker (WinRAR's own numbering).
const FHD_LARGE: u16 = 0x0100;
const FHD_SALT: u16 = 0x0400;
const FHD_EXTTIME: u16 = 0x1000;

/// RAR5 `FILE_HEAD`/`MAIN_HEAD` flags the walker needs.
const R5_BLOCK_EXTRA: u64 = 0x0001;
const R5_BLOCK_DATA: u64 = 0x0002;
const R5_FILE_TIME_UNIX: u64 = 0x0002;
const R5_FILE_CRC32: u64 = 0x0004;
const R5_LOCATOR_FLAG_QO: u64 = 0x0001;
const R5_LOCATOR_FLAG_RR: u64 = 0x0002;

// ── A minimal, independent header walker ───────────────────────────────────
//
// Independent on purpose: reusing the crate's own parser could hide a rule both
// sides get wrong. Only the fields the comparison needs are decoded.

#[derive(Debug, PartialEq, Eq)]
struct Field {
    name: &'static str,
    value: String,
}

#[derive(Debug)]
struct Block {
    kind: &'static str,
    /// Absolute offset of the block's first byte (the checksum field).
    start: usize,
    fields: Vec<Field>,
}

fn field(name: &'static str, value: impl ToString) -> Field {
    Field {
        name,
        value: value.to_string(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Decode a RAR5 variable-length integer; returns the value and its width.
fn vint(bytes: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    let mut i = at;
    loop {
        let byte = *bytes.get(i)?;
        value |= u64::from(byte & 0x7f) << shift;
        i += 1;
        if byte & 0x80 == 0 {
            return Some((value, i - at));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

fn walk(archive: &[u8]) -> Result<Vec<Block>, String> {
    if archive.starts_with(RAR5_SIG) {
        walk_rar5(archive)
    } else if archive.starts_with(RAR4_SIG) {
        walk_rar4(archive)
    } else {
        Err(format!(
            "unknown archive signature: {}",
            hex(&archive[..archive.len().min(8)])
        ))
    }
}

fn walk_rar4(archive: &[u8]) -> Result<Vec<Block>, String> {
    let body = &archive[RAR4_SIG.len()..];
    let mut blocks = Vec::new();
    let mut p = 0usize;
    while p + 7 <= body.len() {
        let kind = match body[p + 2] {
            0x73 => "main",
            0x74 => "file",
            0x7a => "recovery",
            0x7b => "end",
            _ => "other",
        };
        let flags = le16(body, p + 3).ok_or("truncated RAR4 flags")?;
        let size = usize::from(le16(body, p + 5).ok_or("truncated RAR4 head size")?);
        if size < 7 || p + size > body.len() {
            return Err(format!(
                "RAR4 block at {p} has an impossible head size {size}"
            ));
        }
        let mut fields = vec![
            field("header_crc", hex(&body[p..p + 2])),
            field("flags", format!("0x{flags:04x}")),
            field("head_size", size),
        ];
        let mut data = 0usize;
        if kind == "file" {
            let pack = le32(body, p + 7).ok_or("truncated RAR4 pack size")?;
            let unp = le32(body, p + 11).ok_or("truncated RAR4 unp size")?;
            let name_size = usize::from(le16(body, p + 26).ok_or("truncated RAR4 name size")?);
            let name_end = p + 32 + name_size;
            let name = body
                .get(p + 32..name_end)
                .ok_or("truncated RAR4 file name")?;
            fields.push(field("pack_size", pack));
            fields.push(field("unp_size", unp));
            fields.push(field("host_os", body[p + 15]));
            fields.push(field(
                "file_crc32",
                format!("{:08x}", le32(body, p + 16).unwrap()),
            ));
            fields.push(field(
                "dos_time",
                format!("{:08x}", le32(body, p + 20).unwrap()),
            ));
            fields.push(field("unp_ver", body[p + 24]));
            fields.push(field("method", body[p + 25]));
            fields.push(field(
                "attr",
                format!("{:08x}", le32(body, p + 28).unwrap()),
            ));
            fields.push(field("name", String::from_utf8_lossy(name)));
            let mut ext = name_end;
            if flags & FHD_LARGE != 0 {
                // 64-bit sizes: HIGH_PACK_SIZE/HIGH_UNP_SIZE sit after the attr.
                fields.push(field(
                    "high_sizes",
                    format!(
                        "{}/{}",
                        le32(body, p + 32).unwrap_or(0),
                        le32(body, p + 36).unwrap_or(0)
                    ),
                ));
                ext = ext.max(p + 40 + name_size);
            }
            if flags & FHD_SALT != 0 {
                fields.push(field("salt", hex(&body[ext..(ext + 8).min(p + size)])));
                ext += 8;
            }
            if flags & FHD_EXTTIME != 0 {
                fields.push(field("ext_time", hex(&body[ext.min(p + size)..p + size])));
            }
            data = usize::try_from(pack).map_err(|_| "RAR4 pack size overflows usize")?;
        } else if kind == "main" {
            fields.push(field("main_tail", hex(&body[p + 7..p + size])));
        }
        blocks.push(Block {
            kind,
            start: p + RAR4_SIG.len(),
            fields,
        });
        p += size + data;
    }
    if p != body.len() {
        return Err(format!("RAR4 walk stopped at {p} of {} bytes", body.len()));
    }
    Ok(blocks)
}

fn walk_rar5(archive: &[u8]) -> Result<Vec<Block>, String> {
    let body = &archive[RAR5_SIG.len()..];
    let mut blocks = Vec::new();
    let mut p = 0usize;
    while p + 5 <= body.len() {
        let (head_size, size_width) = vint(body, p + 4).ok_or("truncated RAR5 head size")?;
        let head_size = usize::try_from(head_size).map_err(|_| "RAR5 head size overflows")?;
        let head_end = p + 4 + size_width + head_size;
        if head_end > body.len() {
            return Err(format!("RAR5 block at {p} runs past the archive end"));
        }
        let mut q = p + 4 + size_width;
        let (block_type, n) = vint(body, q).ok_or("truncated RAR5 block type")?;
        q += n;
        let (block_flags, n) = vint(body, q).ok_or("truncated RAR5 block flags")?;
        q += n;
        let (extra_size, mut data_size) = (0u64, 0u64);
        let mut extra_size = extra_size;
        if block_flags & R5_BLOCK_EXTRA != 0 {
            let (v, n) = vint(body, q).ok_or("truncated RAR5 extra size")?;
            extra_size = v;
            q += n;
        }
        if block_flags & R5_BLOCK_DATA != 0 {
            let (v, n) = vint(body, q).ok_or("truncated RAR5 data size")?;
            data_size = v;
            q += n;
        }
        let kind = match block_type {
            1 => "main",
            2 => "file",
            5 => "end",
            _ => "other",
        };
        let mut fields = vec![
            field("header_crc32", format!("{:08x}", le32(body, p).unwrap())),
            field("head_size", head_size),
            field("block_flags", format!("0x{block_flags:02x}")),
            field("extra_size", extra_size),
            field("data_size", data_size),
        ];
        let extra_start = head_end - usize::try_from(extra_size).map_err(|_| "extra size")?;
        let extra = &body[extra_start..head_end];
        if kind == "main" {
            let (arch_flags, _) = vint(body, q).ok_or("truncated RAR5 archive flags")?;
            fields.push(field("archive_flags", format!("0x{arch_flags:02x}")));
            let locator = locator_fields(extra)?;
            fields.extend(locator);
        } else if kind == "file" {
            let (file_flags, n) = vint(body, q).ok_or("truncated RAR5 file flags")?;
            q += n;
            let (unp_size, n) = vint(body, q).ok_or("truncated RAR5 unp size")?;
            q += n;
            let (attributes, n) = vint(body, q).ok_or("truncated RAR5 attributes")?;
            q += n;
            fields.push(field("file_flags", format!("0x{file_flags:02x}")));
            fields.push(field("unp_size", unp_size));
            fields.push(field("attributes", format!("{attributes:08x}")));
            if file_flags & R5_FILE_TIME_UNIX != 0 {
                let mtime = le32(body, q).ok_or("truncated RAR5 mtime")?;
                q += 4;
                fields.push(field("mtime", mtime));
            }
            if file_flags & R5_FILE_CRC32 != 0 {
                let crc = le32(body, q).ok_or("truncated RAR5 crc32")?;
                q += 4;
                fields.push(field("file_crc32", format!("{crc:08x}")));
            }
            let (comp_info, n) = vint(body, q).ok_or("truncated RAR5 comp info")?;
            q += n;
            fields.push(field("comp_info", format!("0x{comp_info:04x}")));
            fields.push(field("comp_version", comp_info & 0x3f));
            fields.push(field("comp_solid", (comp_info >> 6) & 1));
            fields.push(field("comp_method", (comp_info >> 7) & 0x07));
            let (host_os, n) = vint(body, q).ok_or("truncated RAR5 host os")?;
            q += n;
            fields.push(field("host_os", host_os));
            let (name_len, n) = vint(body, q).ok_or("truncated RAR5 name length")?;
            q += n;
            let name_len = usize::try_from(name_len).map_err(|_| "name length")?;
            let name = body.get(q..q + name_len).ok_or("truncated RAR5 name")?;
            fields.push(field("name", String::from_utf8_lossy(name)));
            if !extra.is_empty() {
                fields.push(field("file_extra", hex(extra)));
            }
        } else if !extra.is_empty() {
            fields.push(field("extra", hex(extra)));
        }
        blocks.push(Block {
            kind,
            start: p + RAR5_SIG.len(),
            fields,
        });
        p = head_end + usize::try_from(data_size).map_err(|_| "RAR5 data size")?;
    }
    if p != body.len() {
        return Err(format!("RAR5 walk stopped at {p} of {} bytes", body.len()));
    }
    Ok(blocks)
}

/// Decode the locator record (extra record type 1) into its fields. A missing
/// record is reported as such instead of guessed at.
fn locator_fields(extra: &[u8]) -> Result<Vec<Field>, String> {
    let mut q = 0usize;
    while q < extra.len() {
        let (rec_size, n) = vint(extra, q).ok_or("truncated extra record size")?;
        let rec_start = q + n;
        let rec_end = rec_start + usize::try_from(rec_size).map_err(|_| "extra record size")?;
        let (rec_type, n) = vint(extra, q + n).ok_or("truncated extra record type")?;
        if rec_type == 1 {
            let mut f = Vec::new();
            let mut b = q + n + n;
            let (flags, n) = vint(extra, b).ok_or("truncated locator flags")?;
            b += n;
            f.push(field("locator_flags", format!("0x{flags:02x}")));
            if flags & R5_LOCATOR_FLAG_QO != 0 {
                let (offset, width) = vint(extra, b).ok_or("truncated locator qo offset")?;
                f.push(field("locator_qo_offset", offset));
                f.push(field("locator_field_width", width));
            }
            if flags & R5_LOCATOR_FLAG_RR != 0 {
                let (offset, _) = vint(extra, b).ok_or("truncated locator rr offset")?;
                f.push(field("locator_rr_offset", offset));
            }
            let _ = rec_end;
            return Ok(f);
        }
        q = rec_end;
    }
    Ok(vec![field("locator_flags", "absent")])
}

/// Field-level differences between two archives, block by block. Fields named
/// in `ignore` are skipped everywhere (they cover the packed data).
fn mismatches(ours: &[u8], theirs: &[u8], ignore: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let (Ok(a), Ok(b)) = (walk(ours), walk(theirs)) else {
        out.push(format!(
            "walk failed: ours {:?}, official {:?}",
            walk(ours).err(),
            walk(theirs).err()
        ));
        return out;
    };
    if a.len() != b.len() {
        out.push(format!(
            "block count: ours {} {:?}, official {} {:?}",
            a.len(),
            a.iter().map(|b| b.kind).collect::<Vec<_>>(),
            b.len(),
            b.iter().map(|b| b.kind).collect::<Vec<_>>()
        ));
        return out;
    }
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        if x.kind != y.kind {
            out.push(format!("block {i}: kind {} vs {}", x.kind, y.kind));
            continue;
        }
        for f in &x.fields {
            if ignore.contains(&f.name) {
                continue;
            }
            match y.fields.iter().find(|g| g.name == f.name) {
                Some(g) if g.value == f.value => {}
                Some(g) => out.push(format!(
                    "block {i} ({}): {} ours {} vs official {}",
                    x.kind, f.name, f.value, g.value
                )),
                None => out.push(format!(
                    "block {i} ({}): official has no field {}",
                    x.kind, f.name
                )),
            }
        }
        for g in &y.fields {
            if !ignore.contains(&g.name) && !x.fields.iter().any(|f| f.name == g.name) {
                out.push(format!(
                    "block {i} ({}): ours has no field {}",
                    x.kind, g.name
                ));
            }
        }
    }
    out
}

// ── Fixtures ───────────────────────────────────────────────────────────────

/// Copy [`generate::payload`] into `dir` and pin its mtime to the harness
/// constant, so both writers see the same timestamp.
fn sample(dir: &Path, name: &str) -> PathBuf {
    sample_with(dir, name, &generate::payload(name))
}

/// Write `contents` as `name` and pin the mtime to the second, so both
/// writers store the same timestamp without an extended-time record.
fn sample_with(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    let stamp =
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(generate::PAYLOAD_MTIME_SECS);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(stamp))
        .unwrap();
    path
}

/// One comparison case: how to ask both writers for the same archive.
struct Case {
    label: &'static str,
    /// Cache directory name of the official tool.
    tool: &'static str,
    /// Switches handed to the official `a` command.
    switches: &'static [&'static str],
    /// Our writer's container version.
    version: ArchiveVersion,
    level: CompressionLevel,
    /// Payload names, in the order both sides add them.
    members: &'static [&'static str],
    quick_open: bool,
    solid: bool,
}

fn ours(case: &Case, dir: &Path) -> Vec<u8> {
    let path = dir.join(format!("ours-{}.rar", case.label.replace(' ', "_")));
    let mut writer = ArchiveWriter::create_with(
        &path,
        WriterOptions::new()
            .compression(case.version)
            .quick_open(case.quick_open)
            .solid_mode(if case.solid {
                SolidMode::Continuous
            } else {
                SolidMode::Disabled
            }),
    )
    .unwrap();
    for member in case.members {
        let src = sample(dir, member);
        writer
            .add_path_as(
                &src,
                member,
                EntryWriteOptions::new().compression_level(case.level),
            )
            .unwrap();
    }
    writer.finish().unwrap();
    std::fs::read(&path).unwrap()
}

fn official(case: &Case) -> Option<Vec<u8>> {
    generate::archive(case.tool, case.switches, case.members).map(|made| made.read())
}

/// Run one case and return both sides, skipping when the tool is not cached.
fn both(case: &Case, dir: &Path) -> Option<(Vec<u8>, Vec<u8>)> {
    let theirs = official(case)?;
    Some((ours(case, dir), theirs))
}

const STORE: CompressionLevel = CompressionLevel::STORE;
const NORMAL: CompressionLevel = CompressionLevel::NORMAL;

/// Fields that may differ when the member is compressed: the packed length of
/// this block and the checksums that cover the packed bytes.
const PACKED: &[&str] = &[
    "pack_size",
    "header_crc",
    "data_size",
    "header_crc32",
    // The header length itself depends on the packed size, whose vint width
    // follows the encoder's output length.
    "head_size",
];

// ── Tests ──────────────────────────────────────────────────────────────────

/// A STORE archive is pure container rules, so it has to match byte for byte —
/// including the member order, the timestamps, the attributes, the window bits
/// and the end-of-archive block.
#[test]
fn store_archives_are_byte_identical_to_official_rar() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        Case {
            label: "rar5",
            tool: generate::TOOL_RAR50,
            switches: &["-ma5", "-m0", "-qo-"],
            version: ArchiveVersion::V50,
            level: STORE,
            members: &["text.txt"],
            quick_open: false,
            solid: false,
        },
        Case {
            label: "rar5 two members",
            tool: generate::TOOL_RAR50,
            switches: &["-ma5", "-m0", "-qo-"],
            version: ArchiveVersion::V50,
            level: STORE,
            members: &["text.txt", "tiny.txt"],
            quick_open: false,
            solid: false,
        },
        Case {
            label: "rar4",
            tool: generate::TOOL_RAR29,
            switches: &["-ma4", "-m0"],
            version: ArchiveVersion::V29,
            level: STORE,
            members: &["text.txt"],
            quick_open: false,
            solid: false,
        },
        Case {
            label: "rar4 two members",
            tool: generate::TOOL_RAR29,
            switches: &["-ma4", "-m0"],
            version: ArchiveVersion::V29,
            level: STORE,
            members: &["text.txt", "tiny.txt"],
            quick_open: false,
            solid: false,
        },
        Case {
            label: "rar2",
            tool: generate::TOOL_V20_COMPRESSED,
            switches: &["-m0"],
            version: ArchiveVersion::V20,
            level: STORE,
            members: &["text.txt"],
            quick_open: false,
            solid: false,
        },
    ];
    let mut checked = 0;
    for case in &cases {
        let Some((mine, theirs)) = both(case, dir.path()) else {
            continue;
        };
        checked += 1;
        let diffs = mismatches(&mine, &theirs, &[]);
        assert!(
            diffs.is_empty(),
            "{}: header fields differ\n  {}\n  ours {} bytes, official {} bytes",
            case.label,
            diffs.join("\n  "),
            mine.len(),
            theirs.len()
        );
        assert_eq!(
            mine,
            theirs,
            "{}: STORE archives must be byte-identical (ours {} bytes, official {} bytes)",
            case.label,
            mine.len(),
            theirs.len()
        );
    }
    assert!(checked > 0, "no cached tool available for any STORE case");
}

/// Compressed members: the encoder picks its own matches, so the packed bytes
/// and their length may differ. Everything the container decides must not.
#[test]
fn compressed_archives_agree_field_for_field() {
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        Case {
            label: "rar5",
            tool: generate::TOOL_RAR50,
            switches: &["-ma5", "-m3", "-qo-"],
            version: ArchiveVersion::V50,
            level: NORMAL,
            members: &["text.txt"],
            quick_open: false,
            solid: false,
        },
        Case {
            label: "rar4",
            tool: generate::TOOL_RAR29,
            switches: &["-ma4", "-m3"],
            version: ArchiveVersion::V29,
            level: NORMAL,
            members: &["text.txt"],
            quick_open: false,
            solid: false,
        },
        Case {
            label: "rar2",
            tool: generate::TOOL_V20_COMPRESSED,
            switches: &["-m3"],
            version: ArchiveVersion::V20,
            level: NORMAL,
            members: &["text.txt"],
            quick_open: false,
            solid: false,
        },
    ];
    let mut checked = 0;
    for case in &cases {
        let Some((mine, theirs)) = both(case, dir.path()) else {
            continue;
        };
        checked += 1;
        let diffs = mismatches(&mine, &theirs, PACKED);
        assert!(
            diffs.is_empty(),
            "{}: header fields differ\n  {}",
            case.label,
            diffs.join("\n  ")
        );
        // The unp stream must still be covered by the same CRC, i.e. both sides
        // compressed the same bytes.
        let ours_crc = file_crc(&mine).expect("our file header carries a CRC");
        let theirs_crc = file_crc(&theirs).expect("official file header carries a CRC");
        assert_eq!(ours_crc, theirs_crc, "{}: member CRC", case.label);
    }
    assert!(
        checked > 0,
        "no cached tool available for any compressed case"
    );
}

fn file_crc(archive: &[u8]) -> Option<String> {
    walk(archive)
        .ok()?
        .iter()
        .find(|b| b.kind == "file")
        .and_then(|b| {
            b.fields
                .iter()
                .find(|f| f.name == "file_crc32")
                .map(|f| f.value.clone())
        })
}

/// Solid runs flag every continuation member and announce one dictionary for
/// the whole run. The flag and the order must agree with official rar; the
/// window bits may only ever be *larger* on our side, because the streaming
/// writer cannot know the size of the run up front and must stay safe.
#[test]
fn solid_chain_flags_agree_with_official() {
    let dir = tempfile::tempdir().unwrap();
    // Extension order, then name order — the order WinRAR sorts a solid run
    // into, so the member sequence itself is comparable.
    let case = Case {
        label: "rar4 solid",
        tool: generate::TOOL_RAR29,
        switches: &["-ma4", "-m3", "-s"],
        version: ArchiveVersion::V29,
        level: NORMAL,
        members: &["README.md", "repeat.txt", "text.txt"],
        quick_open: false,
        solid: true,
    };
    let Some((mine, theirs)) = both(&case, dir.path()) else {
        return;
    };
    let ours = walk(&mine).unwrap();
    let official = walk(&theirs).unwrap();
    let summary = |blocks: &[Block]| -> Vec<(String, String, String, u64)> {
        blocks
            .iter()
            .filter(|b| b.kind == "file")
            .map(|b| {
                let get = |name: &str| {
                    b.fields
                        .iter()
                        .find(|f| f.name == name)
                        .map(|f| f.value.clone())
                        .unwrap_or_default()
                };
                let flags = u64::from_str_radix(get("flags").trim_start_matches("0x"), 16).unwrap();
                // Window bits: 0x20 << 0..3 (`0x20` .. `0x100` in the header).
                let bits = match flags & 0x00e0 {
                    0x0020 => 1,
                    0x0040 => 2,
                    0x0080 => 3,
                    _ => 4,
                };
                (
                    get("name"),
                    get("method"),
                    format!("solid={}", (flags >> 4) & 1),
                    bits,
                )
            })
            .collect()
    };
    let ours = summary(&ours);
    let official = summary(&official);
    assert_eq!(ours.len(), official.len(), "member count");
    for (i, (mine, theirs)) in ours.iter().zip(official.iter()).enumerate() {
        assert_eq!(mine.0, theirs.0, "member {i} name order");
        assert_eq!(mine.1, theirs.1, "member {i} method");
        assert_eq!(mine.2, theirs.2, "member {i} solid flag");
        assert!(
            mine.3 >= theirs.3,
            "member {i} window bits: ours {} < official {}",
            mine.3,
            theirs.3
        );
    }
}

/// Quick-open disabled and no recovery record: WinRAR 7.23 writes no locator
/// record at all (`main` header size 3), and neither do we.
#[test]
fn locator_parity() {
    let dir = tempfile::tempdir().unwrap();
    let off = Case {
        label: "rar5 qo off",
        tool: generate::TOOL_RAR50,
        switches: &["-ma5", "-m0", "-qo-"],
        version: ArchiveVersion::V50,
        level: STORE,
        members: &["text.txt"],
        quick_open: false,
        solid: false,
    };
    if let Some((mine, theirs)) = both(&off, dir.path()) {
        assert_eq!(
            mismatches(&mine, &theirs, &[]),
            Vec::<String>::new(),
            "quick-open disabled: no locator record on either side"
        );
        let ours = walk(&mine).unwrap();
        let locator = ours[0]
            .fields
            .iter()
            .find(|f| f.name == "locator_flags")
            .map(|f| f.value.clone());
        assert_eq!(locator.as_deref(), Some("absent"));
    }

    // With quick open on, both write the locator; the reserved offset-field
    // width is WinRAR's internal size estimate, which we do not reproduce, so
    // compare the decoded facts rather than the raw record bytes.
    let on = Case {
        label: "rar5 qo on",
        tool: generate::TOOL_RAR50,
        switches: &["-ma5", "-m0", "-qo+"],
        version: ArchiveVersion::V50,
        level: STORE,
        members: &["text.txt"],
        quick_open: true,
        solid: false,
    };
    if let Some((mine, theirs)) = both(&on, dir.path()) {
        let ignore = [
            "locator_field_width",
            // The locator bytes differ with the field width, so the CRC over
            // the header and its size do too.
            "header_crc32",
            "extra_size",
            "head_size",
        ];
        let diffs = mismatches(&mine, &theirs, &ignore);
        assert!(
            diffs.is_empty(),
            "quick-open enabled: locator facts differ\n  {}",
            diffs.join("\n  ")
        );
        let ours = walk(&mine).unwrap();
        let locator_flags = ours[0]
            .fields
            .iter()
            .find(|f| f.name == "locator_flags")
            .map(|f| f.value.clone());
        assert_eq!(
            locator_flags.as_deref(),
            Some("0x01"),
            "quick-open on: the locator carries the QO flag"
        );
        // `-qo+` forces the record, so the locator must point at one on both
        // sides (the field values themselves are compared above).
        let qo_offset = |blocks: &[Block]| -> Option<String> {
            blocks[0]
                .fields
                .iter()
                .find(|f| f.name == "locator_qo_offset")
                .map(|f| f.value.clone())
        };
        let theirs = walk(&theirs).unwrap();
        assert_eq!(qo_offset(&ours), qo_offset(&theirs));
        assert_ne!(qo_offset(&ours).as_deref(), Some("0"), "QO offset present");
    }
}

/// RAR 1.5/2.x output ends after the last member: WinRAR 2.90 writes no
/// end-of-archive block, and UnRAR 2.90 treats one as a corrupt header.
#[test]
fn rar2_era_archives_end_after_the_last_member() {
    let dir = tempfile::tempdir().unwrap();
    let case = Case {
        label: "rar2",
        tool: generate::TOOL_V20_COMPRESSED,
        switches: &["-m0"],
        version: ArchiveVersion::V20,
        level: STORE,
        members: &["text.txt"],
        quick_open: false,
        solid: false,
    };
    let Some((mine, theirs)) = both(&case, dir.path()) else {
        return;
    };
    let kinds = |bytes: &[u8]| -> Vec<&'static str> {
        walk(bytes).unwrap().iter().map(|b| b.kind).collect()
    };
    assert_eq!(kinds(&mine), vec!["main", "file"]);
    assert_eq!(kinds(&mine), kinds(&theirs));
    assert_eq!(mine, theirs, "the whole RAR 2.0 archive must match");
}

// ── The quick-open catalog is rebuilt by every edit ────────────────────────

/// Locate the quick-open record and decode its cached headers into
/// `(absolute offset of the cached header block, cached header bytes)`.
/// `None` means the archive carries no catalog at all.
fn qo_catalog(archive: &[u8]) -> Option<Vec<(usize, Vec<u8>)>> {
    let body = archive.strip_prefix(RAR5_SIG)?;
    let mut p = 0usize;
    while p + 5 <= body.len() {
        let (head_size, size_width) = vint(body, p + 4)?;
        let head_end = p + 4 + size_width + usize::try_from(head_size).ok()?;
        let mut q = p + 4 + size_width;
        let (block_type, n) = vint(body, q)?;
        q += n;
        let (block_flags, n) = vint(body, q)?;
        q += n;
        let mut extra_size = 0u64;
        if block_flags & R5_BLOCK_EXTRA != 0 {
            let (v, n) = vint(body, q)?;
            extra_size = v;
            q += n;
        }
        let mut data_size = 0u64;
        if block_flags & R5_BLOCK_DATA != 0 {
            let (v, n) = vint(body, q)?;
            data_size = v;
            q += n;
        }
        let payload_start = head_end;
        let payload_end = payload_start + usize::try_from(data_size).ok()?;
        let name = if block_type == 3 {
            // Service body: file flags, unpacked size, attributes, compression
            // info, host OS, then the service name.
            for _ in 0..5 {
                let (_, n) = vint(body, q)?;
                q += n;
            }
            let (name_len, n) = vint(body, q)?;
            let name_len = usize::try_from(name_len).ok()?;
            body.get(q + n..q + n + name_len)
                .map(|raw| String::from_utf8_lossy(raw).into_owned())
        } else {
            None
        };
        if name.as_deref() == Some("QO") {
            let qo_start = p + RAR5_SIG.len();
            let payload = body.get(payload_start..payload_end)?;
            return Some(decode_qo(payload, qo_start));
        }
        // Unused extra bytes only matter to the parser, not to the walk.
        let _ = extra_size;
        p = payload_end;
    }
    None
}

/// Decode one quick-open payload: `[entry crc32][body size][flags][relative
/// offset][header size][header block]`, repeated. The checksum covers the
/// size field together with the body, as official writers stamp it.
fn decode_qo(payload: &[u8], qo_start: usize) -> Vec<(usize, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut off = 0usize;
    while off < payload.len() {
        let stored = u32::from_le_bytes(payload[off..off + 4].try_into().unwrap());
        let (body_len, n) = vint(payload, off + 4).unwrap();
        let body_start = off + 4 + n;
        let body_len = usize::try_from(body_len).unwrap();
        let body = &payload[body_start..body_start + body_len];
        assert_eq!(
            crc32(&payload[off + 4..body_start + body_len]),
            stored,
            "cached entry checksum"
        );
        let (_flags, n) = vint(body, 0).unwrap();
        let (rel, m) = vint(body, n).unwrap();
        let (header_len, k) = vint(body, n + m).unwrap();
        let header_start = n + m + k;
        let header_len = usize::try_from(header_len).unwrap();
        let header = body[header_start..header_start + header_len].to_vec();
        entries.push((qo_start - usize::try_from(rel).unwrap(), header));
        off = body_start + body_len;
    }
    entries
}

/// Names of the members the catalog caches, in record order. Each cached entry
/// must name a real file header *and* carry a copy of the bytes on disk there,
/// which is what makes a stale catalog detectable.
fn cached_names(archive: &[u8]) -> Vec<String> {
    let blocks = walk(archive).unwrap();
    qo_catalog(archive)
        .expect("the archive carries a quick-open record")
        .into_iter()
        .map(|(offset, header)| {
            let block = blocks
                .iter()
                .find(|b| b.start == offset && b.kind == "file")
                .unwrap_or_else(|| panic!("cached offset {offset} is not a file header"));
            assert_eq!(
                &archive[offset..offset + header.len()],
                header.as_slice(),
                "the cached header is a copy of the header on disk"
            );
            block
                .fields
                .iter()
                .find(|f| f.name == "name")
                .expect("file blocks carry a name")
                .value
                .clone()
        })
        .collect()
}

/// A volume set is a product of the same rules as a single archive: the split
/// points, the per-volume main-header flags (`MHD_VOLUME` /
/// `MHD_NEWNUMBERING`), the `prefix_crc32` in the end block and the numbering
/// all have to land where WinRAR puts them. The two sides differ only in the
/// archive base name, which no volume carries inside its bytes.
/// Collect `dir`'s `<slug>*` volumes as `(suffix, bytes)` in name order.
fn volume_set(dir: &Path, slug: &str) -> Vec<(String, Vec<u8>)> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(slug)
        })
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            (
                name.trim_start_matches(slug).to_string(),
                std::fs::read(&path).unwrap(),
            )
        })
        .collect()
}

/// Compare two volume sets name by name, then byte by byte.
fn assert_volume_sets_identical(
    label: &str,
    mine: &[(String, Vec<u8>)],
    theirs: &[(String, Vec<u8>)],
) {
    let names = |set: &[(String, Vec<u8>)]| -> Vec<String> {
        set.iter().map(|(name, _)| name.clone()).collect()
    };
    assert_eq!(names(mine), names(theirs), "{label}: volume naming");
    assert!(mine.len() > 1, "{label}: the payload must span volumes");
    for ((name, ours), (_, official)) in mine.iter().zip(theirs) {
        if ours != official {
            let at = ours
                .iter()
                .zip(official)
                .position(|(a, b)| a != b)
                .unwrap_or(ours.len().min(official.len()));
            let lo = at.saturating_sub(16);
            panic!(
                "{label}: volume {name} differs at byte {at} \
                 (ours {} bytes, official {} bytes)
  ours    {}
  official{}",
                ours.len(),
                official.len(),
                hex(&ours[lo..ours.len().min(at + 32)]),
                hex(&official[lo..official.len().min(at + 32)]),
            );
        }
    }
}

/// Write a split STORE set of `member` with our writer and hand back its
/// volumes, suffixes relative to `archive`'s stem.
fn our_volume_set(
    dir: &Path,
    member: &str,
    version: ArchiveVersion,
    old_numbering: bool,
) -> Vec<(String, Vec<u8>)> {
    our_volume_set_with(dir, member, version, old_numbering, 32 * 1024)
}

/// Same, with an explicit split size.
fn our_volume_set_with(
    dir: &Path,
    member: &str,
    version: ArchiveVersion,
    old_numbering: bool,
    volume_size: u64,
) -> Vec<(String, Vec<u8>)> {
    let src = sample(dir, member);
    let path = dir.join("ours.rar");
    let mut writer = ArchiveWriter::create_with(
        &path,
        WriterOptions::new()
            .compression(version)
            .volume_size(volume_size)
            .old_numbering(old_numbering),
    )
    .unwrap();
    writer
        .add_path_as(
            &src,
            member,
            EntryWriteOptions::new().compression_level(CompressionLevel::STORE),
        )
        .unwrap();
    writer.finish().unwrap();
    std::fs::remove_file(&src).unwrap();
    volume_set(dir, "ours")
}

/// A multi-volume set is the same container rules applied per volume — plus
/// the split flags, the per-volume end-of-archive block and the volume naming
/// scheme — so a STORE set has to match byte for byte.
#[test]
fn multivolume_sets_are_byte_identical_to_official_rar() {
    // Incompressible payload: a 32 KiB split of compressible data collapses
    // into one volume and would test nothing.
    for (label, tool, version, old_numbering, switches) in [
        (
            "rar5 store",
            generate::TOOL_RAR50,
            ArchiveVersion::V50,
            false,
            &["-ma5", "-m0", "-qo-", "-v32k"][..],
        ),
        (
            "new numbering",
            generate::TOOL_RAR29,
            ArchiveVersion::V29,
            false,
            &["-ma4", "-m0", "-v32k"][..],
        ),
        (
            "old numbering",
            generate::TOOL_RAR29,
            ArchiveVersion::V29,
            true,
            &["-ma4", "-m0", "-vn", "-v32k"][..],
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mine = our_volume_set(dir.path(), "big.bin", version, old_numbering);
        let Some(made) = generate::archive_with(tool, switches, &["big.bin"], &[]) else {
            return;
        };
        let theirs: Vec<(String, Vec<u8>)> = made
            .volumes()
            .into_iter()
            .map(|(name, bytes)| (name.trim_start_matches("arc").to_string(), bytes))
            .collect();
        assert_volume_sets_identical(label, &mine, &theirs);
    }
}

/// The RAR 2.x era: the volumes carry no end-of-archive block at all, the
/// first volume is `<stem>.rar` and the rest count from `.r00`. RAR 2.90
/// spells the *new* scheme `-vn`, the opposite of RAR 3.0 and later.
#[test]
fn rar20_volume_sets_are_byte_identical_to_official_rar() {
    for (label, old_numbering, switches) in [
        ("rar20 old numbering", true, &["-m0", "-v32k"][..]),
        ("rar20 new numbering", false, &["-m0", "-vn", "-v32k"][..]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mine = our_volume_set(dir.path(), "big.bin", ArchiveVersion::V20, old_numbering);
        let Some(made) =
            generate::archive_with(generate::TOOL_V20_COMPRESSED, switches, &["big.bin"], &[])
        else {
            return;
        };
        let theirs: Vec<(String, Vec<u8>)> = made
            .volumes()
            .into_iter()
            .map(|(name, bytes)| (name.trim_start_matches("arc").to_string(), bytes))
            .collect();
        assert_volume_sets_identical(label, &mine, &theirs);
    }
}

/// One block of a split volume, as the tolerant scan below reports it.
struct VolBlock {
    kind: u64,
    /// Extra area, verbatim (the locator on the main header, the percent
    /// record on a service block).
    extra: Vec<u8>,
    data_size: u64,
}

/// Scan the blocks of one volume and report how many zero bytes follow the
/// end-of-archive block.
///
/// The strict [`walk`] deliberately refuses a trailing byte, because a
/// single-volume archive must end exactly at the end block. Official RAR
/// pads the tail of every *non-final* volume instead, so the multi-volume
/// comparisons need this tolerant variant to see the blocks at all.
fn volume_blocks(volume: &[u8]) -> (Vec<VolBlock>, usize) {
    let body = volume
        .strip_prefix(RAR5_SIG)
        .expect("a RAR5 volume starts with the RAR5 signature");
    let mut p = 0usize;
    let mut blocks = Vec::new();
    while p + 5 <= body.len() {
        let (head_size, size_width) = vint(body, p + 4).expect("head size");
        let head_size = usize::try_from(head_size).expect("head size");
        let head_end = p + 4 + size_width + head_size;
        let mut q = p + 4 + size_width;
        let (kind, n) = vint(body, q).expect("block type");
        q += n;
        let (flags, n) = vint(body, q).expect("block flags");
        q += n;
        let mut extra_size = 0u64;
        if flags & R5_BLOCK_EXTRA != 0 {
            let (value, n) = vint(body, q).expect("extra size");
            extra_size = value;
            q += n;
        }
        let mut data_size = 0u64;
        if flags & R5_BLOCK_DATA != 0 {
            let (value, _) = vint(body, q).expect("data size");
            data_size = value;
        }
        let extra_start = head_end - usize::try_from(extra_size).expect("extra size");
        blocks.push(VolBlock {
            kind,
            extra: body[extra_start..head_end].to_vec(),
            data_size,
        });
        p = head_end + usize::try_from(data_size).expect("data size");
        if kind == 5 {
            break;
        }
    }
    let padding = &body[p..];
    assert!(
        padding.iter().all(|byte| *byte == 0),
        "junk after the end-of-archive block: {}",
        hex(padding)
    );
    (blocks, padding.len())
}

/// Every scan helper above addresses one archive; a recovery record adds a
/// `RR` service block per volume, and official pads each non-final volume
/// with zeros, so the sets cannot be byte-identical. Everything around that
/// padding still has to agree: same names, same block sequence, the same
/// percent record, and the split arithmetic that turns the padding into a
/// smaller data chunk.
#[test]
fn recovery_records_repeat_in_every_volume() {
    let dir = tempfile::tempdir().unwrap();
    let member = "big.bin";
    let total = generate::payload(member).len() as u64;
    let src = sample(dir.path(), member);
    let path = dir.path().join("ours.rar");
    let mut writer = ArchiveWriter::create_with(
        &path,
        WriterOptions::new()
            .compression(ArchiveVersion::V50)
            .volume_size(32 * 1024)
            .recovery_percent(5),
    )
    .unwrap();
    writer
        .add_path_as(
            &src,
            member,
            EntryWriteOptions::new().compression_level(CompressionLevel::STORE),
        )
        .unwrap();
    writer.finish().unwrap();
    let mine = volume_set(dir.path(), "ours");

    let Some(made) = generate::archive_with(
        generate::TOOL_RAR50,
        &["-ma5", "-m0", "-qo-", "-v32k", "-rr5%"],
        &[member],
        &[],
    ) else {
        return;
    };
    let theirs: Vec<(String, Vec<u8>)> = made
        .volumes()
        .into_iter()
        .map(|(name, bytes)| (name.trim_start_matches("arc").to_string(), bytes))
        .collect();

    assert_eq!(
        mine.iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>(),
        theirs
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>(),
        "volume naming"
    );
    assert!(mine.len() > 3, "the payload must span several volumes");

    let mut our_payload = 0u64;
    let mut their_payload = 0u64;
    for (index, ((name, ours), (_, official))) in mine.iter().zip(&theirs).enumerate() {
        let (our_blocks, our_padding) = volume_blocks(ours);
        let (their_blocks, their_padding) = volume_blocks(official);
        assert_eq!(our_padding, 0, "{name}: we never pad a volume");
        assert_eq!(
            our_blocks.iter().map(|b| b.kind).collect::<Vec<_>>(),
            their_blocks.iter().map(|b| b.kind).collect::<Vec<_>>(),
            "{name}: block sequence"
        );

        // The `RR` service block carries the requested percent as its only
        // extra record, so it has to be identical on both sides.
        let records = |blocks: &[VolBlock]| -> Vec<Vec<u8>> {
            blocks
                .iter()
                .filter(|block| block.kind == 3)
                .map(|block| block.extra.clone())
                .collect()
        };
        assert_eq!(
            records(&our_blocks),
            records(&their_blocks),
            "{name}: RR record"
        );
        assert_eq!(
            records(&our_blocks).len(),
            1,
            "{name}: one recovery record per volume"
        );

        let our_data = our_blocks.iter().find(|b| b.kind == 2).expect("file block");
        let their_data = their_blocks
            .iter()
            .find(|b| b.kind == 2)
            .expect("file block");
        our_payload += our_data.data_size;
        their_payload += their_data.data_size;

        // Official reserves the padding inside the volume budget, so its
        // chunk is exactly the padding shorter; the last volume has no
        // padding and no reserve.
        if index + 1 < mine.len() {
            assert_eq!(
                our_data.data_size,
                their_data.data_size + their_padding as u64,
                "{name}: our chunk must cover what official pads"
            );
            assert!(
                their_padding > 0,
                "{name}: expected official to pad a non-final volume"
            );
        } else {
            assert_eq!(their_padding, 0, "{name}: last volume is not padded");
        }
    }
    assert_eq!(our_payload, total, "our volumes carry the whole payload");
    assert_eq!(
        their_payload, total,
        "official volumes carry the whole payload"
    );

    let first = rar_rs::discover_volumes(&path)
        .first()
        .expect("our set has a first volume")
        .clone();
    if let Some(verdict) = generate::unrar_test(generate::TOOL_RAR50, &first) {
        verdict.unwrap_or_else(|error| panic!("official UnRAR rejected our set: {error}"));
    }
}

/// Width of the locator's quick-open offset field, i.e. how much room the
/// writer reserved for an offset it only learns at close time.
fn locator_qo_width(archive: &[u8]) -> usize {
    let body = archive.strip_prefix(RAR5_SIG).expect("a RAR5 archive");
    let (head_size, size_width) = vint(body, 4).expect("head size");
    let head_end = 4 + size_width + usize::try_from(head_size).expect("head size");
    let mut q = 4 + size_width;
    let (_, n) = vint(body, q).expect("block type");
    q += n;
    let (flags, n) = vint(body, q).expect("block flags");
    q += n;
    assert!(
        flags & R5_BLOCK_EXTRA != 0,
        "the main header carries a locator"
    );
    let (extra_size, _) = vint(body, q).expect("extra size");
    let extra = &body[head_end - usize::try_from(extra_size).expect("extra size")..head_end];
    let mut r = 0usize;
    while r < extra.len() {
        let (record_size, n) = vint(extra, r).expect("record size");
        let (record_type, type_width) = vint(extra, r + n).expect("record type");
        let record_end = r + n + usize::try_from(record_size).expect("record size");
        if record_type == 1 {
            let mut q = r + n + type_width;
            let (locator_flags, n) = vint(extra, q).expect("locator flags");
            q += n;
            assert!(
                locator_flags & R5_LOCATOR_FLAG_QO != 0,
                "expected a quick-open offset field"
            );
            return record_end - q;
        }
        r = record_end;
    }
    panic!("no locator record in the main header");
}

/// Official RAR reserves the locator offset field by tier: 3 bytes below 512,
/// then 4 / 5 / 6 as the expected archive size passes 2^16 and 2^23. Its
/// estimate is computed from the member list it scanned, so a streaming
/// writer has to be told the size; with that estimate the reserved width, and
/// therefore the whole main header, matches byte for byte.
#[test]
fn locator_width_follows_official_size_tiers() {
    // From 2^20 bytes up, official also reserves wider `data_size`/`unp_size`
    // fields than the value needs (5 bytes from 2^20, 6 bytes from 2^23), so
    // the 8 MiB tier can only be compared on the locator width; the two
    // smaller tiers are boxed into sizes where official writes the natural
    // width and the whole archive is byte-identical.
    for (label, member, identical) in [
        ("4-byte tier", "tier_1k.bin", true),
        ("5-byte tier", "tier_100k.bin", true),
        ("6-byte tier", "tier_8m.bin", false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let size = generate::payload(member).len();
        let src = sample(dir.path(), member);
        let path = dir.path().join("ours.rar");
        let mut writer = ArchiveWriter::create_with(
            &path,
            WriterOptions::new()
                .compression(ArchiveVersion::V50)
                .quick_open(true)
                .estimated_size(size as u64),
        )
        .unwrap();
        writer
            .add_path_as(
                &src,
                member,
                EntryWriteOptions::new().compression_level(CompressionLevel::STORE),
            )
            .unwrap();
        writer.finish().unwrap();
        let mine = std::fs::read(&path).unwrap();

        let Some(made) = generate::archive_with(
            generate::TOOL_RAR50,
            &["-ma5", "-m0", "-qo+"],
            &[member],
            &[],
        ) else {
            return;
        };
        let theirs = made.read();
        let widths = (locator_qo_width(&mine), locator_qo_width(&theirs));
        assert_eq!(widths.0, widths.1, "{label}: reserved locator width");
        assert_eq!(
            widths.0,
            match label {
                "4-byte tier" => 4,
                "5-byte tier" => 5,
                _ => 6,
            },
            "{label}: unexpected locator width"
        );
        if identical && mine != theirs {
            let at = mine
                .iter()
                .zip(&theirs)
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| mine.len().min(theirs.len()));
            let lo = at.saturating_sub(16);
            panic!(
                "{label}: archive differs at byte {at} (ours {} bytes, official {} bytes)
  ours    {}
  official{}",
                mine.len(),
                theirs.len(),
                hex(&mine[lo..mine.len().min(at + 32)]),
                hex(&theirs[lo..theirs.len().min(at + 32)]),
            );
        }
    }
}

/// Width of the minimal vint that can hold `value`.
fn minimal_vint_width(value: u64) -> usize {
    let mut width = 1;
    let mut left = value >> 7;
    while left != 0 {
        left >>= 7;
        width += 1;
    }
    width
}

/// Field widths of the `data_size` and `unp_size` vints of the first file
/// block. Split member headers reserve these fields, so the widths are part of
/// the format even when the values agree.
fn rar5_size_widths(archive: &[u8]) -> Option<(usize, usize)> {
    let body = archive.get(RAR5_SIG.len()..)?;
    let mut p = 0usize;
    while p + 5 <= body.len() {
        let (head_size, size_width) = vint(body, p + 4)?;
        let head_end = p + 4 + size_width + usize::try_from(head_size).ok()?;
        if head_end > body.len() {
            return None;
        }
        let mut q = p + 4 + size_width;
        let (block_type, n) = vint(body, q)?;
        q += n;
        let (block_flags, n) = vint(body, q)?;
        q += n;
        if block_flags & R5_BLOCK_EXTRA != 0 {
            let (_, n) = vint(body, q)?;
            q += n;
        }
        let mut data_width = 0usize;
        let mut data_size = 0u64;
        if block_flags & R5_BLOCK_DATA != 0 {
            let (value, n) = vint(body, q)?;
            data_size = value;
            data_width = n;
            q += n;
        }
        if block_type == 2 {
            let (_, n) = vint(body, q)?;
            q += n;
            let (_, unp_width) = vint(body, q)?;
            return Some((data_width, unp_width));
        }
        p = head_end + usize::try_from(data_size).ok()?;
    }
    None
}

/// Official RAR does not write the smallest possible size vint in a split
/// member header: it reserves room for the size to grow, and the reserved
/// width steps with the member total (`3` below 2^20-512, `4` up to 2^20,
/// `5` up to 2^23, `6` above). We write the minimal width instead, which is
/// legal and strictly smaller.
///
/// That extra width makes official's *header* longer, so its chunks are
/// shorter and every volume drifts by a few bytes — the per-volume CRCs move
/// with the chunk boundaries, so a set this big cannot be compared byte by
/// byte. What must still hold: the same volume count and names, the same total
/// payload, and our fields being minimal.
#[test]
fn rar5_split_size_fields_are_reserved_wider_by_official() {
    let dir = tempfile::tempdir().unwrap();
    let member = "huge.bin";
    let total = generate::payload(member).len() as u64;
    assert!(total >= 1 << 20, "the payload must cross the first step");

    let mine = our_volume_set_with(dir.path(), member, ArchiveVersion::V50, false, 512 * 1024);
    let Some(made) = generate::archive_with(
        generate::TOOL_RAR50,
        &["-ma5", "-m0", "-qo-", "-v512k"],
        &[member],
        &[],
    ) else {
        return;
    };
    let theirs: Vec<(String, Vec<u8>)> = made
        .volumes()
        .into_iter()
        .map(|(name, bytes)| (name.trim_start_matches("arc").to_string(), bytes))
        .collect();

    assert_eq!(
        mine.iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>(),
        theirs
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>(),
        "volume naming"
    );
    assert!(mine.len() > 1, "the payload must span volumes");

    let (our_data_w, our_unp_w) = rar5_size_widths(&mine[0].1).expect("our size fields");
    let (their_data_w, their_unp_w) = rar5_size_widths(&theirs[0].1).expect("their size fields");
    assert_eq!(our_data_w, our_unp_w, "ours share one width");
    assert_eq!(their_data_w, their_unp_w, "theirs share one width");
    assert_eq!(
        our_data_w,
        minimal_vint_width(total),
        "we write the minimal width for the member total"
    );
    assert!(
        their_data_w > our_data_w,
        "official reserves room ({their_data_w}) beyond the minimal {our_data_w}"
    );

    for ((name, ours), (_, official)) in mine.iter().zip(&theirs) {
        // Each wider header pushes 4 more bytes of payload into the next
        // volume, so the drift accumulates and the last volume absorbs it.
        let drift = ours.len().abs_diff(official.len());
        let bound = 8 * mine.len();
        assert!(
            drift <= bound,
            "{name}: only the reserved width may shift the chunk ({drift} > {bound})"
        );
        // The header is where the reservation lives; the payload behind it is
        // the same bytes, just cut at a slightly different point.
        let (our_fields, their_fields) = (walk(ours).unwrap(), walk(official).unwrap());
        for (x, y) in our_fields.iter().zip(&their_fields) {
            assert_eq!(x.kind, y.kind, "{name}: block kind");
            for f in &x.fields {
                if matches!(
                    f.name,
                    "header_crc32" | "head_size" | "data_size" | "unp_size"
                ) {
                    continue;
                }
                let other = y.fields.iter().find(|g| g.name == f.name);
                if f.name == "file_crc32" {
                    // The CRC in a non-final volume covers the data up to the
                    // end of that volume, so it moves with the boundary.
                    assert!(other.is_some(), "{name}: official also stores a CRC");
                    continue;
                }
                assert_eq!(
                    other.map(|g| g.value.as_str()),
                    Some(f.value.as_str()),
                    "{name}: {}",
                    f.name
                );
            }
        }
    }

    // Both sides reassemble to the same payload: ours through our own reader,
    // official's verdict through its `UnRAR`.
    let first = dir.path().join("ours.part1.rar");
    let mut reader = rar_rs::ArchiveReader::open(&first).unwrap();
    let ids: Vec<_> = reader.entries().map(|entry| entry.id()).collect();
    let mut rebuilt = Vec::new();
    for id in ids {
        rebuilt.extend(reader.read_entry(id).unwrap());
    }
    assert_eq!(rebuilt.len() as u64, total, "our set reassembles");
    assert_eq!(rebuilt, generate::payload(member), "same payload bytes");
    let verdict = generate::unrar_test(generate::TOOL_RAR50, &first);
    if let Some(verdict) = verdict {
        verdict.unwrap_or_else(|e| panic!("official unrar rejects our split set: {e}"));
    }
}

/// WinRAR pads the part number to the digit count of the volume total, so a
/// set with more than 99 volumes names them `part001…`. Ours has to agree.
#[test]
fn volume_names_pad_to_the_digits_of_the_volume_count() {
    let dir = tempfile::tempdir().unwrap();
    let mine = our_volume_set_with(dir.path(), "huge.bin", ArchiveVersion::V50, false, 8 * 1024);
    let Some(made) = generate::archive_with(
        generate::TOOL_RAR50,
        &["-ma5", "-m0", "-qo-", "-v8k"],
        &["huge.bin"],
        &[],
    ) else {
        return;
    };
    let theirs: Vec<String> = made
        .volumes()
        .into_iter()
        .map(|(name, _)| name.trim_start_matches("arc").to_string())
        .collect();
    let ours: Vec<String> = mine.into_iter().map(|(name, _)| name).collect();
    assert!(ours.len() > 99, "the set must reach three digits");
    assert_eq!(ours, theirs, "volume naming");
    // Three digits for the whole set: `part001` … `part<count>`.
    assert_eq!(ours[0], ".part001.rar", "first volume name");
    assert_eq!(
        ours[99], ".part100.rar",
        "the hundredth volume keeps the padding"
    );
    let last = ours.len();
    assert_eq!(ours[last - 1], format!(".part{last:03}.rar"), "last volume");
}

/// The quick-open record is a cache of the file headers, so it cannot survive
/// an edit untouched: a delete has to drop the entry, an append has to add one,
/// and every surviving cached offset has to move with the headers it names.
/// Our writer rebuilds the whole record on every rewrite instead of copying the
/// old one, which keeps the catalog valid without a full re-scan.
#[test]
fn edits_rebuild_the_quick_open_record() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("edited.rar");
    let members = ["text.txt", "noise.bin", "code.exe"];
    let mut writer =
        ArchiveWriter::create_with(&archive, WriterOptions::new().quick_open(true)).unwrap();
    for member in members {
        let src = sample(dir.path(), member);
        writer
            .add_path_as(&src, member, EntryWriteOptions::new())
            .unwrap();
    }
    writer.finish().unwrap();

    let before = std::fs::read(&archive).unwrap();
    assert_eq!(cached_names(&before), members);

    // A delete rewrites the archive. The record must lose the deleted member
    // and keep the other two pointing at their new on-disk positions.
    let mut editor = ArchiveEditor::open(&archive).unwrap();
    let victim = editor.unique_entry("noise.bin").unwrap();
    editor.apply(EditPlan::new().delete(victim)).unwrap();
    let deleted = std::fs::read(&archive).unwrap();
    assert!(deleted.len() < before.len(), "the deleted payload is gone");
    assert_eq!(cached_names(&deleted), ["text.txt", "code.exe"]);

    // An append caches the new member as well: the writer truncates the old
    // record and writes a fresh one for the whole archive.
    let mut writer = ArchiveWriter::append(&archive).unwrap();
    let added = sample(dir.path(), "data.bin");
    writer
        .add_path_as(&added, "data.bin", EntryWriteOptions::new())
        .unwrap();
    writer.finish().unwrap();
    assert_eq!(
        cached_names(&std::fs::read(&archive).unwrap()),
        ["text.txt", "code.exe", "data.bin"]
    );
    // The rebuilt catalog is what official UnRAR sees too.
    if let Some(verdict) = generate::unrar_test(generate::TOOL_RAR50, &archive) {
        verdict
            .unwrap_or_else(|error| panic!("official UnRAR rejected the edited archive: {error}"));
    }
}

/// An archive without a quick-open record must not grow one through an edit:
/// the presence of the record is decided by the archive, not by the editor.
#[test]
fn edits_do_not_invent_a_quick_open_record() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("plain.rar");
    let mut writer = ArchiveWriter::create_with(&archive, WriterOptions::new()).unwrap();
    for member in ["text.txt", "noise.bin"] {
        let src = sample(dir.path(), member);
        writer
            .add_path_as(&src, member, EntryWriteOptions::new())
            .unwrap();
    }
    writer.finish().unwrap();
    assert!(qo_catalog(&std::fs::read(&archive).unwrap()).is_none());

    let mut editor = ArchiveEditor::open(&archive).unwrap();
    let victim = editor.unique_entry("noise.bin").unwrap();
    editor.apply(EditPlan::new().delete(victim)).unwrap();
    assert!(qo_catalog(&std::fs::read(&archive).unwrap()).is_none());
}

/// CRC-32 (IEEE), the polynomial every archive checksum in the format uses.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}
