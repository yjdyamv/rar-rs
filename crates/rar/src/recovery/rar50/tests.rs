use std::io::Cursor;

use super::encode::{
    build_structural_inline_recovery_data, build_structural_inline_recovery_data_streaming,
    encode_inline_recovery_parity,
};
use super::gf16::{
    Gf16, apply_inverse_matrix, encode_parity_shards, invert_linear_system_matrix,
    make_encoder_matrix,
};
use super::plan::{
    InlineRecoveryPlan, crc64_rar_state, crc64_xz, plan_inline_recovery, split_prefix_shard_ranges,
    split_prefix_shards,
};
use super::repair::{
    reconstruct_data_shards, repair_inline_recovery_archive, repair_inline_recovery_prefix,
    repair_inline_recovery_prefix_shards,
};
use super::{Error, MAX_WINRAR602_DATA_SHARDS, shared_gf16};

#[test]
fn rar5_inline_recovery_plan_matches_fixture_formula_examples() {
    assert_eq!(
        plan_inline_recovery(65_681, 5).unwrap(),
        InlineRecoveryPlan {
            data_shards: 65,
            recovery_shards: 3,
            group_count: 1012,
            header_size: 592,
            shard_size: 1604,
        }
    );
    assert_eq!(
        plan_inline_recovery(65_681, 20).unwrap(),
        InlineRecoveryPlan {
            data_shards: 65,
            recovery_shards: 13,
            group_count: 1012,
            header_size: 592,
            shard_size: 1604,
        }
    );
}

#[test]
fn rar5_inline_recovery_plan_handles_clamps_and_large_prefixes() {
    assert_eq!(
        plan_inline_recovery(0, 0).unwrap(),
        InlineRecoveryPlan {
            data_shards: 1,
            recovery_shards: 1,
            group_count: 0,
            header_size: 80,
            shard_size: 80,
        }
    );
    assert_eq!(
        plan_inline_recovery(200 * 1024, 1000).unwrap(),
        InlineRecoveryPlan {
            data_shards: 200,
            recovery_shards: 200,
            group_count: 1024,
            header_size: 1672,
            shard_size: 2696,
        }
    );
}

#[test]
fn rar5_inline_recovery_plan_keeps_fixed_header_above_64k_groups() {
    let boundary = MAX_WINRAR602_DATA_SHARDS * 0x10000;
    assert_eq!(
        plan_inline_recovery(boundary, 1).unwrap(),
        InlineRecoveryPlan {
            data_shards: 200,
            recovery_shards: 2,
            group_count: 65_536,
            header_size: 1672,
            shard_size: 67_208,
        }
    );
    assert_eq!(
        plan_inline_recovery(boundary + 1, 1).unwrap(),
        InlineRecoveryPlan {
            data_shards: 200,
            recovery_shards: 2,
            group_count: 65_538,
            header_size: 1672,
            shard_size: 67_210,
        }
    );
}

#[test]
fn gf16_matches_rar5_polynomial_wrap() {
    let gf = Gf16::new();

    assert_eq!(gf.mul(0x8000, 2), 0x100b);
    assert_eq!(gf.mul(0, 0x1234), 0);
    assert_eq!(gf.mul(0x1234, 0), 0);
    assert_eq!(gf.mul(0, 0), 0);
    assert_eq!(gf.mul(1, 0x1234), 0x1234);
}

#[test]
fn shared_gf16_reuses_field_tables() {
    let first = shared_gf16() as *const Gf16;
    let second = shared_gf16() as *const Gf16;

    assert_eq!(first, second);
    assert_eq!(shared_gf16().mul(0x8000, 2), 0x100b);
}

#[test]
fn crc64_xz_matches_reference_vectors() {
    assert_eq!(crc64_xz(b""), 0);
    assert_eq!(crc64_xz(b"123456789"), 0x995d_c9bb_df19_39fa);
    assert_eq!(crc64_xz(b"testtesttest"), 0x7b1c_2d23_0ede_b436);
}

