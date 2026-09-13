//! Structured mutators for the recovery / recovery-volume / legacy fuzz
//! targets.
//!
//! Plain byte-level fuzzing virtually never passes the recovery-record
//! checksum gates — CRC64-XZ over each `{RB}` inline chunk, CRC32 over the
//! REV5 header, CRC32 over the rev3 trailer and the 16-bit RAR4 header CRC
//! — so the shard arithmetic, Reed-Solomon solve and naming/layout code
//! stay unreachable. These helpers start from *valid* records (built with
//! `rar_rs::wire` or embedded fixtures) and mutate plan/geometry fields and
//! flip bytes **with the checksum recomputed**, so the mutated record still
//! parses and the mutation reaches the code behind the gate.
//!
//! Everything is deterministic for a given [`Rng`](crate::Rng) state; the
//! fuzz targets seed the PRNG from the input bytes so a crash reproduces.

use crate::Rng;

/// One structured mutation plus a short label for crash / failure reports.
pub type Case = (Vec<u8>, &'static str);

// ── Small helpers ──────────────────────────────────────────────────────────

/// FNV-1a 64: the per-input PRNG seed (deterministic, no external state).
pub fn seed_from_bytes(data: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Standard IEEE CRC-32 (reflected `0xEDB88320`, init/final `!0`), matching
/// `crc32fast` — the checksum RAR4 headers, REV5 headers and rev3 trailers
/// are built on.
pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// The 16-bit rolling checksum RAR 1.3/1.4 stamps on member data.
pub fn rar13_checksum(data: &[u8]) -> u16 {
    let mut value = 0u16;
    for &byte in data {
        value = value.wrapping_add(u16::from(byte)).rotate_left(1);
    }
    value
}

fn u16le(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buf[at], buf[at + 1]])
}

