use super::*;

use super::super::{
    FILTER_ARM, FILTER_DELTA, FILTER_E8, FILTER_E8E9, FilterSpec, MAX_FILTER_BLOCK_LENGTH,
    encode_with_auto_delta_filter, encode_with_auto_x86_filter, encode_with_filters,
    pick_delta_channel,
};
use super::engine::checked_dict_size;
use crate::error::RarError;
use crate::version::ArchiveVersion;

/// The RAR5 format ceiling is 4 GiB (log 15); RAR7 sizes come from
/// the byte count. Larger values are rejected.
#[test]
fn checked_dict_size_accepts_range_and_rejects_larger() {
    assert_eq!(checked_dict_size(0, None).unwrap(), 128 * 1024);
    assert_eq!(checked_dict_size(13, None).unwrap(), 1024 * 1024 * 1024);
    assert_eq!(checked_dict_size(15, None).unwrap(), 4 * 1024 * 1024 * 1024);
    let err = checked_dict_size(16, None).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("maximum 15"), "{msg}");
    // RAR7: byte count, non-power-of-two rounds the window up.
    assert_eq!(
        checked_dict_size(0, Some(6 * 1024 * 1024 * 1024)).unwrap(),
        8 * 1024 * 1024 * 1024
    );
}

/// Fuzz regression: block flags with the reserved bit 5 set previously
/// produced a byte_count of 8, overflowing the u32 block-size shift in
/// debug builds. The block size field is 2 bits (1-4 bytes); reserved
/// bits are ignored.
#[test]
fn block_flags_with_reserved_bit_do_not_overflow() {
    let stream = [0xe4u8, 0x00, 0xe0, 0x00, 0xe0, 0x00, 0x00];
    let result = std::panic::catch_unwind(|| {
        let _ = decode_standalone(&stream, 78090, 0, None, ArchiveVersion::V50);
    });
    assert!(
        result.is_ok(),
        "decode must not panic on reserved flag bits"
    );
}

/// Fuzz regression: a valid archive with a 3-byte block size field
/// (byte_count 3) still decodes.
#[test]
fn three_byte_block_size_field_decodes() {
    let data = b"rar5 three-byte block size regression test data ".repeat(64);
    let packed = crate::codec::encode_raw(&data, 3, 3, ArchiveVersion::V50);
    let back = decode_standalone(&packed, data.len() as u64, 3, None, ArchiveVersion::V50).unwrap();
    assert_eq!(back, data);
}

/// The buffered decoder must reject a stream that stops before producing
/// the declared unpacked size instead of returning a short buffer (the
/// streaming path already checked this; the buffered one did not).
#[test]
fn underproduced_stream_is_rejected() {
    let data = b"underproduction regression data ".repeat(256);
    let packed = crate::codec::encode_raw(&data, 3, 3, ArchiveVersion::V50);
    let err = decode_standalone(&packed, data.len() as u64 + 1, 3, None, ArchiveVersion::V50)
        .unwrap_err();
    assert!(matches!(err, RarError::Format(_)), "got {err}");
}

/// Regression: streaming decode (used by `extract_all`) applied split
/// filter records at the wrong staging offset once part of the staging
/// buffer had already been written out. Members whose filter region
/// exceeds MAX_FILTER_BLOCK_LENGTH are split into multiple records, so
/// the streaming path must produce byte-identical output to the
/// buffered path for every filter type.
#[test]
fn streaming_decode_matches_buffered_for_split_filter_records() {
    fn pattern(filter_type: u8, size: usize) -> Vec<u8> {
        match filter_type {
            FILTER_E8 | FILTER_E8E9 => {
                let mut data = vec![0x90u8; size];
                let mut pos = 0usize;
                while pos + 5 <= size {
                    data[pos] = if filter_type == FILTER_E8 || pos.is_multiple_of(170) {
                        0xE8
                    } else {
                        0xE9
                    };
                    let addr = ((pos as u32).wrapping_mul(7)) & 0x00FF_FFFF;
                    data[pos + 1..pos + 5].copy_from_slice(&addr.to_le_bytes());
                    pos += 85;
                }
                data
            }
            FILTER_ARM => {
                let mut data = vec![0x00u8; size];
                let mut pos = 0usize;
                while pos + 4 <= size {
                    data[pos + 3] = 0xEB;
                    let off = ((pos as u32).wrapping_mul(3)) & 0x00FF_FFFF;
                    data[pos..pos + 3].copy_from_slice(&off.to_le_bytes()[..3]);
                    pos += 64;
                }
                data
            }
            _ => (0..size).map(|i| (i % 251) as u8).collect(),
        }
    }

    let cases: [(u8, u8); 5] = [
        (FILTER_E8, 0),
        (FILTER_E8E9, 0),
        (FILTER_ARM, 0),
        (FILTER_DELTA, 1),
        (FILTER_DELTA, 4),
    ];
    for &(filter_type, channels) in &cases {
        for &size in &[MAX_FILTER_BLOCK_LENGTH as usize + 1, 300_000usize] {
            let data = pattern(filter_type, size);
            let spec = FilterSpec::new(filter_type, channels, 0, size as u32);
            let packed = encode_with_filters(&data, 3, 0, &[spec], ArchiveVersion::V50).unwrap();
            let buffered =
                decode_standalone(&packed, size as u64, 0, None, ArchiveVersion::V50).unwrap();
            let mut streamed = Vec::new();
            let written = decode_standalone_to_writer(
                &packed,
                size as u64,
                0,
                None,
                ArchiveVersion::V50,
                &mut streamed,
            )
            .unwrap();
            assert_eq!(written, size as u64);
            assert_eq!(
                streamed, buffered,
                "streaming != buffered for filter {filter_type:#x}, channels {channels}, size {size}"
            );
            assert_eq!(
                streamed, data,
                "streaming != original for filter {filter_type:#x}, channels {channels}, size {size}"
            );
        }
    }
}

