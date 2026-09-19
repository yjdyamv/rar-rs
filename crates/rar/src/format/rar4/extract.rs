//! RAR 1.5–4.x read path: volume scanning, member decoding and the legacy
//! solid-chain driver.
//!
//! Everything a legacy archive needs to be listed or extracted lives here;
//! the family-neutral orchestration is in
//! [`crate::format::shared::extract`].

use std::fs::File;
use std::io::{Read, Write};

use super::{LegacyDecoder, MemberDecodeOptions, Rar4VolumeScan};
use crate::detect::RAR4_SIGNATURE;
use crate::engine::Engine;
use crate::error::{RarError, RarResult};
use crate::format::shared::extract::{MAX_CATALOG_ENTRIES, check_entry_cap};
use crate::format::shared::stream_mut;
use crate::model::FileHeader;
use crate::version::LegacyCodec;

/// Whether the member belongs to the RAR29 (RAR 3.x/4.x) codec generation,
/// whose solid runs are flagged per member with FHD_SOLID.
fn is_rar29_codec(hdr: &FileHeader) -> bool {
    matches!(
        LegacyCodec::from_unp_ver(hdr.unp_ver),
        Some(LegacyCodec::Rar29)
    )
}

/// Scan a RAR 1.5–4.x volume set (legacy fixed-width block headers) into
/// the entry catalog. Volume 0 is the already-open primary stream,
/// positioned right after the signature (SFX-aware); later volumes open
/// fresh and each starts with its own 7-byte signature.
pub(crate) fn open_read_rar4(cx: &mut dyn Engine) -> RarResult<()> {
    cx.entries_mut().clear();
    let mut scan = Rar4VolumeScan::default();
    let mut out = Vec::new();

    {
        let p = cx.parts();
        scan.scan_volume(stream_mut(p.stream)?, 0, p.password, &mut out)?;
    }
    check_entry_cap(out.len(), MAX_CATALOG_ENTRIES)?;
    let volume_paths = cx.volume_paths().to_vec();
    for (vol_idx, vol_path) in volume_paths.iter().enumerate().skip(1) {
        cx.check_cancel()?;
        let mut stream = File::open(vol_path)?;
        let mut sig = [0u8; 7];
        stream.read_exact(&mut sig)?;
        if &sig != RAR4_SIGNATURE {
            return Err(RarError::Format(format!(
                "volume {} has a bad RAR4 signature",
                vol_path.display()
            )));
        }
        {
            let p = cx.parts();
            scan.scan_volume(&mut stream, vol_idx, p.password, &mut out)?;
        }
        check_entry_cap(out.len(), MAX_CATALOG_ENTRIES)?;
    }
    let archive_solid = scan.archive_solid;
    let new_numbering = scan.new_numbering;
    scan.finish()?;
    cx.set_archive_solid(archive_solid);
    cx.read_ctx_mut().legacy.new_numbering = new_numbering;
    *cx.entries_mut() = out;
    Ok(())
}

/// Whether `idx` sits in a legacy solid run. RAR3+ members (the RAR29
/// codec) chain on the per-file FHD_SOLID bit (a head member is solid by
/// being directly followed by a flagged member). Pre-RAR3 codecs never
/// write that bit: when the main header carried MHD_SOLID, every
/// compressed member of such a codec is part of one shared-window run.
fn is_rar4_solid_member(cx: &dyn Engine, idx: usize) -> bool {
    let hdr = &cx.entries()[idx].header;
    if !is_rar29_codec(hdr) {
        return cx.archive_solid() && !cx.entries()[idx].is_dir();
    }
    if hdr.comp_solid {
        return true;
    }
    idx + 1 < cx.entries().len() && cx.entries()[idx + 1].header.comp_solid
}

/// Find the start index of the legacy solid chain containing `idx` (the
/// first member at or before it that is not solid, followed by solid
/// members; directory entries do not break the run).
fn rar4_find_chain_start(cx: &dyn Engine, target_idx: usize) -> usize {
    // Pre-RAR3 codecs under MHD_SOLID do not write FHD_SOLID. STORE
    // members leave the shared window untouched, so the run reaches back
    // across them to the first compressed member.
    if !is_rar29_codec(&cx.entries()[target_idx].header) {
        let mut chain_start = target_idx;
        for i in (0..target_idx).rev() {
            if !cx.entries()[i].is_dir() && !super::is_stored(cx.entries()[i].header.comp_method) {
                chain_start = i;
            }
        }
        return chain_start;
    }

    // For RAR3+, FHD_SOLID belongs to the current member and means it
    // continues the previous non-directory member. Stop as soon as the
    // current chain head is unflagged; inspecting the previous member's
    // flag would incorrectly cross into an independent earlier run.
    let mut chain_start = target_idx;
    while cx.entries()[chain_start].header.comp_solid {
        let Some(previous) = (0..chain_start).rev().find(|&i| !cx.entries()[i].is_dir()) else {
            break;
        };
        if !is_rar29_codec(&cx.entries()[previous].header) {
            break;
        }
        chain_start = previous;
    }
    chain_start
}