fn u32le(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

// ── RAR5 inline `{RB}` recovery chunks ─────────────────────────────────────

/// Fixed part of a `{RB}` chunk header (`RAR5_RECOVERY_CHUNK_FIXED_HEADER_SIZE`).
const RB_FIXED_HEADER: usize = 0x48;

/// Recompute the CRC64-XZ field (`0x04..0x0c`) of the `{RB}` chunk at
/// `start` over the current `[start+0x0c .. start+total_size]` bytes, when
/// that range fits the buffer.
pub fn refresh_rb_crc(input: &mut [u8], start: usize) {
    let Some(size_at) = input.get(start + 0x0c..start + 0x10) else {
        return;
    };
    let total = u32le(size_at, 0) as usize;
    let Some(end) = start.checked_add(total) else {
        return;
    };
    if total < 0x10 || end > input.len() {
        return;
    }
    let crc = rar_rs::wire::crc64_xz(&input[start + 0x0c..end]);
    input[start + 0x04..start + 0x0c].copy_from_slice(&crc.to_le_bytes());
}

/// Synthesize a parser-valid `{RB}` chunk for `prefix`.
///
/// Shard states are computed from the real prefix bytes, so the record is
/// repairable; the shard at `damage` deliberately records a wrong state so
/// the repair path has exactly one damaged shard (relocation parse + RS
/// solve). All parser cross-checks hold and the CRC64-XZ is computed last.
/// Counts are clamped to keep allocations bounded (geometry, not size, is
/// the fuzzing target).
pub fn synthetic_rb_chunk(
    prefix: &[u8],
    data_shards: u16,
    recovery_shards: u16,
    group_count: u64,
    shard_index: u16,
    damage: Option<usize>,
    rng: &mut Rng,
) -> Vec<u8> {
    let ds = usize::from(data_shards.clamp(1, 512));
    let rs = recovery_shards.clamp(1, 256);
    let gc = group_count.clamp(2, 64 * 1024) as usize;
    let header_size = RB_FIXED_HEADER + ds * 8;
    let total = header_size + gc;

    let mut out = vec![0u8; total];
    out[..4].copy_from_slice(b"{RB}");
    out[0x0c..0x10].copy_from_slice(&(total as u32).to_le_bytes());
    out[0x10..0x14].copy_from_slice(&(header_size as u32).to_le_bytes());
    out[0x14] = 1;
    out[0x15] = 1;
    // 0x16..0x1e: remaining structural fields stay zero.
    let last_extent = prefix
        .len()
        .saturating_sub(ds.saturating_sub(1) * gc)
        .min(gc);
    out[0x1e..0x22].copy_from_slice(&(last_extent as u32).to_le_bytes());
    out[0x22..0x2a].copy_from_slice(&(prefix.len() as u64).to_le_bytes());
    out[0x2a..0x32].copy_from_slice(&(gc as u64).to_le_bytes());
    out[0x32..0x3a].copy_from_slice(&(total as u64).to_le_bytes());
    out[0x3a..0x3c].copy_from_slice(&(ds as u16).to_le_bytes());
    out[0x3c..0x3e].copy_from_slice(&rs.to_le_bytes());
    out[0x3e..0x40].copy_from_slice(&(shard_index % rs).to_le_bytes());
    for index in 0..ds {
        let start = index.saturating_mul(gc).min(prefix.len());
        let end = start.saturating_add(gc).min(prefix.len());
        let mut state = rar_rs::wire::crc64_rar_state(&prefix[start..end]);
        if damage == Some(index) {
            state ^= 0x00ff_00ff_00ff_00ff;
        }
        let at = 0x40 + index * 8;
        out[at..at + 8].copy_from_slice(&state.to_le_bytes());
    }
    let final_at = 0x40 + ds * 8;
    out[final_at..final_at + 8].copy_from_slice(&rng.next_u64().to_le_bytes());
    for byte in &mut out[header_size..] {
        *byte = rng.next_u64() as u8;
    }
    refresh_rb_crc(&mut out, 0);
    out
}

/// Build a valid `{RB}` record for `prefix` with the public wire builder and
/// derive structured mutations that all still parse:
///
/// * the intact record and a genuinely damaged prefix (full repair cycle),
/// * field/parity flips with the CRC64-XZ recomputed,
/// * synthesized plans with exactly one damaged shard (RS solve path),
/// * hostile shard geometry the writer never emits — an odd `group_count`
///   and a reversed shard range (`(data_shards-1) * group_count >
///   prefix_len`) — classified by the repair path, not filtered out,
/// * truncations at structural boundaries.
pub fn inline_rr_cases(prefix: &[u8], pct: u64, rng: &mut Rng) -> Vec<Case> {
    let mut cases = Vec::new();
    let Ok(built) = rar_rs::wire::build_structural_inline_recovery_data(prefix, pct) else {
        return cases;
    };
    if built.len() < 0x40 {
        return cases;
    }
    let total = u32le(&built, 0x0c) as usize;
    let header_size = u32le(&built, 0x10) as usize;
    if total < RB_FIXED_HEADER
        || total > built.len()
        || header_size < RB_FIXED_HEADER
        || header_size > total
    {
        return cases;
    }
    let start = prefix.len();
    let mut full = prefix.to_vec();
    full.extend_from_slice(&built[..total]);

    // 1. Intact archive: repair must be idempotent.
    cases.push((full.clone(), "intact record"));

    // 2. Real damage inside the prefix: repair must restore the original.
    if !prefix.is_empty() {
        let mut damaged = full.clone();
        let at = rng.below(prefix.len());
        damaged[at] ^= 1 << rng.below(8);
        cases.push((damaged, "damaged prefix"));
    }

    // 3. Header/parity flips with the CRC recomputed: the record still
    // parses, so geometry checks and shard math run on hostile values.
    for _ in 0..3 {
        let mut case = full.clone();
        let at = start + 0x0c + rng.below(total - 0x0c);
        case[at] ^= 1 << rng.below(8);
        refresh_rb_crc(&mut case, start);
        cases.push((case, "field/parity flip with CRC"));
    }

    // 4. Synthesized plans, one damaged shard each. Hostile geometry
    // (reversed shard ranges included) is in scope: `split_prefix_shard_ranges`
    // clamps the start, so the repair path must classify these.
    for _ in 0..4 {
        let gc = 2 * (1 + rng.below(512)) as u64;
        let ds = (1 + rng.below(64)) as u16;
        if u64::from(ds) * gc < prefix.len() as u64 {
            continue;
        }
        let rs = (1 + rng.below(4)) as u16;
        let shard_index = rng.below(usize::from(rs)) as u16;
        let damaged = rng.below(usize::from(ds));
        let chunk = synthetic_rb_chunk(prefix, ds, rs, gc, shard_index, Some(damaged), rng);
        let mut case = prefix.to_vec();
        case.extend_from_slice(&chunk);
        cases.push((case, "synthetic plan, one damaged shard"));
    }

    // 5. Reversed shard range: the last shard starts past the prefix end
    // while `data_shards * group_count` still covers it. The writer never
    // emits this; a damaged/crafted record can. Regression:
    // `reversed_shard_range_geometry_must_not_panic`.
    {
        let gc = 16u64 << rng.below(3);
        let ds = (prefix.len() as u64 / gc + 2) as u16;
        let chunk = synthetic_rb_chunk(prefix, ds, 1, gc, 0, Some(0), rng);
        let mut case = prefix.to_vec();
        case.extend_from_slice(&chunk);
        cases.push((case, "reversed shard range"));
    }

    // 6. Odd group_count: rejected before the word-pair loops (regression
    // for the former panic), with a parity length that matches it.
    let mut case = prefix.to_vec();
    case.extend_from_slice(&synthetic_rb_chunk(prefix, 2, 1, 0x11, 0, None, rng));
    cases.push((case, "odd group_count"));

    // 7. Truncations at structural boundaries.
    for cut in [
        0usize,
        3,
        0x0c,
        0x40,
        header_size,
        header_size + 1,
        total - 1,
    ] {
        if cut < total {
            cases.push((full[..start + cut].to_vec(), "structural truncation"));
        }
    }
    cases
}

/// Mutate the `{RB}` chunks of a real archive in place, refreshing the
/// CRC64-XZ so the record still parses: field/parity flips and a truncation
/// inside the chunk. Finds up to four chunks (WinRAR archives may carry
/// several); offsets are located from the `{RB}` markers, not the container.
pub fn rr_archive_mutations(archive: &[u8], rng: &mut Rng) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut offset = 0usize;
    let mut found = 0usize;
    while offset + 4 <= archive.len() && found < 4 {
        let Some(relative) = archive[offset..].iter().position(|&byte| byte == b'{') else {
            break;
        };
        let at = offset + relative;
        if archive.get(at..at + 4) != Some(b"{RB}") {
            offset = at + 1;
            continue;
        }
        let Some(size_at) = archive.get(at + 0x0c..at + 0x10) else {
            break;
        };
        let total = u32le(size_at, 0) as usize;
        if total < RB_FIXED_HEADER || at + total > archive.len() {
            offset = at + 1;
            continue;
        }
        found += 1;
        for _ in 0..2 {
            let mut case = archive.to_vec();
            let pos = at + 0x0c + rng.below(total - 0x0c);
            case[pos] ^= 1 << rng.below(8);
            refresh_rb_crc(&mut case, at);
            cases.push((case, "real {RB} field flip"));
        }
        let cut = at + 0x10 + rng.below((total - 0x10).max(1));
        cases.push((archive[..cut].to_vec(), "real {RB} truncation"));
        offset = at + total;
    }
    cases
}

