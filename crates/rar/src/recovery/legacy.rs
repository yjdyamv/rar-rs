//! Legacy (RAR 1.5–4.x) recovery-record repair — the RAR 3-style
//! `PROTECT_HEAD` (0x78) inline recovery record.
//!
//! The recovery record covers the archive prefix in 512-byte sectors. Each
//! sector's CRC16 (the low 16 bits of the ones-complemented CRC32) is stored
//! in a tag table, and `rec_sectors` XOR parity sectors follow it. Repair =
//! compare stored tags against recomputed ones, then XOR the surviving
//! sectors of each parity group back into the parity to rebuild the damaged
//! sector. Ported from the `rars` project
//! (<https://github.com/bitplane/rars>, `repair_protect_head_bytes`), licensed
//! MIT OR Apache-2.0, at upstream revision `c08a17b`; validated against
//! genuine RAR 2.5/3.x archives. See NOTICE for attribution and the unresolved
//! workspace-metadata/COPYING difference.
//!
//! Two on-disk block shapes carry the same data layout:
//!
//! - `PROTECT_HEAD` (0x78), the RAR 2.5-era form: a 26-byte header whose
//!   `[12..14]` rec_sectors (u16), `[14..18]` total_blocks (u32) and
//!   `[18..26]` = `Protect!` mark sit in the fixed header.
//! - `NEWSUB` (0x7a) named `RR`, the RAR 3.x/4.x form WinRAR 6.23 writes:
//!   a FILE_HEAD-shaped header (32 fixed + name), then after the `RR` name a
//!   20-byte tail = `Protect+` + rec_sectors (u32) + total_blocks (u32) +
//!   zero. The data area (tags + parity) follows the header either way.
//!
//! The write side (`build_legacy_recovery_block`) emits the NEWSUB form, the
//! one WinRAR's RAR4 writer produces, so its own repair path handles it.
//!
//! Only archives whose creator added `-rr` are repairable — without the
//! redundant parity there is nothing to rebuild from. The legacy `.rev`
//! recovery volumes are a separate follow-up.

use crate::detect::RAR4_SIGNATURE;
use crate::error::{RarError, RarResult};

/// `PROTECT_HEAD` / `RR` NEWSUB parsed from a legacy archive
/// (file-absolute positions).
#[derive(Debug, Clone)]
pub(crate) struct Rar4Protect {
    /// Number of 512-byte parity sectors.
    pub rec_sectors: u32,
    /// Number of 512-byte sectors the record declares as protected.
    pub total_blocks: u32,
    /// Must be the 8 bytes `Protect!` (0x78) or `Protect+` (0x7a).
    pub mark: [u8; 8],
    /// File-absolute offset of the recovery block's data area (tag table +
    /// parity sectors).
    pub data_start: usize,
    /// File-absolute end of the recovery block's data area.
    pub data_end: usize,
    /// File-absolute offset where the recovery block itself starts.
    pub block_offset: usize,
}

/// Whether a byte stream carries a legacy RAR4 `PROTECT_HEAD` recovery
/// record, and where the archive (signature) starts.
pub(crate) struct Rar4ProtectScan {
    pub sfx_offset: usize,
    pub protect: Option<Rar4Protect>,
}

/// Find the RAR4 signature inside `bytes` (SFX stubs allowed, bounded like
/// the reader's own scan) and walk the blocks looking for a recovery record
/// (either the PROTECT_HEAD 0x78 or the NEWSUB 0x7a `RR` form).
///
/// Header-encrypted (`-hp`) archives need [`scan_protect_with_password`]:
/// this wrapper only sees plaintext block headers.
#[allow(dead_code)] // kept as the plaintext shortcut; tests exercise it directly
pub(crate) fn scan_protect(bytes: &[u8]) -> RarResult<Rar4ProtectScan> {
    scan_protect_with_password(bytes, None)
}

/// [`scan_protect`] for an archive that may be `-hp` header-encrypted.
///
/// The main header is always plaintext and carries `MHD_PASSWORD`; every
/// block after it is `[8B salt][AES-128-CBC header][plaintext data]`, so the
/// scan decrypts each header before reading its fields (the record's own
/// data area — tags and parity — is never encrypted). `password` is ignored
/// for plaintext archives and required for encrypted ones.
pub(crate) fn scan_protect_with_password(
    bytes: &[u8],
    password: Option<&[u8]>,
) -> RarResult<Rar4ProtectScan> {
    scan_protect_impl(bytes, password, false)
}