/// Reset the legacy solid decoder to immediately before the current run.
fn reset_rar4_solid_decoder(cx: &mut dyn Engine, chain_start: usize) {
    let ctx = cx.read_ctx_mut();
    ctx.legacy.decoder = None;
    ctx.legacy.decoded_through = chain_start as isize - 1;
}

/// Decode the legacy solid chain up through `target_idx` with one shared
/// decoder, returning the target member's bytes. Intermediate members are
/// decoded only to advance the shared window. STORE members in a RAR2.x
/// or RAR1.5 chain do not advance the window but do not break the chain
/// either (the decoder is simply not called).
pub(crate) fn rar4_decode_solid_through(
    cx: &mut dyn Engine,
    target_idx: usize,
) -> RarResult<Vec<u8>> {
    let chain_start = rar4_find_chain_start(cx, target_idx);

    let start_from = {
        let ctx = cx.read_ctx_mut();
        if ctx.legacy.decoder.is_some()
            && ctx.legacy.decoded_through >= chain_start as isize
            && ctx.legacy.decoded_through < target_idx as isize
        {
            // Continue from where we left off.
        } else {
            // Backwards request or a fresh chain: restart from this run's
            // head, not from unrelated members in an earlier solid run.
            ctx.legacy.decoder = None;
            ctx.legacy.decoded_through = chain_start as isize - 1;
        }
        if ctx.legacy.decoder.is_none() {
            // Bootstrap with a Rar29 decoder; it will be replaced on the
            // first compressed member that reveals the actual codec.
            ctx.legacy.decoder = Some(LegacyDecoder::new_for(LegacyCodec::Rar29));
        }
        (ctx.legacy.decoded_through + 1) as usize
    };

    let mut target = Vec::new();
    for i in start_from..=target_idx {
        crate::format::shared::extract::members::validate_entry_limits(cx, i)?;
        let entry = cx.entries()[i].clone();
        if entry.is_dir() {
            continue;
        }
        let hdr = entry.header;
        let chunks = entry.chunks;

        // Determine the decoder type from the member's codec. A STORE
        // member keeps the existing decoder unchanged (for RAR2.x the
        // window is not advanced; for RAR1.5 likewise).
        let is_compressed = !super::is_stored(hdr.comp_method);
        if is_compressed && let Some(codec) = LegacyCodec::from_unp_ver(hdr.unp_ver) {
            // Ensure the decoder matches this member's codec version.
            let needs_rebuild = {
                let dec = cx.read_ctx_mut().legacy.decoder.as_ref();
                !dec.is_some_and(|dec| dec.codec() == codec)
            };
            if needs_rebuild {
                cx.read_ctx_mut().legacy.decoder = Some(LegacyDecoder::new_for(codec));
            }
        }

        let mut decoder = cx.read_ctx_mut().legacy.decoder.take();
        let max_packed_bytes = crate::format::rar5::extract::decode::max_packed_bytes(cx);
        let data = {
            let p = cx.parts();
            super::decode_member_bytes(
                stream_mut(p.stream)?,
                p.volume_paths,
                &chunks,
                &hdr,
                MemberDecodeOptions {
                    password: p.password,
                    decoder: decoder.as_mut(),
                    max_alloc_packed_bytes: max_packed_bytes,
                    max_stream_packed_bytes: max_packed_bytes,
                },
            )
        };
        let data = match data {
            Ok(data) => data,
            Err(err) => {
                reset_rar4_solid_decoder(cx, chain_start);
                return Err(err);
            }
        };
        cx.read_ctx_mut().legacy.decoder = decoder;
        cx.read_ctx_mut().legacy.decoded_through = i as isize;
        if i == target_idx {
            target = data;
        }
    }
    Ok(target)
}