// ── REV5 `.rev` files ──────────────────────────────────────────────────────
const REV5_SIGNATURE: &[u8; 8] = b"Rar!\x1aRev";

/// Recompute the REV5 header CRC32 (`8..12`) over the writer's coverage
/// (`12..16 + header_size`) when that range fits.
fn refresh_rev5_header_crc(file: &mut [u8]) {
    if file.len() < 16 {
        return;
    }
    let header_size = u32le(file, 12) as usize;
    let Some(end) = 16usize.checked_add(header_size) else {
        return;
    };
    if end > file.len() || header_size == 0 {
        return;
    }
    let crc = crc32_ieee(&file[12..end]);
    file[8..12].copy_from_slice(&crc.to_le_bytes());
}

/// Structured mutations of a valid REV5 `.rev` file: version/count/rev
/// number/header-size/volume-entry fields and the parity payload, each with
/// the header CRC (and payload CRC where applicable) recomputed, plus
/// truncations at the structural boundaries.
pub fn rev5_mutations(file: &[u8], rng: &mut Rng) -> Vec<Case> {
    let mut cases = Vec::new();
    if file.len() < 51 || &file[..8] != REV5_SIGNATURE {
        return cases;
    }
    let header_size = u32le(file, 12) as usize;
    let header_end = 16usize.saturating_add(header_size);
    if header_size < 13 || header_end > file.len() {
        return cases;
    }
    let data_count = usize::from(u16le(file, 17));
    if 27 + data_count * 12 > file.len() {
        return cases;
    }

    // Version byte.
    let mut case = file.to_vec();
    case[16] = case[16].wrapping_add(1);
    refresh_rev5_header_crc(&mut case);
    cases.push((case, "rev5 version"));

    // Counts and rev number.
    for (at, label) in [
        (17usize, "rev5 data_count"),
        (19, "rev5 rec_count"),
        (21, "rev5 rev_number"),
    ] {
        let mut case = file.to_vec();
        let value = u16le(&case, at).wrapping_add(1);
        case[at..at + 2].copy_from_slice(&value.to_le_bytes());
        refresh_rev5_header_crc(&mut case);
        cases.push((case, label));
    }

    // Header-size field: the CRC coverage moves with the field.
    for delta in [-4i64, 4] {
        let mut case = file.to_vec();
        let value = (header_size as i64 + delta).clamp(1, file.len() as i64) as u32;
        case[12..16].copy_from_slice(&value.to_le_bytes());
        refresh_rev5_header_crc(&mut case);
        cases.push((case, "rev5 header_size"));
    }

    // Payload flip with the payload CRC recomputed: both gates pass, so the
    // parity math runs on altered symbols.
    if file.len() > header_end {
        let mut case = file.to_vec();
        let at = header_end + rng.below(case.len() - header_end);
        case[at] ^= 1 << rng.below(8);
        let payload_crc = crc32_ieee(&case[header_end..]);
        case[23..27].copy_from_slice(&payload_crc.to_le_bytes());
        refresh_rev5_header_crc(&mut case);
        cases.push((case, "rev5 payload flip with CRC"));
    }

    // Volume metadata entries (size and recorded CRC).
    for _ in 0..2 {
        if data_count == 0 {
            break;
        }
        let at = 27 + rng.below(data_count) * 12;
        let mut case = file.to_vec();
        if rng.below(2) == 0 {
            let value = u64::from_le_bytes(case[at..at + 8].try_into().unwrap()).wrapping_add(1);
            case[at..at + 8].copy_from_slice(&value.to_le_bytes());
        } else {
            let value = u32le(&case, at + 8).wrapping_add(1);
            case[at + 8..at + 12].copy_from_slice(&value.to_le_bytes());
        }
        refresh_rev5_header_crc(&mut case);
        cases.push((case, "rev5 volume entry"));
    }

    // Truncations at structural boundaries.
    for cut in [
        16usize,
        header_end.saturating_sub(1),
        header_end,
        file.len().saturating_sub(1),
    ] {
        if cut < file.len() {
            cases.push((file[..cut].to_vec(), "rev5 truncation"));
        }
    }
    cases
}