/// [`scan_protect_with_password`] tolerating corrupt block headers: a block
/// that cannot be parsed or whose declared size runs past the archive makes
/// the walk resync to the next valid header instead of aborting, so the record
/// is still found when an earlier block is damaged. Only the repair path uses
/// it; the edit path needs the strict scan (a malformed archive is an error
/// there).
pub(crate) fn scan_protect_tolerant(
    bytes: &[u8],
    password: Option<&[u8]>,
) -> RarResult<Rar4ProtectScan> {
    scan_protect_impl(bytes, password, true)
}

fn scan_protect_impl(
    bytes: &[u8],
    password: Option<&[u8]>,
    tolerant: bool,
) -> RarResult<Rar4ProtectScan> {
    let sig = find_bytes(bytes, RAR4_SIGNATURE, 8 * 1024 * 1024)
        .ok_or_else(|| RarError::Format("not a RAR4 archive (signature not found)".into()))?;
    let mut stream = std::io::Cursor::new(bytes);
    let protect = scan_protect_stream(
        &mut stream,
        sig + RAR4_SIGNATURE.len(),
        bytes.len(),
        password,
        tolerant,
    )?;
    Ok(Rar4ProtectScan {
        sfx_offset: sig,
        protect,
    })
}

/// [`scan_protect_with_password`] over a file path: only the signature
/// prefix and the block headers are read, so a multi-GiB archive is scanned
/// with bounded memory instead of being loaded whole.
pub(crate) fn scan_protect_file(
    path: &std::path::Path,
    password: Option<&[u8]>,
) -> RarResult<Rar4ProtectScan> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).map_err(RarError::Io)?;
    let file_len_u64 = file.metadata().map_err(RarError::Io)?.len();
    let file_len = usize::try_from(file_len_u64)
        .map_err(|_| RarError::Format("RAR4: archive size overflows host address space".into()))?;
    // The signature may sit behind an SFX stub, like the slice scanner.
    let probe_len = (8 * 1024 * 1024).min(file_len);
    let mut prefix = vec![0u8; probe_len];
    let mut filled = 0usize;
    while filled < probe_len {
        let n = file.read(&mut prefix[filled..]).map_err(RarError::Io)?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    prefix.truncate(filled);
    let sig = find_bytes(&prefix, RAR4_SIGNATURE, probe_len)
        .ok_or_else(|| RarError::Format("not a RAR4 archive (signature not found)".into()))?;
    file.seek(SeekFrom::Start((sig + RAR4_SIGNATURE.len()) as u64))
        .map_err(RarError::Io)?;
    let protect = scan_protect_stream(
        &mut file,
        sig + RAR4_SIGNATURE.len(),
        file_len,
        password,
        false,
    )?;
    Ok(Rar4ProtectScan {
        sfx_offset: sig,
        protect,
    })
}

