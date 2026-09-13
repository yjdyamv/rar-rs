//! Regression tests for the solid-chain dictionary window.
//!
//! The shared decoder window is fixed by the chain head's dictionary, so a
//! later member whose own size selects a larger dictionary must be clamped
//! to the chain-start value. Before the clamp, the encoder emitted
//! distances beyond the window and the member decoded to garbage (CRC
//! mismatch in rar-rs, "Not enough memory" from UnRAR). Reading such an
//! archive must now fail with [`RarError::Format`] instead of silently
//! wrapping out-of-window distances.

use rar_rs::{
    ArchiveReader, ArchiveWriter, CompressionLevel, EntryWriteOptions, RarError, SolidMode,
    WriterOptions, wire,
};

fn compressible(seed: u8, n: usize) -> Vec<u8> {
    let pat: Vec<u8> = (0..64u8)
        .map(|i| i.wrapping_mul(7).wrapping_add(seed))
        .collect();
    let mut out = Vec::with_capacity(n + pat.len());
    while out.len() < n {
        out.extend_from_slice(&pat);
    }
    out.truncate(n);
    out
}

fn write_solid_archive(path: &std::path::Path, small: &[u8], large: &[u8]) {
    let mut rar = ArchiveWriter::create_with(
        path,
        WriterOptions::default().solid_mode(SolidMode::Continuous),
    )
    .expect("create solid archive");
    let opts = EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap());
    rar.add_bytes("small.bin", small, opts)
        .expect("add small member");
    rar.add_bytes("large.bin", large, opts)
        .expect("add large member");
    rar.finish().expect("close solid archive");
}

/// The 64 KiB head selects a 128 KiB dictionary; the ~5 MiB second member
/// would select 8 MiB and repeats the head's bytes 320 KiB back. The writer
/// must clamp it to the chain window and the archive must round-trip.
#[test]
fn solid_chain_clamps_a_larger_member_dictionary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("solid-dict.rar");

    let small = compressible(7, 64 * 1024);
    let mut large = compressible(200, 256 * 1024);
    large.extend_from_slice(&small);
    large.extend_from_slice(&compressible(41, 5 * 1024 * 1024 - large.len()));

    write_solid_archive(&path, &small, &large);

    let mut reader = ArchiveReader::open(&path).expect("open archive");
    let small_id = reader.unique_entry("small.bin").expect("small entry");
    let large_id = reader.unique_entry("large.bin").expect("large entry");
    let small_entry = reader.entry(small_id).expect("small metadata");
    let large_entry = reader.entry(large_id).expect("large metadata");
    assert_eq!(
        small_entry.comp_dict_size(),
        0,
        "chain head declares the 128 KiB selection"
    );
    assert_eq!(
        large_entry.comp_dict_size(),
        0,
        "continuation member is clamped to the chain-start dictionary"
    );
    assert!(large_entry.comp_solid(), "large member continues the chain");

    assert_eq!(reader.read_entry(small_id).expect("read small"), small);
    assert_eq!(reader.read_entry(large_id).expect("read large"), large);
}

/// Rebuild `name`'s header with a dictionary log of `dict_log` and splice it
/// back into a copy of the archive, simulating the pre-fix (growing-window)
/// archives a broken writer produced.
fn patch_member_dict(archive: &[u8], name: &str, dict_log: u8) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(archive);
    cursor.set_position(8);
    loop {
        let meta = wire::read_block(&mut cursor, None)
            .expect("read block")
            .expect("block");
        if meta.block_type == 0x02 {
            let mut hdr = wire::FileHeader::from_raw(&meta.raw, meta.block_start)
                .expect("parse member header");
            if hdr.name == name {
                hdr.comp_dict_size = dict_log;
                hdr.dict_size_bytes = None;
                let mut out = archive.to_vec();
                out.splice(
                    meta.block_start as usize..meta.data_offset as usize,
                    hdr.to_bytes(),
                );
                return out;
            }
        }
        cursor.set_position(meta.data_end);
        if meta.block_type == 0x05 {
            panic!("member {name} not found");
        }
    }
}

/// A member header declaring a dictionary larger than the chain window must
/// be rejected with `Format`, not decoded against the too-small window.
#[test]
fn read_rejects_a_solid_member_with_a_larger_dictionary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("solid-patched.rar");

    let small = compressible(7, 64 * 1024);
    let large = compressible(9, 512 * 1024);
    write_solid_archive(&path, &small, &large);

    let bytes = std::fs::read(&path).expect("read archive");
    let patched = patch_member_dict(&bytes, "large.bin", 6); // 8 MiB
    let patched_path = dir.path().join("solid-patched-header.rar");
    std::fs::write(&patched_path, &patched).expect("write patched archive");

    let mut reader = ArchiveReader::open(&patched_path).expect("open patched archive");
    let id = reader.unique_entry("large.bin").expect("large entry");
    let err = reader
        .read_entry(id)
        .expect_err("growing solid dictionary must be rejected");
    assert!(
        matches!(err, RarError::Format(_)),
        "expected Format error, got {err}"
    );
}

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

#[test]
fn per_extension_reset_group_starts_a_new_window() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("se.rar");
    let a = compressible(1, 64 * 1024);
    let a2 = compressible(2, 64 * 1024);
    // Reset group head: 256 KiB of pattern, a 256 KiB random block and its
    // exact copy (a 256 KiB-back match, beyond the first group's 128 KiB
    // window), then filler to select an 8 MiB dictionary.
    let mut b = compressible(3, 256 * 1024);
    let rnd = pseudo_random(256 * 1024, 99);
    b.extend_from_slice(&rnd);
    b.extend_from_slice(&rnd);
    b.extend_from_slice(&compressible(30, 4 * 1024 * 1024));
    let b2 = compressible(4, 2 * 1024 * 1024);
    {
        let mut rar = ArchiveWriter::create_with(
            &path,
            WriterOptions::default().solid_mode(SolidMode::PerExtension),
        )
        .unwrap();
        let opts =
            EntryWriteOptions::new().compression_level(CompressionLevel::try_from(3u8).unwrap());
        rar.add_bytes("a.txt", &a, opts).unwrap();
        rar.add_bytes("a2.txt", &a2, opts).unwrap();
        rar.add_bytes("b.bin", &b, opts).unwrap();
        rar.add_bytes("b2.bin", &b2, opts).unwrap();
        rar.finish().unwrap();
    }
    let mut reader = ArchiveReader::open(&path).unwrap();
    for (name, expected) in [
        ("a.txt", &a),
        ("a2.txt", &a2),
        ("b.bin", &b),
        ("b2.bin", &b2),
    ] {
        let id = reader.unique_entry(name).unwrap();
        assert_eq!(&reader.read_entry(id).unwrap(), expected, "{name}");
    }
}