// ── rev3 trailer `.rev` files ──────────────────────────────────────────────

/// Structured mutations of a rev3 `.rev` file: the three trailer metadata
/// bytes and the parity payload, with the trailer CRC32 recomputed (the
/// `parse_trailer` gate), a legacy pure-parity variant, and truncations.
pub fn rev3_trailer_mutations(file: &[u8], rng: &mut Rng) -> Vec<Case> {
    let mut cases = Vec::new();
    let len = file.len();
    if len < 7 {
        return cases;
    }
    let refresh = |case: &mut [u8]| {
        let len = case.len();
        let crc = crc32_ieee(&case[..len - 4]);
        case[len - 4..].copy_from_slice(&crc.to_le_bytes());
    };

    // Trailer metadata: data_count-1, rec_count-1, recovery_index.
    for (offset, label) in [
        (7usize, "rev3 data_count"),
        (6, "rev3 rec_count"),
        (5, "rev3 recovery_index"),
    ] {
        let mut case = file.to_vec();
        let at = len - offset;
        case[at] = case[at].wrapping_add(1);
        refresh(&mut case);
        cases.push((case, label));
    }

    // Parity payload flips with the trailer CRC recomputed.
    for _ in 0..3 {
        if len <= 7 {
            break;
        }
        let mut case = file.to_vec();
        let at = rng.below(len - 7);
        case[at] ^= 1 << rng.below(8);
        refresh(&mut case);
        cases.push((case, "rev3 payload flip"));
    }

    // Truncations across the trailer / CRC boundary.
    for cut in [len - 7, len - 4, len - 1] {
        cases.push((file[..cut].to_vec(), "rev3 truncation"));
    }
    cases
}