/// Decode a single RAR4 member in memory, verifying its CRC32. Solid
/// chain members decode through their chain prefix (shared window).
pub(crate) fn decode_rar4_at(cx: &mut dyn Engine, idx: usize) -> RarResult<Vec<u8>> {
    crate::format::shared::extract::members::validate_entry_limits(cx, idx)?;
    let hdr = cx.entries()[idx].header.clone();
    if is_rar4_solid_member(cx, idx) {
        let chain_start = rar4_find_chain_start(cx, idx);
        let result = rar4_decode_solid_through(cx, idx)
            .and_then(|out| rar4_verify_crc(&hdr, &out).map(|()| out));
        if result.is_err() {
            reset_rar4_solid_decoder(cx, chain_start);
        }
        return result;
    }
    let out = rar4_decode_member(cx, idx)?;
    rar4_verify_crc(&hdr, &out)?;
    Ok(out)
}

/// Maximum packed bytes accepted by a truly streaming STORE path. With no
/// unpacked limit there is no allocation-driven packed-size ceiling.
fn max_stream_packed_bytes(cx: &dyn Engine) -> u64 {
    cx.read_ctx()
        .extract_options
        .max_unpacked_bytes
        .map(|u| u.saturating_add(1 << 20))
        .unwrap_or(u64::MAX)
}

/// Decode a single RAR4 member, streaming output to `writer`, verifying
/// its CRC32 over the written bytes. Non-chain members stream through
/// the bounded-memory path (STORE chunks copied straight out; compressed
/// members decode incrementally); solid-chain members keep the shared
/// window semantics and decode in one pass.
pub(crate) fn decode_rar4_to(
    cx: &mut dyn Engine,
    idx: usize,
    writer: &mut dyn Write,
) -> RarResult<u64> {
    crate::format::shared::extract::members::validate_entry_limits(cx, idx)?;
    let hdr = cx.entries()[idx].header.clone();
    if is_rar4_solid_member(cx, idx) {
        let chain_start = rar4_find_chain_start(cx, idx);
        let result = rar4_decode_solid_through(cx, idx).and_then(|out| {
            rar4_verify_crc(&hdr, &out)?;
            writer.write_all(&out).map_err(RarError::Io)?;
            Ok(out.len() as u64)
        });
        if result.is_err() {
            reset_rar4_solid_decoder(cx, chain_start);
        }
        return result;
    }
    let entry = cx.entries()[idx].clone();
    let max_alloc_packed_bytes = crate::format::rar5::extract::decode::max_packed_bytes(cx);
    let max_stream_packed_bytes = max_stream_packed_bytes(cx);
    let (written, crc, rar13_checksum) = {
        let p = cx.parts();
        super::decode_member_bytes_to(
            stream_mut(p.stream)?,
            p.volume_paths,
            &entry.chunks,
            &entry.header,
            MemberDecodeOptions {
                password: p.password,
                decoder: None,
                max_alloc_packed_bytes,
                max_stream_packed_bytes,
            },
            writer,
        )?
    };
    // The streamed checksum is authoritative; compare with the header.
    let actual = if hdr.uses_rar13_checksum() {
        u32::from(rar13_checksum)
    } else {
        crc
    };
    verify_member_crc(&hdr, actual)?;
    Ok(written)
}

/// Decode RAR4 member `idx`, routing solid-chain members through the
/// persistent legacy decoder so their look-behind window covers the
/// chain prefix.
fn rar4_decode_member(cx: &mut dyn Engine, idx: usize) -> RarResult<Vec<u8>> {
    if is_rar4_solid_member(cx, idx) {
        return rar4_decode_solid_through(cx, idx);
    }
    let entry = cx.entries()[idx].clone();
    let max_packed_bytes = crate::format::rar5::extract::decode::max_packed_bytes(cx);
    let p = cx.parts();
    super::decode_member_bytes(
        stream_mut(p.stream)?,
        p.volume_paths,
        &entry.chunks,
        &entry.header,
        MemberDecodeOptions {
            password: p.password,
            decoder: None,
            max_alloc_packed_bytes: max_packed_bytes,
            max_stream_packed_bytes: max_packed_bytes,
        },
    )
}

/// Verify a member's CRC32 (or RAR 1.3/1.4 checksum) against its header.
fn rar4_verify_crc(hdr: &FileHeader, data: &[u8]) -> RarResult<()> {
    let actual = if hdr.uses_rar13_checksum() {
        u32::from(crate::format::rar13::file_checksum(data))
    } else {
        super::member_crc(data)
    };
    verify_member_crc(hdr, actual)
}

/// Compare a member's stored checksum with the computed one.
fn verify_member_crc(hdr: &FileHeader, actual: u32) -> RarResult<()> {
    if let Some(expected) = hdr.crc32_val
        && actual != expected
    {
        return Err(RarError::Crc {
            expected,
            actual,
            context: format!("{}: checksum mismatch", hdr.name),
        });
    }
    Ok(())
}