#[test]
fn raw_crc64_state_matches_reference_vector() {
    assert_eq!(crc64_rar_state(b""), 0);
    assert_eq!(crc64_rar_state(b"te\x80st"), 0xb5db_f958_3a6e_ed4a);
}

#[test]
fn rar5_prefix_split_produces_even_padded_data_shards() {
    let plan = InlineRecoveryPlan {
        data_shards: 3,
        recovery_shards: 1,
        group_count: 4,
        header_size: 96,
        shard_size: 100,
    };
    let shards = split_prefix_shards(b"abcdefghij", plan).unwrap();

    assert_eq!(
        shards,
        vec![b"abcd".to_vec(), b"efgh".to_vec(), b"ij\0\0".to_vec()]
    );
}

#[test]
fn rar5_prefix_split_rejects_prefix_larger_than_plan_capacity() {
    let plan = InlineRecoveryPlan {
        data_shards: 2,
        recovery_shards: 1,
        group_count: 2,
        header_size: 88,
        shard_size: 90,
    };

    assert_eq!(
        split_prefix_shards(b"abcde", plan),
        Err(Error::PrefixExceedsPlan)
    );
}

#[test]
fn gf16_inverse_round_trips_nonzero_elements() {
    let gf = Gf16::new();

    for value in [1, 2, 3, 0x100b, 0x8000, 0xffff] {
        let inverse = gf.inv(value).unwrap();
        assert_eq!(gf.mul(value, inverse), 1);
    }
    assert_eq!(gf.inv(0), Err(Error::SingularElement));
}

#[test]
fn rar5_cauchy_encoder_matrix_uses_inverse_xor_denominators() {
    let gf = Gf16::new();
    let matrix = make_encoder_matrix(3, 2).unwrap();

    assert_eq!(matrix.len(), 2);
    assert_eq!(matrix[0].len(), 3);
    for (i, row) in matrix.iter().enumerate() {
        for (j, &cell) in row.iter().enumerate() {
            let denominator = ((i + 3) ^ j) as u16;
            assert_eq!(gf.mul(cell, denominator), 1);
        }
    }
}

#[test]
fn rar5_cauchy_encoder_matrix_rejects_impossible_shard_counts() {
    assert_eq!(make_encoder_matrix(0, 1), Err(Error::TooManyShards));
    assert_eq!(make_encoder_matrix(1, 0), Err(Error::TooManyShards));
    assert_eq!(make_encoder_matrix(65535, 1), Err(Error::TooManyShards));
}

#[test]
fn rar5_recovery_inverse_matrix_solves_reused_equations() {
    let gf = shared_gf16();
    let equations = vec![vec![3, 5], vec![7, 11]];
    let expected = [0x1234, 0xabcd];
    let rhs = equations
        .iter()
        .map(|row| gf.mul(row[0], expected[0]) ^ gf.mul(row[1], expected[1]))
        .collect::<Vec<_>>();

    let inverse = invert_linear_system_matrix(gf, &equations).unwrap();
    let solved = apply_inverse_matrix(gf, &inverse, &rhs).unwrap();

    assert_eq!(solved, expected);
}

#[test]
fn rar5_parity_encoder_generates_systematic_recovery_shards() {
    let first = [1, 0, 2, 0, 3, 0, 4, 0];
    let parity = encode_parity_shards(&[&first], 1).unwrap();

    assert_eq!(parity, [first.to_vec()]);
}

#[test]
fn rar5_parity_encoder_applies_cauchy_matrix_coefficients() {
    let gf = Gf16::new();
    let first = [1, 0, 2, 0];
    let second = [3, 0, 4, 0];
    let matrix = make_encoder_matrix(2, 2).unwrap();
    let parity = encode_parity_shards(&[&first, &second], 2).unwrap();

    for recovery_index in 0..2 {
        for word_index in 0..2 {
            let offset = word_index * 2;
            let left = u16::from_le_bytes([first[offset], first[offset + 1]]);
            let right = u16::from_le_bytes([second[offset], second[offset + 1]]);
            let expected =
                gf.mul(matrix[recovery_index][0], left) ^ gf.mul(matrix[recovery_index][1], right);
            assert_eq!(
                u16::from_le_bytes([
                    parity[recovery_index][offset],
                    parity[recovery_index][offset + 1],
                ]),
                expected
            );
        }
    }
}