// ── RAR4 / RAR13 block envelopes ───────────────────────────────────────────

/// Structured mutations of a valid small RAR4 or RAR13 archive: block
/// header types, flags, sizes and bodies — with the 16-bit RAR4 header CRC
/// recomputed where the reader verifies it — plus truncations at block
/// boundaries.
pub fn legacy_block_cases(seed: &[u8], rng: &mut Rng) -> Vec<Case> {
    if seed.starts_with(b"RE~^") {
        rar13_cases(seed, rng)
    } else if seed.starts_with(b"Rar!\x1a\x07\x00") {
        rar4_cases(seed, rng)
    } else {
        let mut case = seed.to_vec();
        if !case.is_empty() {
            let at = rng.below(case.len());
            case[at] ^= 1 << rng.below(8);
        }
        vec![(case, "legacy raw flip")]
    }
}

/// Where the reader's 16-bit header CRC coverage ends for a RAR4 block
/// (mirrors `format::rar4::header_crc_end` for the shapes small fixtures
/// use).
fn rar4_crc_end(bytes: &[u8], start: usize, head_size: usize) -> usize {
    let head_type = bytes[start + 2];
    let flags = u16le(bytes, start + 3);
    let mut end = head_size;
    if head_type == 0x73 && flags & 0x0002 != 0 {
        end = 13.min(head_size);
    } else if head_type == 0x74 && flags & 0x0008 != 0 {
        // FILE_HEAD with a nested comment: named-block extent approximation.
        let mut covered = 32usize.min(head_size);
        if flags & 0x0100 != 0 {
            covered += 8;
        }
        if start + 28 <= bytes.len() {
            covered += usize::from(u16le(bytes, start + 26));
        }
        if flags & 0x0400 != 0 {
            covered += 8;
        }
        end = covered.min(head_size);
    }
    end.min(bytes.len() - start)
}

