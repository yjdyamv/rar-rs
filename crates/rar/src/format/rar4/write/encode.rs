//! RAR4 member compression and encryption: the per-generation codec dispatch
//! (RAR 3.x/4.x LZSS + auto VM filters + PPMd, RAR 2.x, RAR 1.5), the level
//! ladders ported from the reference writers, the solid-chain encoder reuse
//! and the `-p` cipher dispatch.

use crate::codec::legacy::rar29_encoder::Rar29FilterKind;
use crate::engine::Engine;
use crate::error::{RarError, RarResult};
use crate::version::LegacyCodec;
/// Encode one RAR4 member payload, dispatching on the archive's legacy
/// member version (`rar4_unp_ver`). RAR29 (the default) keeps the full
/// engine set — LZSS with auto VM filters and a PPMd trial on m4/m5.
/// RAR 1.5/2.x members (`v15`/`v20`) mirror the `rars` legacy writers'
/// level ladders, and STORE wins whenever the configured codec cannot
/// shrink the data. In solid archives the legacy encoder instance is
/// reused across the members of a run so its adaptive tables (and the
/// RAR 2.x window) carry over — historical WinRAR produced solid
/// RAR 1.5/2.x archives this way too.
///
/// The reader skips STORE members (their bytes never reach the
/// decoder), so the persistent encoder must not advance for a member
/// that falls back to STORE. The trial encode runs on a clone; only a
/// member that actually packs (and is therefore decoded) commits the
/// advanced state.
pub(super) fn encode_rar4_member(
    cx: &mut dyn Engine,
    data: &[u8],
    level: u8,
) -> RarResult<(Vec<u8>, u8)> {
    let Some(codec) = LegacyCodec::from_unp_ver(cx.write_ctx().solid.rar4_unp_ver) else {
        return Err(RarError::Unsupported(format!(
            "RAR4 write dispatch: unp_ver {} has no encoder",
            cx.write_ctx().solid.rar4_unp_ver
        )));
    };
    if codec == LegacyCodec::Rar29 {
        return encode_rar29_member(cx, data, level);
    }
    if !(1..=5).contains(&level) {
        return Ok((data.to_vec(), crate::format::rar4::RAR4_METHOD_STORE));
    }
    let method = crate::format::rar4::RAR4_METHOD_STORE + level;
    let packed = if cx.write_ctx().solid.mode {
        use crate::engine::LegacySolidEncoder;
        let mut trial = match cx.write_ctx().solid.legacy_encoder.as_ref() {
            Some(LegacySolidEncoder::Rar15(encoder)) => {
                LegacySolidEncoder::Rar15(Box::new(encoder.clone_for_trial()))
            }
            Some(LegacySolidEncoder::Rar20(encoder)) => LegacySolidEncoder::Rar20(encoder.clone()),
            None => build_legacy_solid_encoder(codec, level)?,
        };
        let packed = match &mut trial {
            LegacySolidEncoder::Rar15(encoder) => encoder.encode_member(data)?,
            LegacySolidEncoder::Rar20(encoder) => encoder.encode_member(data)?,
        };
        if packed.len() < data.len() {
            cx.write_ctx_mut().solid.legacy_encoder = Some(trial);
        }
        packed
    } else {
        encode_legacy_codec_member(data, level, codec)?
    };
    if packed.len() < data.len() {
        Ok((packed, method))
    } else {
        Ok((data.to_vec(), crate::format::rar4::RAR4_METHOD_STORE))
    }
}

