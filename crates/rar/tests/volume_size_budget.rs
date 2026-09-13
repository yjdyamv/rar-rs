//! Regression test for the multi-volume size budget: every volume file
//! must be at most the configured `volume_size`.
//!
//! The split loop used to size chunks with a mid-chunk header estimate
//! (which omits the final chunk's BLAKE2sp hash / OWNER extra records)
//! while the final chunk was written with the full extra area, so a chunk
//! that turned out to be final could overflow the volume.

use rar_rs::{ArchiveWriter, CompressionLevel, EntryWriteOptions, WriterOptions};

fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
    let mut state = seed
        .wrapping_mul(0x2545_F491_4F6C_DD1D)
        .wrapping_add(0x9E37_79B9);
    (0..n)
        .map(|_| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8
        })
        .collect()
}

fn assert_volumes_within_budget(path: &std::path::Path, volume_size: u64, context: &str) {
    let volumes = rar_rs::discover_volumes(path);
    assert!(volumes.len() >= 2, "{context}: expected a split archive");
    for volume in &volumes {
        let actual = std::fs::metadata(volume).expect("volume metadata").len();
        assert!(
            actual <= volume_size,
            "{context}: {} is {actual} bytes (limit {volume_size})",
            volume.display()
        );
    }
}

/// STORE members around the boundary where the final chunk's BLAKE2sp
/// record used to push the second volume over 4096 bytes.
#[test]
fn blake2_multivolume_volumes_stay_within_budget() {
    let volume_size = 4096u64;
    for len in 8000usize..=8100 {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("budget.rar");
        let data = pseudo_random(len, 0xB1A2);
        {
            let mut rar = ArchiveWriter::create_with(
                &path,
                WriterOptions::default()
                    .volume_size(volume_size)
                    .blake2(true),
            )
            .expect("create archive");
            let opts = EntryWriteOptions::new()
                .compression_level(CompressionLevel::try_from(0u8).unwrap());
            rar.add_bytes("payload.bin", &data, opts)
                .expect("add member");
            rar.finish().expect("close archive");
        }
        assert_volumes_within_budget(&path, volume_size, &format!("blake2 len {len}"));
    }
}

/// The same budget with member encryption: the final chunk also switches
/// from the flag-stripped encryption record to the full one.
#[test]
fn encrypted_blake2_multivolume_volumes_stay_within_budget() {
    let volume_size = 4096u64;
    for len in 8000usize..=8100 {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("budget-enc.rar");
        let data = pseudo_random(len, 0xE9C0);
        {
            let mut rar = ArchiveWriter::create_with(
                &path,
                WriterOptions::default()
                    .volume_size(volume_size)
                    .blake2(true)
                    .password("secret"),
            )
            .expect("create archive");
            let opts = EntryWriteOptions::new()
                .compression_level(CompressionLevel::try_from(0u8).unwrap());
            rar.add_bytes("payload.bin", &data, opts)
                .expect("add member");
            rar.finish().expect("close archive");
        }
        assert_volumes_within_budget(&path, volume_size, &format!("encrypted len {len}"));
    }
}