#[test]
fn rar5_inline_recovery_parity_splits_and_encodes_prefix() {
    let prefix = b"RAR5 inline recovery parity payload input";
    let (plan, parity) = encode_inline_recovery_parity(prefix, 10).unwrap();

    assert_eq!(plan, plan_inline_recovery(prefix.len() as u64, 10).unwrap());
    assert_eq!(parity.len(), plan.recovery_shards as usize);
    assert!(
        parity
            .iter()
            .all(|shard| shard.len() == plan.group_count as usize)
    );

    let data_shards = split_prefix_shards(prefix, plan).unwrap();
    let shard_refs: Vec<&[u8]> = data_shards.iter().map(Vec::as_slice).collect();
    assert_eq!(
        parity,
        encode_parity_shards(&shard_refs, plan.recovery_shards as usize).unwrap()
    );
}

#[test]
fn rar5_structural_inline_recovery_data_writes_chunks_and_crc64() {
    let prefix = b"RAR5 structural inline recovery data";
    let (plan, parity) = encode_inline_recovery_parity(prefix, 10).unwrap();
    let data = build_structural_inline_recovery_data(prefix, 10).unwrap();

    assert_eq!(data.len(), plan.payload_size().unwrap() as usize);
    for (shard_index, payload) in parity.iter().enumerate() {
        let chunk_start = shard_index * plan.shard_size as usize;
        let chunk = &data[chunk_start..chunk_start + plan.shard_size as usize];
        assert_eq!(&chunk[..4], b"{RB}");
        assert_eq!(
            u64::from_le_bytes(chunk[4..12].try_into().unwrap()),
            crc64_xz(&chunk[0x0c..])
        );
        assert_eq!(
            u32::from_le_bytes(chunk[0x0c..0x10].try_into().unwrap()) as u64,
            plan.shard_size
        );
        assert_eq!(
            u32::from_le_bytes(chunk[0x10..0x14].try_into().unwrap()) as u64,
            plan.header_size
        );
        assert_eq!(chunk[0x14], 1);
        assert_eq!(chunk[0x15], 1);
        assert_eq!(
            u64::from_le_bytes(chunk[0x22..0x2a].try_into().unwrap()),
            prefix.len() as u64
        );
        assert_eq!(
            u16::from_le_bytes(chunk[0x3e..0x40].try_into().unwrap()) as usize,
            shard_index
        );
        let shard_ranges = split_prefix_shard_ranges(prefix.len(), plan).unwrap();
        assert_eq!(
            u32::from_le_bytes(chunk[0x1e..0x22].try_into().unwrap()) as usize,
            shard_ranges.last().unwrap().len()
        );
        for (data_index, range) in shard_ranges.iter().enumerate() {
            let state_offset = 0x40 + data_index * 8;
            assert_eq!(
                u64::from_le_bytes(chunk[state_offset..state_offset + 8].try_into().unwrap()),
                crc64_rar_state(&prefix[range.clone()])
            );
        }
        assert_eq!(&chunk[plan.header_size as usize..], payload);
    }
}

