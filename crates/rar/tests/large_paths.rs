//! Large-path regression tests at a scale that runs in the default suite.
//!
//! The extreme-scale tiers (> 4 GiB files, RAR7 v70 > 4 GiB dictionaries,
//! 128 MiB long-range ratio vs WinRAR) are irreducible — they exist only
//! to hit boundaries that smaller data cannot — and stay `#[ignore]`d in
//! `crates/rar-cli/tests/winrar_interop.rs`. These tests exercise the
//! same code paths (spill pipeline, chunked multi-volume writes, chained
//! CBC, long-range matching) at sizes that complete in seconds, so the
//! default suite still catches regressions in them.

#![allow(deprecated)] // legacy facade (add_bytes/close/read) — kept for the scale tiers

use rar5::RarArchive;
use std::path::Path;

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

fn sha256(path: &Path) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let mut f = std::fs::File::open(path).expect("open");
    let mut buf = vec![0u8; 1 << 20];
    loop {
        use std::io::Read;
        let n = f.read(&mut buf).expect("read");
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Create a sparse file of `size` bytes (reads as zeros, allocates almost
/// nothing on disk).
fn create_sparse(path: &Path, size: u64) {
    let f = std::fs::File::create(path).expect("create sparse file");
    f.set_len(size).expect("extend sparse file");
}

/// 256 MiB sparse single-file streaming compression round trip: the
/// all-zero input must stream through the spill pipeline (memory stays
/// bounded) and compress hard, and streamed extraction must reproduce it
/// byte-for-byte. The reduced-scale sibling of the ignored > 4 GiB test.
#[test]
fn large_sparse_streamed_compression_roundtrips() {
    let dir = make_temp_dir();
    let size = 256 * 1024 * 1024u64; // 256 MiB
    let src = dir.path().join("big.bin");
    create_sparse(&src, size);

    let arc = dir.path().join("big.rar");
    {
        let mut rar =
            RarArchive::create_with_options(&arc, rar5::CreateOptions::default()).unwrap();
        rar.add(&src, 3).unwrap();
        rar.close().unwrap();
    }
    assert!(
        std::fs::metadata(&arc).unwrap().len() < 64 * 1024 * 1024,
        "all-zero input must compress well"
    );

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    {
        let mut rar = RarArchive::open(&arc).unwrap();
        rar.extract_with_options(
            "big.bin",
            &out,
            rar5::ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(std::fs::metadata(out.join("big.bin")).unwrap().len(), size);
    assert_eq!(sha256(&out.join("big.bin")), sha256(&src));
}

/// 192 MiB stored + encrypted multi-volume round trip: chained CBC across
/// exact-sized volumes (per-chunk ciphertext CRCs, per-chunk encryption
/// records), byte-exact volume sizes, and a streamed extraction round
/// trip. The reduced-scale sibling of the ignored > 4 GiB encrypted
/// multi-volume test.
#[test]
fn large_streamed_encrypted_multivolume_roundtrips() {
    let dir = make_temp_dir();
    let src = dir.path().join("big.bin");
    write_repeated(&src, 0x5A, 192 * 1024 * 1024); // stored: fills volumes

    let vol_size = 32 * 1024 * 1024u64;
    let arc = dir.path().join("big.rar");
    {
        let mut rar = rar5::RarArchive::create_with_options(
            &arc,
            rar5::CreateOptions {
                password: Some("s3cret".into()),
                volume_size: Some(vol_size),
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 0).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar5::discover_volumes(&arc);
    assert!(
        volumes.len() >= 6,
        "expected several exact volumes, got {}",
        volumes.len()
    );
    for vol in &volumes[..volumes.len() - 1] {
        assert_eq!(
            std::fs::metadata(vol).unwrap().len(),
            vol_size,
            "non-final volume must be byte-exact"
        );
    }

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    {
        let mut rar = RarArchive::open_with_password(&volumes[0], "s3cret").unwrap();
        rar.extract_with_options(
            "big.bin",
            &out,
            rar5::ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(
        std::fs::metadata(out.join("big.bin")).unwrap().len(),
        192 * 1024 * 1024u64
    );
    assert_eq!(sha256(&out.join("big.bin")), sha256(&src));
}

/// Long-range matching (`-mcl` semantics) at reduced scale: a 32 MiB
/// file whose second half copies its random first half at exactly 16 MiB
/// distance. The near finder only sees ~12 MiB of context (8 MiB tail +
/// 4 MiB chunk) and the 32 MiB dictionary window bounds representable
/// distances, so the sampled long-range history must supply the match:
/// the archive must compress the copy away and decode byte-identically.
/// The ratio-vs-WinRAR gate stays in the ignored 128 MiB test; this
/// locks correctness into the default suite.
#[test]
fn long_range_matches_roundtrip_at_scale() {
    let dir = make_temp_dir();
    let src = dir.path().join("pair.bin");
    let half = 16 * 1024 * 1024usize;
    let mut data = vec![0u8; half * 2];
    {
        let mut state = 42u64;
        for b in data[..half].iter_mut() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (state >> 33) as u8;
        }
        let first = data[..half].to_vec();
        data[half..].copy_from_slice(&first);
    }
    std::fs::write(&src, &data).unwrap();

    let arc = dir.path().join("pair.rar");
    {
        let mut rar = rar5::RarArchive::create_with_options(
            &arc,
            rar5::CreateOptions {
                dict_size_log: Some(8), // 32 MiB window: 16 MiB copy is long-range
                ..Default::default()
            },
        )
        .unwrap();
        rar.add(&src, 3).unwrap();
        rar.close().unwrap();
    }
    // The distant copy must compress away: the random first half alone
    // already costs ~16 MiB, so a packed size barely above the first
    // half's cost proves the copy half collapsed (raw it would be ~32
    // MiB).
    let packed = std::fs::metadata(&arc).unwrap().len();
    assert!(
        packed < half as u64 * 11 / 10,
        "long-range match did not fire: packed {packed} >= 1.1x half {half}"
    );

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    {
        let mut rar = RarArchive::open(&arc).unwrap();
        rar.extract_with_options(
            "pair.bin",
            &out,
            rar5::ExtractOptions {
                max_unpacked_bytes: None,
                max_total_unpacked_bytes: None,
                ..Default::default()
            },
        )
        .unwrap();
    }
    assert_eq!(sha256(&out.join("pair.bin")), sha256(&src));
}