/// The size-based channel selection must pick the frame size (bytes ×
/// channels) for interleaved little-endian samples: each byte position of
/// the frame packs best as its own delta lane, so 16-bit stereo picks 4,
/// 32-bit stereo 8, 24-bit 3-channel 9, 32-bit 4-channel 16.
#[test]
fn delta_selection_prefers_frame_size() {
    fn correlated_samples(bytes: usize, channels: usize, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes * channels * n);
        let mut val = vec![0i64; channels];
        let mut state = 0x1234_5678u64;
        for _ in 0..n {
            for v in &mut val {
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                let r = (state >> 33) as u32;
                *v += (r % 8) as i64 - 4;
                let value = *v;
                for b in 0..bytes {
                    out.push((value >> (8 * b)) as u8);
                }
            }
        }
        out
    }
    for (bytes, channels, expect) in [
        (1usize, 1usize, 1u8),
        (2, 1, 2),
        (2, 2, 4),
        (3, 1, 3),
        (3, 3, 9),
        (1, 2, 2),
        (2, 4, 8),
        (4, 2, 8),
        (4, 4, 16),
    ] {
        let data = correlated_samples(bytes, channels, 100_000);
        let got = pick_delta_channel(&data, 3, 0, ArchiveVersion::V50).unwrap();
        assert_eq!(
            got,
            Some(expect),
            "bytes={bytes} channels={channels}: expected frame size {expect}, got {got:?}"
        );
    }
}

/// The automatic delta (multimedia) filter must round-trip correlated
/// multi-channel data and pack it smaller than STORE, while refusing
/// random/text data.
#[test]
fn auto_delta_filter_roundtrips_and_packs_smaller() {
    // Correlated N-bit interleaved samples (small per-sample deltas),
    // matching the kind of 8/16/24/32-bit multi-channel data WinRAR
    // deltas: `bytes` little-endian bytes per sample × `channels` lanes.
    fn correlated(bytes: usize, channels: usize, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes * channels * n);
        let mut val = vec![0i64; channels];
        let mut state = 0xABCDEF01u64;
        for _ in 0..n {
            for v in &mut val {
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                *v += ((state >> 33) as u32 % 8) as i64 - 4;
                for b in 0..bytes {
                    out.push((*v >> (8 * b)) as u8);
                }
            }
        }
        out
    }

    for (bytes, channels) in [
        (1usize, 1usize),
        (2, 1),
        (2, 2),
        (3, 1),
        (3, 3),
        (4, 2),
        (4, 4),
    ] {
        let data = correlated(bytes, channels, 200_000);
        let packed = encode_with_auto_delta_filter(&data, 3, 0, ArchiveVersion::V50, 1, None)
            .unwrap()
            .expect("delta scan must find a beneficial channel count");
        assert!(
            packed.len() < data.len(),
            "bytes={bytes} channels={channels}: delta-filtered encoding should compress: {} vs {}",
            packed.len(),
            data.len()
        );
        let back =
            decode_standalone(&packed, data.len() as u64, 0, None, ArchiveVersion::V50).unwrap();
        assert_eq!(back, data, "bytes={bytes} channels={channels}");
    }

    // Text must NOT be delta-filtered: delta cannot beat plain LZSS on it.
    let text = b"the quick brown fox jumps over the lazy dog. ".repeat(6_000);
    assert!(
        encode_with_auto_delta_filter(&text, 3, 0, ArchiveVersion::V50, 1, None)
            .unwrap()
            .is_none(),
        "text must fall back to plain LZSS"
    );

    // Random data must NOT be delta-filtered.
    let mut state = 0x9E37_9B97_7F4A_7C15u64;
    let mut random = vec![0u8; 200_000];
    for b in random.iter_mut() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        *b = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u8;
    }
    assert!(
        encode_with_auto_delta_filter(&random, 3, 0, ArchiveVersion::V50, 1, None)
            .unwrap()
            .is_none()
    );
}