#[test]
fn rar5_structural_inline_recovery_round_trips_above_64k_groups() {
    let prefix_len = (MAX_WINRAR602_DATA_SHARDS * 0x10000 + 1) as usize;
    let prefix: Vec<u8> = (0..prefix_len).map(|index| index as u8).collect();
    let plan = plan_inline_recovery(prefix.len() as u64, 1).unwrap();
    let recovery_data = build_structural_inline_recovery_data(&prefix, 1).unwrap();

    assert_eq!(plan.header_size, 1672);
    assert_eq!(plan.group_count, 65_538);
    assert_eq!(recovery_data.len(), plan.payload_size().unwrap() as usize);
    for shard_index in 0..plan.recovery_shards as usize {
        let chunk_start = shard_index * plan.shard_size as usize;
        let chunk_end = chunk_start + plan.shard_size as usize;
        let chunk = &recovery_data[chunk_start..chunk_end];
        assert_eq!(
            u32::from_le_bytes(chunk[0x0c..0x10].try_into().unwrap()) as u64,
            plan.shard_size
        );
        assert_eq!(
            u32::from_le_bytes(chunk[0x10..0x14].try_into().unwrap()) as u64,
            plan.header_size
        );
        assert_eq!(
            u64::from_le_bytes(chunk[0x04..0x0c].try_into().unwrap()),
            crc64_xz(&chunk[0x0c..])
        );
    }

    assert_eq!(
        repair_inline_recovery_prefix(&prefix, &recovery_data).unwrap(),
        prefix
    );
    let mut damaged = prefix.clone();
    damaged[0] ^= 0xff;
    assert_eq!(
        repair_inline_recovery_prefix(&damaged, &recovery_data).unwrap(),
        prefix
    );
}

#[test]
fn rar5_streaming_recovery_record_matches_buffered() {
    // The streaming encoder must produce byte-identical recovery records to
    // the buffered encoder, including partial/empty trailing shards and the
    // >64 KiB / >200 KiB plan boundaries. If these diverge, the on-disk
    // format no longer matches what UnRAR/WinRAR expect.
    let cases: &[(usize, u64)] = &[
        (0, 1),
        (1, 10),
        (65_681, 5),
        (200 * 1024, 25),
        (MAX_WINRAR602_DATA_SHARDS as usize * 0x10000 + 1, 10),
        (256 * 1024, 20),
        (128_000, 20),
        (32_000, 1),
    ];
    for (len, pct) in cases {
        let prefix: Vec<u8> = (0..*len).map(|i| (i * 31) as u8).collect();
        let buffered =
            build_structural_inline_recovery_data(&prefix, *pct).expect("buffered encode");
        let streamed = build_structural_inline_recovery_data_streaming(
            Cursor::new(prefix.clone()),
            prefix.len() as u64,
            *pct,
            None,
            1,
        )
        .expect("streamed encode");
        assert_eq!(
            streamed, buffered,
            "streaming recovery record diverged from buffered (len={len}, pct={pct})"
        );
    }
}

#[test]
fn rar5_structural_inline_recovery_uses_shared_final_state() {
    let prefix: Vec<u8> = (0..(256 * 1024)).map(|index| index as u8).collect();
    let (plan, parity) = encode_inline_recovery_parity(&prefix, 20).unwrap();
    assert!(plan.recovery_shards > 1);
    let data = build_structural_inline_recovery_data(&prefix, 20).unwrap();
    let expected = crc64_rar_state(&parity[0]);

    for shard_index in 0..plan.recovery_shards as usize {
        let chunk_start = shard_index * plan.shard_size as usize;
        let final_state_offset = chunk_start + 0x40 + plan.data_shards as usize * 8;
        assert_eq!(
            u64::from_le_bytes(
                data[final_state_offset..final_state_offset + 8]
                    .try_into()
                    .unwrap()
            ),
            expected
        );
    }
}

#[test]
fn rar5_parity_encoder_rejects_invalid_shard_shapes() {
    assert_eq!(encode_parity_shards(&[], 1), Err(Error::TooManyShards));
    assert_eq!(
        encode_parity_shards(&[&[1, 2, 3]], 1),
        Err(Error::OddShardSize)
    );
    assert_eq!(
        encode_parity_shards(&[&[1, 2], &[3, 4, 5, 6]], 1),
        Err(Error::ShardSizeMismatch)
    );
}