/// Member-level encryption (WinRAR `-p`), dispatched on the member's
/// cipher generation. RAR29 members get the RAR30 AES-128-CBC cipher
/// with a fresh per-member 8-byte salt (flagged `FHD_SALT`); RAR 2.x
/// members use the RAR20 block cipher (16-byte padded, no salt); RAR
/// 1.5 members the RAR15 stream XOR (no padding, no salt). Returns the
/// salt for RAR29 (written into the file header) and `None` for the
/// saltless old codecs.
pub(super) fn rar4_member_encrypt(
    cx: &mut dyn Engine,
    packed: &mut Vec<u8>,
) -> RarResult<Option<[u8; 8]>> {
    let Some(pw) = cx.password().filter(|pw| !pw.is_empty()) else {
        return Ok(None);
    };
    match LegacyCodec::from_unp_ver(cx.write_ctx().solid.rar4_unp_ver) {
        Some(LegacyCodec::Rar15) => {
            crate::crypto::Rar15Cipher::new(pw.as_bytes()).crypt_in_place(packed);
            Ok(None)
        }
        Some(LegacyCodec::Rar20) => {
            let pad = (16 - packed.len() % 16) % 16;
            packed.resize(packed.len() + pad, 0);
            crate::crypto::Rar20Cipher::new(pw.as_bytes())
                .encrypt_in_place(packed)
                .map_err(|e| RarError::Format(format!("RAR4 member (RAR20) encrypt: {e}")))?;
            Ok(None)
        }
        Some(LegacyCodec::Rar29) => {
            let mut salt = [0u8; 8];
            rand::fill(&mut salt);
            let mut cipher = crate::crypto::Rar30Cipher::new(pw.as_bytes(), Some(salt))
                .map_err(|e| RarError::Format(format!("RAR4 member key setup: {e:?}")))?;
            let pad = (16 - packed.len() % 16) % 16;
            packed.resize(packed.len() + pad, 0);
            cipher
                .encrypt_in_place(packed)
                .map_err(|e| RarError::Format(format!("RAR4 member encrypt: {e:?}")))?;
            Ok(Some(salt))
        }
        None => Err(RarError::Unsupported(format!(
            "RAR4 write encryption: unp_ver {} has no cipher",
            cx.write_ctx().solid.rar4_unp_ver
        ))),
    }
}

fn encode_rar29_member(cx: &mut dyn Engine, data: &[u8], level: u8) -> RarResult<(Vec<u8>, u8)> {
    // Compress with the RAR29 LZSS encoder (m1–m5). If compressing does
    // not shrink the data, fall back to STORE. Non-solid members also try
    // the automatic VM filters and, on m4/m5, a PPMd pass; the smallest
    // candidate wins (see `best_rar29_member`).
    if !(1..=5).contains(&level) {
        return Ok((data.to_vec(), crate::format::rar4::RAR4_METHOD_STORE));
    }
    if !cx.write_ctx().solid.mode {
        let filters = cx.write_ctx().compression.filters;
        return Ok(match best_rar29_member(data, level, filters)? {
            Some(best) => best,
            None => (data.to_vec(), crate::format::rar4::RAR4_METHOD_STORE),
        });
    }
    // Solid: reuse the persistent encoder so its sliding window, Huffman
    // table and PPMd model state carry across the members of the run
    // (this is what makes a real -ms archive compress better than
    // independent members). The plain member and every `-mc` filter
    // candidate are measured against the chain as it stands, the smallest
    // LZ result then competes with the chain-continuing PPMd trial, and
    // only the winner advances the chain (see
    // `Unpack29Encoder::encode_solid_member_with_filter_candidates`). A
    // filtered member stays an ordinary chain link: the reader's window
    // holds the coded bytes, so the next member may keep matching them.
    use crate::codec::legacy::rar29_encoder::{Unpack29Encoder, options_for_level};
    let candidates = rar29_filter_candidates(data, cx.write_ctx().compression.filters);
    let encoder = cx
        .write_ctx_mut()
        .solid
        .rar4_encoder
        .get_or_insert_with(|| Unpack29Encoder::with_options(options_for_level(level)));
    let lz = if data.is_empty() {
        encoder.encode_member(data)?
    } else {
        encoder.encode_solid_member_with_filter_candidates(data, &candidates)?
    };
    if lz.len() < data.len() {
        Ok((lz, crate::format::rar4::RAR4_METHOD_STORE + level))
    } else {
        Ok((data.to_vec(), crate::format::rar4::RAR4_METHOD_STORE))
    }
}