/// Synthetic x86 code: E8/E9 opcodes with plausible relative targets,
/// dense enough that the automatic scan finds a region. The auto-filter
/// path must pick it up, pack it, and decode back to the original.
#[test]
fn auto_x86_filter_roundtrips_and_packs_smaller() {
    fn x86ish(size: usize) -> Vec<u8> {
        let mut data = vec![0x90u8; size]; // NOP padding
        let mut pos = 0usize;
        while pos + 5 <= size {
            data[pos] = if pos.is_multiple_of(170) { 0xE8 } else { 0xE9 };
            let addr = ((pos as u32).wrapping_mul(7)) & 0x00FF_FFFF;
            data[pos + 1..pos + 5].copy_from_slice(&addr.to_le_bytes());
            pos += 85;
        }
        data
    }

    let data = x86ish(400_000);
    let packed = encode_with_auto_x86_filter(&data, 3, 0, ArchiveVersion::V50, 1, None)
        .unwrap()
        .expect("x86 scan must find regions");
    assert!(
        packed.len() < data.len(),
        "filtered encoding should compress: {} vs {}",
        packed.len(),
        data.len()
    );
    let back = decode_standalone(&packed, data.len() as u64, 0, None, ArchiveVersion::V50).unwrap();
    assert_eq!(back, data);

    // Non-code data with isolated opcodes must NOT be filtered.
    let mut sparse = vec![0x41u8; 20_000];
    sparse[100] = 0xE8;
    sparse[10_000] = 0xE8;
    assert!(
        encode_with_auto_x86_filter(&sparse, 3, 0, ArchiveVersion::V50, 1, None)
            .unwrap()
            .is_none()
    );
}

/// Regression: filter positions and E8 transform offsets are
/// member-relative even when the member sits at a non-zero offset of a
/// solid chain (WinRAR's `WrittenFileSize` is per-file while the filter
/// record positions are stream-absolute). Decoding a filtered member
/// with shared decoder state must reproduce the original bytes.
#[test]
fn filtered_member_at_solid_offset_decodes_member_relative() {
    fn x86ish(size: usize) -> Vec<u8> {
        let mut data = vec![0x90u8; size];
        let mut pos = 0usize;
        while pos + 5 <= size {
            data[pos] = if pos.is_multiple_of(170) { 0xE8 } else { 0xE9 };
            let addr = ((pos as u32).wrapping_mul(7)) & 0x00FF_FFFF;
            data[pos + 1..pos + 5].copy_from_slice(&addr.to_le_bytes());
            pos += 85;
        }
        data
    }

    // A first plain member fills the shared window; the filtered member
    // then decodes at a non-zero stream offset.
    let first = b"solid chain prefix data padding padding padding".repeat(64);
    let member = x86ish(120_000);

    let packed_first = crate::codec::encode_raw(&first, 3, 3, ArchiveVersion::V50);
    let packed_member = encode_with_auto_x86_filter(&member, 3, 0, ArchiveVersion::V50, 1, None)
        .unwrap()
        .expect("x86 scan must find regions");

    let mut state = DecoderState::new(128 * 1024);
    let decoded_first = decode_raw(
        &packed_first,
        first.len() as u64,
        DecodeOptions {
            dict_size_log: 3,
            dict_size_bytes: None,
            variant: ArchiveVersion::V50,
            state: Some(&mut state),
        },
    )
    .unwrap();
    assert_eq!(decoded_first, first);

    let decoded_member = decode_raw(
        &packed_member,
        member.len() as u64,
        DecodeOptions {
            dict_size_log: 0,
            dict_size_bytes: None,
            variant: ArchiveVersion::V50,
            state: Some(&mut state),
        },
    )
    .unwrap();
    assert_eq!(
        decoded_member, member,
        "filtered member must decode member-relative at a solid offset"
    );
}