fn recompute_rar4_crc(bytes: &mut [u8], start: usize, head_size: usize) {
    let end = start + rar4_crc_end(bytes, start, head_size);
    if end <= start + 2 || end > bytes.len() {
        return;
    }
    let crc = (crc32_ieee(&bytes[start + 2..end]) & 0xffff) as u16;
    bytes[start..start + 2].copy_from_slice(&crc.to_le_bytes());
}

fn rar4_cases(seed: &[u8], rng: &mut Rng) -> Vec<Case> {
    const LONG_BLOCK: u16 = 0x8000;
    let mut cases = Vec::new();
    let mut pos = 7usize;
    let mut block_count = 0;
    while pos + 7 <= seed.len() && block_count < 8 {
        block_count += 1;
        let head_size = usize::from(u16le(seed, pos + 5));
        if head_size < 7 || pos + head_size > seed.len() {
            break;
        }
        let flags = u16le(seed, pos + 3);
        let add_size = if flags & LONG_BLOCK != 0 && pos + 11 <= seed.len() {
            u32le(seed, pos + 7) as usize
        } else {
            0
        };
        let total = head_size + add_size;

        // Block type variants.
        for head_type in [0x72u8, 0x73, 0x74, 0x75, 0x77, 0x7a, 0x7b, 0xff] {
            let mut case = seed.to_vec();
            case[pos + 2] = head_type;
            recompute_rar4_crc(&mut case, pos, head_size);
            cases.push((case, "rar4 head_type"));
        }
        // Flag variants (LONG_BLOCK / split / comment / large / ext-time / …).
        for value in [
            0x0000u16, 0x0001, 0x0002, 0x0004, 0x0008, 0x0100, 0x0200, 0x0400, 0x1000, 0x8000,
            0xffff,
        ] {
            let mut case = seed.to_vec();
            case[pos + 3..pos + 5].copy_from_slice(&value.to_le_bytes());
            recompute_rar4_crc(&mut case, pos, head_size);
            cases.push((case, "rar4 flags"));
        }
        // Head-size variants (some extend, some truncate the header).
        for value in [7usize, 11, 13, 32, 0xffff] {
            if value == head_size {
                continue;
            }
            let mut case = seed.to_vec();
            case[pos + 5..pos + 7].copy_from_slice(&(value as u16).to_le_bytes());
            recompute_rar4_crc(&mut case, pos, value);
            cases.push((case, "rar4 head_size"));
        }
        // Data-size variants for LONG_BLOCK headers.
        if pos + 11 <= seed.len() && flags & LONG_BLOCK != 0 {
            for value in [0u32, 1, u32::MAX] {
                let mut case = seed.to_vec();
                case[pos + 7..pos + 11].copy_from_slice(&value.to_le_bytes());
                recompute_rar4_crc(&mut case, pos, head_size);
                cases.push((case, "rar4 add_size"));
            }
        }
        // One body byte with the CRC recomputed (the header still parses).
        if head_size > 11 {
            let at = pos + 7 + rng.below(head_size - 7);
            let mut case = seed.to_vec();
            case[at] ^= 0x40;
            recompute_rar4_crc(&mut case, pos, head_size);
            cases.push((case, "rar4 body flip with CRC"));
        }
        // CRC rejection paths: zeroed and the 0xFFFF "no CRC" sentinel.
        let mut case = seed.to_vec();
        case[pos..pos + 2].copy_from_slice(&0u16.to_le_bytes());
        cases.push((case, "rar4 head_crc=0"));
        let mut case = seed.to_vec();
        case[pos..pos + 2].copy_from_slice(&0xffffu16.to_le_bytes());
        cases.push((case, "rar4 head_crc=sentinel"));
        // Truncations at the block boundary.
        if pos + total <= seed.len() {
            cases.push((seed[..pos + total].to_vec(), "rar4 truncate at block end"));
            if total > 0 {
                cases.push((
                    seed[..pos + total - 1].to_vec(),
                    "rar4 truncate one before block end",
                ));
            }
        }
        if total == 0 {
            break;
        }
        pos += total;
    }
    for cut in [7usize, 8, 11] {
        if cut < seed.len() {
            cases.push((seed[..cut].to_vec(), "rar4 prefix truncation"));
        }
    }
    cases
}