#[test]
fn rar5_inline_recovery_repairs_single_damaged_data_shard() {
    let prefix: Vec<u8> = (0..32_000).map(|index| (index * 17) as u8).collect();
    let recovery_data = build_structural_inline_recovery_data(&prefix, 20).unwrap();
    let mut damaged = prefix.clone();
    damaged[1500..1537].fill(0xa5);

    let repaired = repair_inline_recovery_prefix(&damaged, &recovery_data).unwrap();

    assert_eq!(repaired, prefix);
}

#[test]
fn rar5_inline_recovery_skips_damaged_recovery_chunks_if_enough_survive() {
    let prefix: Vec<u8> = (0..32_000).map(|index| (index * 13) as u8).collect();
    let mut recovery_data = build_structural_inline_recovery_data(&prefix, 20).unwrap();
    recovery_data[0x48] ^= 0xff;
    let mut damaged = prefix.clone();
    damaged[1024..1300].fill(0xa5);

    let repaired = repair_inline_recovery_prefix(&damaged, &recovery_data).unwrap();

    assert_eq!(repaired, prefix);
}

#[test]
fn rar5_inline_recovery_repairs_multiple_damaged_data_shards() {
    let prefix: Vec<u8> = (0..128_000).map(|index| (index * 31) as u8).collect();
    let recovery_data = build_structural_inline_recovery_data(&prefix, 20).unwrap();
    let mut damaged = prefix.clone();
    damaged[100..500].fill(0x11);
    damaged[4_000..4_400].fill(0x22);
    damaged[9_000..9_400].fill(0x33);

    let repaired = repair_inline_recovery_prefix(&damaged, &recovery_data).unwrap();

    assert_eq!(repaired, prefix);
}

#[test]
fn rar5_inline_recovery_returns_only_repaired_shard_ranges() {
    let prefix: Vec<u8> = (0..128_000).map(|index| (index * 29) as u8).collect();
    let recovery_data = build_structural_inline_recovery_data(&prefix, 20).unwrap();
    let mut damaged = prefix.clone();
    damaged[100..500].fill(0x11);
    damaged[9_000..9_400].fill(0x33);

    let repaired_shards = repair_inline_recovery_prefix_shards(
        prefix.len(),
        &recovery_data,
        |range| Ok(damaged[range].to_vec()),
        None,
    )
    .unwrap();
    assert!(!repaired_shards.is_empty());

    let mut repaired = damaged;
    for (range, data) in repaired_shards {
        assert_eq!(range.len(), data.len());
        repaired[range].copy_from_slice(&data);
    }

    assert_eq!(repaired, prefix);
}

/// Two RR generations of the same protected size have the same plan, so
/// only the recorded data-shard states distinguish them. Mixing their
/// chunk streams must be rejected rather than solved into the wrong
/// bytes.
#[test]
fn rar5_inline_recovery_rejects_mismatched_generations() {
    let prefix_a: Vec<u8> = (0..32_000).map(|index| (index * 13) as u8).collect();
    let prefix_b: Vec<u8> = (0..32_000).map(|index| (index * 13 + 1) as u8).collect();
    let mut recovery = build_structural_inline_recovery_data(&prefix_a, 20).unwrap();
    recovery.extend_from_slice(&build_structural_inline_recovery_data(&prefix_b, 20).unwrap());
    let mut damaged = prefix_a.clone();
    damaged[1024..1300].fill(0xa5);

    assert!(matches!(
        repair_inline_recovery_prefix_shards(
            prefix_a.len(),
            &recovery,
            |range| Ok(damaged[range].to_vec()),
            None,
        ),
        Err(Error::BadRecoveryChunk)
    ));
}

#[test]
fn rar5_inline_recovery_archive_scans_chunks_and_repairs_prefix() {
    let prefix: Vec<u8> = (0..32_000).map(|index| (index * 13) as u8).collect();
    let recovery_data = build_structural_inline_recovery_data(&prefix, 20).unwrap();
    let mut archive = prefix.clone();
    archive.extend_from_slice(b"service header bytes before chunks");
    archive.extend_from_slice(&recovery_data);
    archive.extend_from_slice(b"end bytes");
    let mut damaged = archive.clone();
    damaged[256..320].fill(0x5a);

    let repaired = repair_inline_recovery_archive(&damaged).unwrap();

    assert_eq!(repaired, archive);
}