/// RAR 2.x ladder: candidate counts 16/64/256/512/1024 + lazy matching +
/// lookahead 2 + optimal parse on m4/m5 + audio encoding on m2–m5
/// (mirrors rars' `Unpack20Encoder` write ladder exactly).
pub(super) fn legacy_rar20_options(
    level: u8,
) -> crate::codec::legacy::rar20_encoder::EncodeOptions {
    use crate::codec::legacy::rar20_encoder::EncodeOptions;
    let candidates = match level {
        1 => 16,
        2 => 64,
        3 => 256,
        4 => 512,
        _ => 1024,
    };
    EncodeOptions::new(candidates)
        .with_lazy_matching(true)
        .with_lazy_lookahead(2)
        .with_optimal_parse(level >= 4)
        .with_try_audio(level > 1)
}

/// RAR 1.5 ladder: old-distance tokens off on m1/m2, st-mode literal runs
/// off on m1/m2, max long-match distance 4/8/16/24 KiB / unbounded, lazy
/// matching off at every level (mirrors rars' `Unpack15Encoder` ladder).
pub(super) fn legacy_rar15_options(
    level: u8,
) -> crate::codec::legacy::rar15_encoder::EncodeOptions {
    use crate::codec::legacy::rar15_encoder::EncodeOptions;
    match level {
        1 => EncodeOptions::new()
            .with_old_distance_tokens(false)
            .with_lazy_matching(false)
            .with_stmode_literal_runs(false)
            .with_max_long_match_distance(4 * 1024),
        2 => EncodeOptions::new()
            .with_old_distance_tokens(false)
            .with_lazy_matching(false)
            .with_stmode_literal_runs(false)
            .with_max_long_match_distance(8 * 1024),
        3 => EncodeOptions::new()
            .with_lazy_matching(false)
            .with_max_long_match_distance(16 * 1024),
        4 => EncodeOptions::new()
            .with_lazy_matching(false)
            .with_max_long_match_distance(24 * 1024),
        _ => EncodeOptions::new().with_lazy_matching(false),
    }
}

/// Encode a RAR4 member with the RAR 1.5/2.x codec `codec` using a fresh
/// instance (non-solid semantics). Shared by the sequential member writer
/// (`encode_rar4_member`) and the parallel batch preparation
/// (`prepare_rar4_file_member`). STORE fallback stays with the callers'
/// size comparison.
pub(super) fn encode_legacy_codec_member(
    data: &[u8],
    level: u8,
    codec: LegacyCodec,
) -> RarResult<Vec<u8>> {
    match codec {
        LegacyCodec::Rar20 => Ok(
            crate::codec::legacy::rar20_encoder::unpack20_encode_auto_with_options(
                data,
                legacy_rar20_options(level),
            )?,
        ),
        LegacyCodec::Rar15 => Ok(
            crate::codec::legacy::rar15_encoder::Unpack15Encoder::with_options(
                legacy_rar15_options(level),
            )
            .encode_member(data)?,
        ),
        LegacyCodec::Rar29 => {
            unreachable!("encode_rar4_member dispatches the RAR29 encoder before this helper")
        }
    }
}

/// Build the persistent encoder for a solid RAR 1.5/2.x run; the same
/// instance is reused for every member of the run (rars' `solid_encoder`
/// reuse). The run's level defaults to the first member's ladder options,
/// matching the RAR29 solid encoder's get-or-create semantics.
fn build_legacy_solid_encoder(
    codec: LegacyCodec,
    level: u8,
) -> RarResult<crate::engine::LegacySolidEncoder> {
    use crate::engine::LegacySolidEncoder;
    match codec {
        LegacyCodec::Rar20 => Ok(LegacySolidEncoder::Rar20(
            crate::codec::legacy::rar20_encoder::Unpack20Encoder::with_options(
                legacy_rar20_options(level),
            ),
        )),
        LegacyCodec::Rar15 => Ok(LegacySolidEncoder::Rar15(Box::new(
            crate::codec::legacy::rar15_encoder::Unpack15Encoder::with_options(
                legacy_rar15_options(level),
            ),
        ))),
        LegacyCodec::Rar29 => {
            unreachable!("RAR29 solid runs use the write context's persistent encoder")
        }
    }
}