fn rar13_cases(seed: &[u8], rng: &mut Rng) -> Vec<Case> {
    let mut cases = Vec::new();
    if seed.len() < 7 {
        return cases;
    }
    let main_size = usize::from(u16le(seed, 4));
    // Main-header flag / size variants (the RAR13 container has no header
    // CRC; the flags drive solid/split/comment handling).
    for flags in [0x00u8, 0x80, 0x81, 0x82, 0x88, 0x90, 0x0f, 0xff] {
        let mut case = seed.to_vec();
        case[6] = flags;
        cases.push((case, "rar13 main flags"));
    }
    for value in [7usize, main_size.saturating_sub(1).max(7), main_size + 4] {
        if value == main_size || value > usize::from(u16::MAX) {
            continue;
        }
        let mut case = seed.to_vec();
        case[4..6].copy_from_slice(&(value as u16).to_le_bytes());
        cases.push((case, "rar13 main head_size"));
    }

    let mut pos = main_size.max(7);
    let mut count = 0;
    while pos + 21 <= seed.len() && count < 8 {
        count += 1;
        let pack = u32le(seed, pos);
        let head_size = usize::from(u16le(seed, pos + 10));
        let name_size = usize::from(seed[pos + 19]);
        if head_size < 21 + name_size || pos + head_size > seed.len() {
            break;
        }
        let data_start = pos + head_size;
        let data_end = data_start.saturating_add(pack as usize).min(seed.len());

        // Fixed-field variants.
        let mut case = seed.to_vec();
        case[pos..pos + 4].copy_from_slice(&pack.wrapping_add(1).to_le_bytes());
        cases.push((case, "rar13 pack_size"));
        let mut case = seed.to_vec();
        let value = u32le(seed, pos + 4).wrapping_add(1);
        case[pos + 4..pos + 8].copy_from_slice(&value.to_le_bytes());
        cases.push((case, "rar13 unp_size"));
        let mut case = seed.to_vec();
        case[pos + 10..pos + 12].copy_from_slice(&(head_size as u16).wrapping_add(8).to_le_bytes());
        cases.push((case, "rar13 head_size"));
        let mut case = seed.to_vec();
        case[pos + 19] = (name_size as u8).wrapping_add(4);
        cases.push((case, "rar13 name_size"));
        let mut case = seed.to_vec();
        case[pos + 20] = 5;
        cases.push((case, "rar13 method"));
        let mut case = seed.to_vec();
        case[pos + 17] ^= 0x1f;
        cases.push((case, "rar13 flags"));
        // Data flip with the 16-bit rolling checksum recomputed (STORE
        // members) so the integrity check runs on the mutated payload.
        if data_end > data_start && seed[pos + 20] == 0 {
            let at = data_start + rng.below(data_end - data_start);
            let mut case = seed.to_vec();
            case[at] ^= 0x20;
            let checksum = rar13_checksum(&case[data_start..data_end]);
            case[pos + 8..pos + 10].copy_from_slice(&checksum.to_le_bytes());
            cases.push((case, "rar13 checksum refreshed"));
        }
        // Truncations at the header / data boundaries.
        cases.push((
            seed[..pos + head_size].to_vec(),
            "rar13 truncate at header end",
        ));
        if data_end > data_start {
            cases.push((
                seed[..data_end - 1].to_vec(),
                "rar13 truncate before data end",
            ));
        }
        if pack == 0 {
            break;
        }
        pos = data_start + pack as usize;
    }
    for cut in [4usize, 7, 10] {
        if cut < seed.len() {
            cases.push((seed[..cut].to_vec(), "rar13 prefix truncation"));
        }
    }
    cases
}