#[test]
fn rar5_inline_recovery_archive_accepts_healthy_archive() {
    let prefix: Vec<u8> = (0..32_000).map(|index| (index * 17) as u8).collect();
    let recovery_data = build_structural_inline_recovery_data(&prefix, 20).unwrap();
    let mut archive = prefix.clone();
    archive.extend_from_slice(b"service header bytes before chunks");
    archive.extend_from_slice(&recovery_data);
    archive.extend_from_slice(b"end bytes");

    let repaired = repair_inline_recovery_archive(&archive).unwrap();

    assert_eq!(repaired, archive);
}

#[test]
fn rar5_inline_recovery_rejects_unrepairable_damage_count() {
    let prefix = b"small prefix with only one parity shard".repeat(100);
    let recovery_data = build_structural_inline_recovery_data(&prefix, 1).unwrap();
    let mut damaged = prefix.clone();
    damaged[0] ^= 0xff;
    damaged[1024] ^= 0xff;

    assert_eq!(
        repair_inline_recovery_prefix(&damaged, &recovery_data),
        Err(Error::TooManyDamagedShards)
    );
}

#[test]
fn rar5_reconstruct_data_shards_repairs_missing_shards_from_parity() {
    let first = b"abcdefgh".to_vec();
    let second = b"ijklmnop".to_vec();
    let third = b"qrstuvwx".to_vec();
    let refs = [first.as_slice(), second.as_slice(), third.as_slice()];
    let parity = encode_parity_shards(&refs, 2).unwrap();

    let reconstructed = reconstruct_data_shards(
        &[Some(&first), None, Some(&third)],
        &[(0, parity[0].as_slice())],
    )
    .unwrap();

    assert_eq!(reconstructed[0], first);
    assert_eq!(reconstructed[1], second);
    assert_eq!(reconstructed[2], third);
}

#[test]
fn rar5_reconstruct_data_shards_repairs_multiple_missing_shards() {
    let first = b"abcdefgh".to_vec();
    let second = b"ijklmnop".to_vec();
    let third = b"qrstuvwx".to_vec();
    let refs = [first.as_slice(), second.as_slice(), third.as_slice()];
    let parity = encode_parity_shards(&refs, 2).unwrap();

    let reconstructed = reconstruct_data_shards(
        &[None, Some(&second), None],
        &[(0, parity[0].as_slice()), (1, parity[1].as_slice())],
    )
    .unwrap();

    assert_eq!(reconstructed[0], first);
    assert_eq!(reconstructed[1], second);
    assert_eq!(reconstructed[2], third);
}

#[cfg(feature = "parallel")]
#[test]
fn parallel_parity_encode_matches_scalar_reference() {
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut shards = vec![vec![0u8; 128]; 4];
    for shard in &mut shards {
        for byte in shard.iter_mut() {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            *byte = (state.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8;
        }
    }
    let refs: Vec<&[u8]> = shards.iter().map(|s| s.as_slice()).collect();
    let got = encode_parity_shards(&refs, 3).unwrap();

    // Scalar reference row-by-row.
    let matrix = make_encoder_matrix(refs.len(), 3).unwrap();
    let gf = shared_gf16();
    let mut expected = vec![vec![0u8; 128]; 3];
    for (row_index, row) in matrix.iter().enumerate() {
        for word_offset in (0..128).step_by(2) {
            let mut symbol = 0u16;
            for (data_index, shard) in refs.iter().enumerate() {
                let data_symbol = u16::from_le_bytes([shard[word_offset], shard[word_offset + 1]]);
                symbol ^= gf.mul(row[data_index], data_symbol);
            }
            expected[row_index][word_offset..word_offset + 2]
                .copy_from_slice(&symbol.to_le_bytes());
        }
    }
    assert_eq!(got, expected);
}