/// Non-solid RAR29 candidate selection: the smallest of LZ, the automatic VM
/// filters (E8/E8E9, delta, audio) and the PPMd trial on m4/m5. Returns
/// `None` when nothing shrinks the input, so an owning caller can fall back
/// to STORE without copying its buffer.
///
/// The solid-run path is separate: it reuses the persistent encoder,
/// searches the same filter list against the carried chain, and then pits
/// the best LZ result against the chain-continuing PPMd trial
/// (`Unpack29Encoder::encode_solid_member_with_filter_candidates`).
/// The standard-filter candidates the `-mc` policy allows for one member.
///
/// Shared by the non-solid and solid paths so both search the same list in
/// the same order; a tie keeps the earlier candidate because every comparison
/// is a strict `<`. Auto mode keeps the scanner-gated search — text never
/// produces x86 clusters or structured deltas — while forced modes run on the
/// whole member.
fn rar29_filter_candidates(
    data: &[u8],
    policy: crate::options::FilterOptions,
) -> Vec<(Rar29FilterKind, Vec<std::ops::Range<usize>>)> {
    use crate::options::FilterMode;
    if data.is_empty() {
        return Vec::new();
    }
    let mut candidates: Vec<(Rar29FilterKind, Vec<std::ops::Range<usize>>)> = Vec::new();
    if policy.x86 == FilterMode::Forced {
        candidates.push((
            Rar29FilterKind::E8E9,
            std::iter::once(0..data.len()).collect(),
        ));
    } else if policy.x86 == FilterMode::Auto {
        let e8e9 = crate::codec::common::filters::auto_x86_filter_ranges(data, true);
        if !e8e9.is_empty() {
            candidates.push((Rar29FilterKind::E8E9, e8e9));
        }
        let e8 = crate::codec::common::filters::auto_x86_filter_ranges(data, false);
        if !e8.is_empty() {
            candidates.push((Rar29FilterKind::E8, e8));
        }
    }
    if policy.delta == FilterMode::Forced {
        let channels = policy.delta_channels.unwrap_or_else(|| {
            crate::codec::common::filters::auto_delta_filter_channels(data).unwrap_or(1)
        });
        candidates.push((
            Rar29FilterKind::Delta {
                channels: channels as usize,
            },
            std::iter::once(0..data.len()).collect(),
        ));
    } else if policy.delta == FilterMode::Auto
        && let Some(channels) = crate::codec::common::filters::auto_delta_filter_channels(data)
    {
        candidates.push((
            Rar29FilterKind::Delta {
                channels: channels as usize,
            },
            std::iter::once(0..data.len()).collect(),
        ));
    }
    if let Some(channels) = crate::codec::common::filters::auto_audio_filter_channels(data) {
        candidates.push((
            Rar29FilterKind::Audio { channels },
            std::iter::once(0..data.len()).collect(),
        ));
    }
    candidates
}

pub(super) fn best_rar29_member(
    data: &[u8],
    level: u8,
    policy: crate::options::FilterOptions,
) -> RarResult<Option<(Vec<u8>, u8)>> {
    use crate::codec::legacy::rar29_encoder::{Unpack29Encoder, options_for_level};
    if !(1..=5).contains(&level) {
        return Ok(None);
    }
    let options = options_for_level(level);
    let method = crate::format::rar4::RAR4_METHOD_STORE + level;
    let lz = Unpack29Encoder::with_options(options).encode_member(data)?;
    let mut best_len = lz.len();
    let mut best: (Vec<u8>, u8) = (lz, method);

    // Filters under the -mc policy: every candidate is measured with its own
    // throwaway encoder (no chain state).
    for (kind, ranges) in rar29_filter_candidates(data, policy) {
        let Ok(candidate) = Unpack29Encoder::with_options(options)
            .encode_member_with_filter_ranges(data, kind, &ranges)
        else {
            continue;
        };
        if candidate.len() < best_len {
            best_len = candidate.len();
            best = (candidate, method);
        }
    }
    if level >= 4
        && !data.is_empty()
        && let Ok(ppmd) = Unpack29Encoder::with_options(options).encode_ppmd_member(data)
        && ppmd.len() < best_len
    {
        best_len = ppmd.len();
        best = (ppmd, method);
    }
    if best_len < data.len() {
        Ok(Some(best))
    } else {
        Ok(None)
    }
}