/// The shared block walk behind both scanners: `pos` starts just past the
/// archive signature and `file_len` bounds every block. `password` is
/// required once the main header latches `-hp`.
fn scan_protect_stream<R: std::io::Read + std::io::Seek>(
    stream: &mut R,
    mut pos: usize,
    file_len: usize,
    password: Option<&[u8]>,
    tolerant: bool,
) -> RarResult<Option<Rar4Protect>> {
    const ENDARC_HEAD: u8 = 0x7b;

    let mut protect = None;
    // `-hp` flag, latched from the (plaintext) main header: every block
    // after it has an encrypted header.
    let mut encrypted = false;
    // A block found by resynchronizing past a corrupt one; consumed on the
    // next iteration (the stream is already positioned past it).
    let mut resynced: Option<crate::format::rar4::Rar4Block> = None;
    while pos + 7 <= file_len {
        let block = match resynced.take() {
            Some(block) => block,
            None => {
                // A damaged header is exactly what this scanner exists to
                // repair, so the envelope CRC is not checked; only the shared
                // bounds apply.
                match crate::format::rar4::read_block(
                    &mut *stream,
                    encrypted,
                    password,
                    crate::format::rar4::EnvelopePolicy::REPAIR,
                ) {
                    Ok(Some(block)) => block,
                    Ok(None) => break,
                    // A header too broken to parse means a corrupt block, not
                    // the record: resync past it and keep looking, the way
                    // WinRAR searches for the record.
                    Err(error) if tolerant && !matches!(error, RarError::Io(_)) => {
                        match crate::format::rar4::resync_block(&mut *stream, pos as u64 + 1)? {
                            Some(block) => {
                                pos = block.offset as usize;
                                resynced = Some(block);
                                continue;
                            }
                            None => break,
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        let start = block.offset as usize;
        let header = &block.header;
        let head_type = block.head_type;
        let flags = block.flags;
        let on_disk_header = block.on_disk_header() as usize;
        let total = block.total_size as usize;
        if start + total > file_len {
            if !tolerant {
                return Err(RarError::Format("RAR4: truncated block".into()));
            }
            // The size fields are unusable (a damaged header derailed the
            // walk): resync to the next valid block rather than giving up on
            // finding the record at all.
            match crate::format::rar4::resync_block(&mut *stream, start as u64 + 1)? {
                Some(block) => {
                    pos = block.offset as usize;
                    resynced = Some(block);
                    continue;
                }
                None => break,
            }
        }
        if head_type == 0x73 && flags & 0x0080 != 0 {
            // MAIN_HEAD + MHD_PASSWORD: the rest of the archive is `-hp`.
            encrypted = true;
        }

        // RAR 2.5-era PROTECT_HEAD (0x78): 26-byte fixed header with the
        // `Protect!` mark in the last eight bytes.
        if head_type == 0x78
            && header.len() == 26
            && flags & 0x8000 != 0
            && header.get(18..26) == Some(b"Protect!")
        {
            let rec_sectors = u16::from_le_bytes(header[12..14].try_into().unwrap());
            let total_blocks = u32::from_le_bytes(header[14..18].try_into().unwrap());
            let data_start = start + on_disk_header;
            let data_end = start + total;
            if u64::from(total_blocks) * 2 + u64::from(rec_sectors) * 512
                != (data_end - data_start) as u64
            {
                return Err(RarError::Format(
                    "RAR4: recovery data size does not match header".into(),
                ));
            }
            protect = Some(Rar4Protect {
                rec_sectors: u32::from(rec_sectors),
                total_blocks,
                mark: [0x50, 0x72, 0x6f, 0x74, 0x65, 0x63, 0x74, 0x21], // "Protect!"
                data_start,
                data_end,
                block_offset: start,
            });
            break;
        }

        // RAR 3.x/4.x NEWSUB (0x7a) named `RR`: FILE_HEAD-shaped header
        // whose 20-byte tail after the name is `Protect+` + rec_sectors
        // (u32) + total_blocks (u32) + zero.
        if head_type == 0x7a
            && flags & 0x8000 != 0
            && header.len() >= 32 + 2 + 20
            && header.get(32..34) == Some(b"RR")
        {
            let name_size = u16::from_le_bytes(header[26..28].try_into().unwrap()) as usize;
            let tail = 32 + name_size;
            if header.get(tail..tail + 8) == Some(b"Protect+") {
                // `name_size` is attacker-controlled, so "Protect+" can sit at
                // the very end of the header; the two four-byte fields that
                // follow need their own bounds check.
                let Some(rec_bytes) = header.get(tail + 8..tail + 12) else {
                    return Err(RarError::Format("RAR4: recovery header truncated".into()));
                };
                let Some(total_bytes) = header.get(tail + 12..tail + 16) else {
                    return Err(RarError::Format("RAR4: recovery header truncated".into()));
                };
                let rec_sectors = u32::from_le_bytes(rec_bytes.try_into().unwrap());
                let total_blocks = u32::from_le_bytes(total_bytes.try_into().unwrap());
                let data_start = start + on_disk_header;
                let data_end = start + total;
                if u64::from(total_blocks) * 2 + u64::from(rec_sectors) * 512
                    != (data_end - data_start) as u64
                {
                    return Err(RarError::Format(
                        "RAR4: recovery data size does not match header".into(),
                    ));
                }
                protect = Some(Rar4Protect {
                    rec_sectors,
                    total_blocks,
                    mark: [0x50, 0x72, 0x6f, 0x74, 0x65, 0x63, 0x74, 0x2b], // "Protect+"
                    data_start,
                    data_end,
                    block_offset: start,
                });
                break;
            }
        }
        if head_type == ENDARC_HEAD {
            break;
        }
        pos = start + total;
    }
    Ok(protect)
}

/// One damaged 512-byte sector of a legacy recovery record, on the sector grid
/// anchored at the archive signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyDamagedSector {
    /// Sector index.
    pub index: u32,
    /// Byte offset of the sector's first byte in the archive.
    pub offset: u64,
    /// Whether parity rebuilt it (`false` = damage the record cannot reach:
    /// two damaged sectors in one parity group, or a record written without
    /// coverage for its own final partial sector).
    pub recovered: bool,
}

/// Outcome of a legacy repair: every damaged sector and whether a rebuilt
/// archive was written. WinRAR reports one line per entry
/// (`Sector N (offsets ...) damaged - data recovered|cannot recover data`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LegacyRepair {
    /// Damaged sectors in index order, empty when the archive was intact.
    pub sectors: Vec<LegacyDamagedSector>,
    /// Whether the rebuilt archive was written to the destination.
    pub repaired: bool,
}

impl LegacyRepair {
    /// Whether the archive was already intact (nothing to fix).
    pub fn is_intact(&self) -> bool {
        self.sectors.is_empty()
    }

    /// The damaged sectors parity could not rebuild.
    pub fn unrecovered(&self) -> Vec<LegacyDamagedSector> {
        self.sectors
            .iter()
            .copied()
            .filter(|sector| !sector.recovered)
            .collect()
    }
}

/// The bytes a sector's tag covers: a complete sector as stored, or — for the
/// record's own final sector, whose tail is where the record block itself sits
/// — the protected prefix zero-padded to 512. WinRAR tags and parity-cover
/// that sector the same way (verified against 6.23 output).
fn protected_sector_image(bytes: &[u8], start: usize, prefix_end: usize) -> [u8; 512] {
    let mut image = [0u8; 512];
    let end = (start + 512).min(prefix_end).min(bytes.len());
    if end > start {
        image[..end - start].copy_from_slice(&bytes[start..end]);
    }
    image
}

/// Check every sector a record declares and rebuild what parity can, returning
/// the per-sector outcome and the rebuilt image (only when a sector was
/// rebuilt). `sfx_offset` is where the archive signature starts (0 for plain
/// archives); the 512-byte sector grid is anchored there. Damage parity cannot
/// reach is reported per sector rather than raised: WinRAR reports
/// `cannot recover data` and lets the caller decide.
pub(crate) fn repair_protect_head(
    bytes: &[u8],
    sfx_offset: usize,
    protect: &Rar4Protect,
) -> RarResult<(LegacyRepair, Option<Vec<u8>>)> {
    if protect.rec_sectors == 0 {
        return Err(RarError::Format(
            "RAR4: recovery record has no parity sectors".into(),
        ));
    }
    if &protect.mark != b"Protect!" && &protect.mark != b"Protect+" {
        return Err(RarError::Format("RAR4: recovery mark is invalid".into()));
    }
    if protect.data_end > bytes.len() {
        return Err(RarError::Format("RAR4: protected range is invalid".into()));
    }
    let total_blocks = protect.total_blocks as usize;
    let recovery = &bytes[protect.data_start..protect.data_end];
    let tag_len = total_blocks
        .checked_mul(2)
        .ok_or_else(|| RarError::Format("RAR4: recovery tag size overflows".into()))?;
    let parity_len = (protect.rec_sectors as usize)
        .checked_mul(512)
        .ok_or_else(|| RarError::Format("RAR4: recovery parity size overflows".into()))?;
    if recovery.len() != tag_len + parity_len {
        return Err(RarError::Format(
            "RAR4: recovery data size is invalid".into(),
        ));
    }
    let tags = &recovery[..tag_len];
    let parity = &recovery[tag_len..];
    // The protected prefix ends where the record block begins; the record's
    // own final sector overlaps it and is only partly on disk.
    let prefix_end = protect.block_offset;
    let rec_sectors = protect.rec_sectors as usize;
    let expected_tag =
        |index: usize| u16::from_le_bytes(tags[index * 2..index * 2 + 2].try_into().unwrap());

    // Every declared sector is checked, including the final partial one: a
    // damaged tail sector is a data error WinRAR does not report as healthy.
    let mut damaged = Vec::new();
    for index in 0..total_blocks {
        let start = sfx_offset + index * 512;
        let image = protected_sector_image(bytes, start, prefix_end);
        if sector_tag(&image) != expected_tag(index) {
            damaged.push(index);
        }
    }
    if damaged.is_empty() {
        return Ok((LegacyRepair::default(), None));
    }

    // A parity slot can rebuild a sector only when it is that slot's single
    // damaged member; the rest of the group must be intact to divide it out.
    let mut slots: Vec<Vec<usize>> = vec![Vec::new(); rec_sectors];
    for &index in &damaged {
        slots[index % rec_sectors].push(index);
    }

    let mut rebuilt = bytes.to_vec();
    let mut sectors = Vec::with_capacity(damaged.len());
    let mut repaired = false;
    for (slot, indexes) in slots.iter().enumerate() {
        for &missing in indexes {
            let offset = (sfx_offset + missing * 512) as u64;
            let mut recovered = false;
            if indexes.len() == 1 {
                let mut sector = parity[slot * 512..slot * 512 + 512].to_vec();
                for index in (slot..total_blocks).step_by(rec_sectors) {
                    if index == missing {
                        continue;
                    }
                    let start = sfx_offset + index * 512;
                    let image = protected_sector_image(bytes, start, prefix_end);
                    for (out, byte) in sector.iter_mut().zip(&image) {
                        *out ^= *byte;
                    }
                }
                // Only the prefix part of the sector exists on disk: the tail
                // of the record's own final sector is the record block.
                if sector_tag(&sector) == expected_tag(missing) {
                    let start = sfx_offset + missing * 512;
                    let end = (start + 512).min(prefix_end).min(rebuilt.len());
                    if end > start {
                        rebuilt[start..end].copy_from_slice(&sector[..end - start]);
                        recovered = true;
                        repaired = true;
                    }
                }
            }
            sectors.push(LegacyDamagedSector {
                index: missing as u32,
                offset,
                recovered,
            });
        }
    }

    Ok((
        LegacyRepair { sectors, repaired },
        repaired.then_some(rebuilt),
    ))
}

/// Repair the legacy archive at `src` into `dst` when it carries a
/// PROTECT_HEAD recovery record: `dst` is written only when parity rebuilt a
/// sector. The report names every damaged sector, so a caller can tell an
/// intact archive from damage the record cannot reach. An error means the
/// archive has no usable recovery record.
pub fn repair_legacy_archive_path(
    src: &std::path::Path,
    dst: &std::path::Path,
) -> RarResult<LegacyRepair> {
    repair_legacy_archive_path_with_password(src, dst, None)
}

/// [`repair_legacy_archive_path`] for a `-hp` header-encrypted archive: the
/// recovery record's own header is encrypted like every other block, so
/// locating it needs the archive password (the protected data — including
/// the record's tag table and parity — is never encrypted).
pub fn repair_legacy_archive_path_with_password(
    src: &std::path::Path,
    dst: &std::path::Path,
    password: Option<&str>,
) -> RarResult<LegacyRepair> {
    let bytes = std::fs::read(src).map_err(RarError::Io)?;
    let scan = scan_protect_tolerant(&bytes, password.map(str::as_bytes))?;
    let Some(protect) = scan.protect else {
        return Err(RarError::Unsupported(
            "archive has no legacy PROTECT_HEAD recovery record".into(),
        ));
    };
    let (report, rebuilt) = repair_protect_head(&bytes, scan.sfx_offset, &protect)?;
    let Some(rebuilt) = rebuilt else {
        return Ok(report);
    };
    // Keep the write atomic: stage next to the destination, then rename.
    use std::io::Write;
    let (mut staged, mut file) = crate::fs::atomic::StagedFile::create(dst)?;
    file.write_all(&rebuilt).map_err(RarError::Io)?;
    drop(file);
    staged.commit()?;
    Ok(report)
}

fn find_bytes(haystack: &[u8], needle: &[u8], limit: usize) -> Option<usize> {
    let window = &haystack[..haystack.len().min(limit)];
    window.windows(needle.len()).position(|w| w == needle)
}

// ── Write side ──────────────────────────────────────────────────────────────

/// Sector CRC16 for one 512-byte protected sector: the low 16 bits of the
/// ones-complemented CRC32 (matches both the 0x78 read path and WinRAR's
/// RAR4 NEWSUB tags, verified against 6.23 output).
pub(crate) fn sector_tag(sector: &[u8]) -> u16 {
    (!crate::crc32::crc32(sector) & 0xffff) as u16
}

/// Number of 512-byte sectors a legacy `-rrN%` record declares over a
/// `prefix` of `prefix_len` bytes: `N%` of the protected bytes **rounded up**
/// to whole sectors, with a floor of two sectors so a tiny archive still gets
/// a usable record. Rounding up is deliberate — the record must never protect
/// less than the requested percent — and it is the closest reading of WinRAR
/// 6.23, whose own count sits on or one above the ceiling (it drifts below
/// only for multi-hundred-KiB archives; see `PLAN.md`). The `-rrN` (no
/// percent) count form is used verbatim by the caller instead.
pub(crate) fn recovery_sector_count(prefix_len: usize, percent: u8) -> u32 {
    let percent = u64::from(percent);
    let bytes = (prefix_len as u64).saturating_mul(percent);
    let count = bytes.div_ceil(100 * 512);
    (count as u32).max(2)
}

/// Build a complete RAR 3.x/4.x NEWSUB (0x7a) recovery block — the shape
/// WinRAR 6.23's RAR4 writer produces for `-rr`: a FILE_HEAD-shaped header
/// named `RR` with a `Protect+` tail, followed by the tag table (one
/// sector CRC16 per protected 512-byte sector) and `rec_sectors` XOR
/// parity sectors. `prefix` is the archive bytes the record protects
/// (everything written before this block); the 512-byte sector grid is
/// anchored at `prefix[0]`, and `total_blocks` covers the whole prefix
/// (the final partial sector is still CRC-protected and parity-covered).
///
/// The caller appends this block between the last member and the
/// end-of-archive block.
pub(crate) fn build_legacy_recovery_block(prefix: &[u8], rec_sectors: u32) -> RarResult<Vec<u8>> {
    if rec_sectors == 0 {
        return Err(RarError::Format(
            "RAR4: recovery record needs at least one parity sector".into(),
        ));
    }
    let total_blocks = prefix.len().div_ceil(512) as u32;
    if total_blocks == 0 {
        return Err(RarError::Format(
            "RAR4: recovery record over an empty archive".into(),
        ));
    }
    // The parity is held in memory while it is built (like the prefix), so an
    // absurd `-rr<N>` count is rejected instead of overflowing the size math.
    let parity_len = (rec_sectors as usize)
        .checked_mul(512)
        .ok_or_else(|| RarError::Format("RAR4: recovery parity size overflows".into()))?;
    // Sector tags (little-endian u16 each): every declared sector, with a
    // partial tail zero-padded for its tag CRC, matching the reader.
    let mut tags = Vec::with_capacity(total_blocks as usize * 2);
    let mut parity = vec![0u8; parity_len];
    let mut sector = [0u8; 512];
    for block in 0..total_blocks as usize {
        let start = block * 512;
        let end = (start + 512).min(prefix.len());
        sector[..end - start].copy_from_slice(&prefix[start..end]);
        sector[end - start..].fill(0);
        tags.extend_from_slice(&sector_tag(&sector).to_le_bytes());
        // Every declared sector — the zero-padded final partial one included —
        // joins its parity group, exactly as WinRAR's records do (verified
        // against 6.23 output): without it, damage in that sector can be seen
        // but never repaired.
        let slot = block % rec_sectors as usize;
        for (out, byte) in parity[slot * 512..slot * 512 + 512].iter_mut().zip(&sector) {
            *out ^= *byte;
        }
    }

    let data_len = tags.len() + parity.len();
    let mut header = Vec::with_capacity(32 + 2 + 20);
    // CRC placeholder + type + flags (LONG_BLOCK) + head_size + add_size.
    header.extend_from_slice(&[0u8; 2]);
    header.push(0x7a); // NEWSUB_HEAD
    header.extend_from_slice(&(0xC000u16).to_le_bytes()); // LONG_BLOCK
    header.extend_from_slice(&(54u16).to_le_bytes()); // head_size
    header.extend_from_slice(&(data_len as u32).to_le_bytes()); // packed/add
    header.extend_from_slice(&(data_len as u32).to_le_bytes()); // unpacked
    header.push(2); // host_os: Windows
    header.extend_from_slice(&0u32.to_le_bytes()); // file_crc (unused)
    header.extend_from_slice(&0u32.to_le_bytes()); // file_time
    header.push(29); // unp_ver
    header.push(0x30); // method: store
    header.extend_from_slice(&2u16.to_le_bytes()); // name_size
    header.extend_from_slice(&0u32.to_le_bytes()); // file_attr
    header.extend_from_slice(b"RR");
    header.extend_from_slice(b"Protect+");
    header.extend_from_slice(&rec_sectors.to_le_bytes());
    header.extend_from_slice(&total_blocks.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    debug_assert_eq!(header.len(), 54);

    // Header CRC16 over bytes[2..] (the RAR4 convention).
    let crc = (crate::crc32::crc32(&header[2..]) & 0xffff) as u16;
    header[0..2].copy_from_slice(&crc.to_le_bytes());

    let mut out = header;
    out.extend_from_slice(&tags);
    out.extend_from_slice(&parity);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NEWSUB `RR` block whose `name_size` pushes `Protect+` to the end of
    /// its own header must be rejected, not index past it.
    #[test]
    fn crafted_rr_name_size_does_not_panic() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(RAR4_SIGNATURE);
        let head_size = 54usize;
        let mut header = vec![0u8; head_size];
        header[2] = 0x7a; // NEWSUB_HEAD
        header[3..5].copy_from_slice(&0x8000u16.to_le_bytes()); // LONG_BLOCK
        header[5..7].copy_from_slice(&(head_size as u16).to_le_bytes());
        header[7..11].copy_from_slice(&0u32.to_le_bytes()); // no data area
        header[26..28].copy_from_slice(&14u16.to_le_bytes()); // name_size -> tail 46
        header[32..34].copy_from_slice(b"RR");
        header[46..54].copy_from_slice(b"Protect+"); // tail..tail+8
        bytes.extend_from_slice(&header);

        assert!(scan_protect(&bytes).is_err());
    }

    /// A block whose `head_size` is 7 while `LONG_BLOCK` demands a 4-byte
    /// ADD_SIZE must be rejected by the shared envelope reader instead of
    /// reading the size field past the header.
    #[test]
    fn short_long_block_is_rejected_without_panicking() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(RAR4_SIGNATURE);
        let mut header = vec![0u8; 7];
        header[2] = 0x74; // FILE_HEAD
        header[3..5].copy_from_slice(&0x8000u16.to_le_bytes()); // LONG_BLOCK
        header[5..7].copy_from_slice(&7u16.to_le_bytes());
        bytes.extend_from_slice(&header);

        let err = match scan_protect(&bytes) {
            Err(err) => err,
            Ok(_) => panic!("expected the malformed block to be rejected"),
        };
        assert!(
            matches!(err, RarError::Format(_)),
            "expected a format error, got {err}"
        );
    }

    #[test]
    fn protect_scan_finds_and_repairs_damage() {
        // A genuine RAR 2.5 archive with a 5% PROTECT_HEAD recovery record.
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/rar40/repair/rar250_protect_head_rr5.rar"
        );
        let original = std::fs::read(fixture).expect("read fixture");
        let scan = scan_protect(&original).expect("scan");
        let protect = scan.protect.expect("protect record present");
        assert_eq!(&protect.mark, b"Protect!");

        // Damage 64 bytes inside sector 1 (same spot the reference test uses).
        let mut damaged = original.clone();
        let damage_offset = 512 + 16;
        damaged[damage_offset..damage_offset + 64].fill(0xa5);

        let (report, rebuilt) = repair_protect_head(&damaged, 0, &protect).expect("repair");
        assert!(
            report.repaired,
            "the damaged sector must be rebuilt: {report:?}"
        );
        assert_eq!(
            rebuilt.expect("damage found"),
            original,
            "repair restores the original bytes"
        );

        // An intact archive reports nothing to do.
        let (report, rebuilt) = repair_protect_head(&original, 0, &protect).expect("intact scan");
        assert!(report.is_intact(), "{report:?}");
        assert!(rebuilt.is_none());
    }

    /// Build a NEWSUB (0x7a) recovery block over a synthetic prefix and
    /// round-trip it through the same scan/repair path a legacy archive
    /// would: scan recognises the `RR` record, an intact copy reports no
    /// damage, and damage inside a protected sector is rebuilt exactly.
    #[test]
    fn newsub_rr_block_roundtrips_through_scan_and_repair() {
        use crate::format::rar4::RAR4_METHOD_STORE;
        use crate::format::rar4::write::{FileHeaderParams, build_file_header, build_main_header};

        // A minimal but structurally valid archive: signature + main header
        // + one stored member, whose payload fills most of the protected
        // range (the scan walks real RAR4 blocks, so the prefix cannot be
        // arbitrary bytes).
        let mut payload = Vec::with_capacity(200_000);
        let mut seed = 0x1234_5678u32;
        while payload.len() < 200_000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            payload.push((seed >> 24) as u8);
        }
        let fh = build_file_header(&FileHeaderParams {
            flags: 0,
            packed_size: payload.len() as u32,
            unpacked_size: payload.len() as u32,
            host_os: 0,
            file_crc: 0,
            file_time: 0,
            unp_ver: 20,
            method: RAR4_METHOD_STORE,
            name: b"d.bin",
            attr: 0x20,
            window_bits: 0,
            salt: None,
            ext_time: None,
        })
        .expect("file head");
        let mut prefix = b"Rar!\x1a\x07\x00".to_vec();
        prefix.extend_from_slice(&build_main_header(0));
        prefix.extend_from_slice(&fh);
        prefix.extend_from_slice(&payload);

        // 10% recovery, rounded up: the record never protects less than the
        // requested percent.
        let rec = recovery_sector_count(prefix.len(), 10);
        assert_eq!(
            rec,
            (prefix.len() as u64 * 10).div_ceil(51_200) as u32,
            "rec formula rounds up"
        );
        let block = build_legacy_recovery_block(&prefix, rec).expect("build");
        let total_blocks = prefix.len().div_ceil(512) as u32;
        assert_eq!(
            block.len(),
            54 + total_blocks as usize * 2 + rec as usize * 512
        );

        // Append the RR block + ENDARC.
        let mut archive = prefix.clone();
        let rr_offset = archive.len();
        archive.extend_from_slice(&block);
        archive.extend_from_slice(&[0, 0, 0x7b, 0, 0, 7, 0]); // ENDARC

        let scan = scan_protect(&archive).expect("scan");
        let protect = scan.protect.expect("NEWSUB RR record present");
        assert_eq!(&protect.mark, b"Protect+");
        assert_eq!(protect.rec_sectors, rec);
        assert_eq!(protect.total_blocks, total_blocks);
        assert_eq!(protect.block_offset, rr_offset);
        assert_eq!(protect.data_start, rr_offset + 54);

        // Intact: nothing to repair.
        let (report, rebuilt) = repair_protect_head(&archive, 0, &protect).expect("intact scan");
        assert!(report.is_intact(), "{report:?}");
        assert!(rebuilt.is_none());

        // Damage a stretch of a protected data sector (inside `payload`, far
        // from the recovery block itself) and rebuild it byte-identically.
        let mut damaged = archive.clone();
        let damage_at = 40_000;
        damaged[damage_at..damage_at + 128].fill(0x5a);
        let repaired = repair_protect_head(&damaged, 0, &protect)
            .expect("repair")
            .1
            .expect("damage found");
        assert_eq!(repaired, archive, "NEWSUB RR repair restores the prefix");

        // The record's own final sector: its tail is where the record block
        // sits, so only [sector_start, record_start) is on disk. WinRAR both
        // tags and parity-covers it; damage there must be found (never
        // reported as healthy) and rebuilt byte-identically.
        let last = total_blocks as usize - 1;
        let mut damaged = archive.clone();
        damaged[(last * 512)..rr_offset].fill(0x3c);
        let (report, rebuilt) = repair_protect_head(&damaged, 0, &protect).expect("tail scan");
        assert_eq!(
            report.sectors,
            vec![LegacyDamagedSector {
                index: last as u32,
                offset: last as u64 * 512,
                recovered: true,
            }],
            "{report:?}"
        );
        assert_eq!(
            rebuilt.expect("tail damage rebuilt"),
            archive,
            "the record's own final sector is rebuilt from parity"
        );

        // Two damaged sectors in the SAME parity group exceed that group's
        // capacity (one parity sector can rebuild one member), even though
        // the global count is far below the parity sector total. Both are
        // reported as unrecovered, the way WinRAR says `cannot recover data`,
        // rather than aborting the whole repair.
        let mut hopeless = archive.clone();
        hopeless[0..32].fill(0x7e);
        hopeless[(rec as usize * 512)..(rec as usize * 512 + 32)].fill(0x7e);
        let (report, rebuilt) = repair_protect_head(&hopeless, 0, &protect).expect("scan");
        assert!(!report.repaired, "{report:?}");
        assert!(rebuilt.is_none());
        let unrecovered = report.unrecovered();
        assert_eq!(
            unrecovered.iter().map(|s| s.index).collect::<Vec<_>>(),
            vec![0, rec],
            "{report:?}"
        );
        assert_eq!(unrecovered[1].offset, u64::from(rec) * 512);
    }

    /// The streaming file scanner must agree with the slice scanner it was
    /// split from (signature offset and every record position).
    #[test]
    fn scan_protect_file_matches_the_slice_scan() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/rar40/repair/rar250_protect_head_rr5.rar"
        );
        let original = std::fs::read(fixture).expect("read fixture");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("protect.rar");
        std::fs::write(&path, &original).unwrap();

        let slice = scan_protect(&original).expect("slice scan");
        let file = scan_protect_file(&path, None).expect("file scan");
        assert_eq!(file.sfx_offset, slice.sfx_offset);
        let (slice, file) = (
            slice.protect.expect("slice record"),
            file.protect.expect("file record"),
        );
        assert_eq!(file.mark, slice.mark);
        assert_eq!(file.rec_sectors, slice.rec_sectors);
        assert_eq!(file.total_blocks, slice.total_blocks);
        assert_eq!(file.block_offset, slice.block_offset);
        assert_eq!(file.data_start, slice.data_start);
        assert_eq!(file.data_end, slice.data_end);
    }

    /// The percent form rounds **up**: a record must never protect less than
    /// the requested percent (the caller uses the `-rr<N>` count verbatim).
    #[test]
    fn percent_recovery_count_rounds_up() {
        // 50_000 bytes at 10% is 9.77 sectors: 10, never 9.
        assert_eq!(recovery_sector_count(50_000, 10), 10);
        // A whole multiple stays exact.
        assert_eq!(recovery_sector_count(100 * 512, 10), 10);
        // One byte past a multiple still rounds up.
        assert_eq!(recovery_sector_count(100 * 512 + 1, 10), 11);
        // A tiny archive keeps the two-sector floor.
        assert_eq!(recovery_sector_count(1_000, 1), 2);
    }
}
