//! RAR7 (v70) at small scale via the `CreateOptions::force_v70` test seam.
//!
//! WinRAR only writes v70 members when the dictionary exceeds 4 GiB (the
//! `-md8g` tests need a > 4 GiB source and stay `#[ignore]`d), so the v70
//! header paths, the 5+5-bit dictionary encoding and the DCX distance
//! table had no default-suite coverage. `force_v70` writes legal v70
//! headers (`comp_version` 1) with any declared dictionary — the format
//! does not require > 4 GiB — letting these tests run the v70 archive
//! I/O at small scale. WinRAR compatibility at this scale is not part of
//! the validated surface; the seam is for our own round trips.
//!
//! Note on sizes: the declared dictionary is capped at twice the member
//! size (WinRAR's selection rule), so members here are >= 4 MiB to keep
//! the requested 6-8 MiB dictionaries intact.

use rar5::RarArchive;

#[path = "support/mod.rs"]
mod support;
#[allow(unused_imports)]
use support::*;

/// Deterministic pseudo-random bytes (LCG) — incompressible, so a
/// level-3 member keeps its size and actually splits across volumes.
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

/// `compressible`-style data with a distant copy (second half = first
/// half) so long matches exercise the DCX distance coding.
fn distant_copy(seed: u8, half: usize) -> Vec<u8> {
    let mut data = compressible(seed, half);
    data.reserve(half);
    let first = data.clone();
    data.extend_from_slice(&first);
    data
}

/// v70 members: `comp_version` 1, exact `dict_size_bytes` round trip
/// (both a power of two and a non-power exercising the 1/32 increment
/// bits), and byte-identical reads. Without `force_v70` the same small
/// dictionary must stay a plain v50 member.
#[test]
fn v70_forced_headers_and_roundtrip() {
    for dict in [6u64 * 1024 * 1024, 8 * 1024 * 1024] {
        let dir = make_temp_dir();
        let arc = dir.path().join("v70.rar");
        let a = compressible(11, 4 * 1024 * 1024);
        let b = distant_copy(12, 2 * 1024 * 1024);
        {
            let mut rar = RarArchive::create_with_options(
                &arc,
                rar5::CreateOptions {
                    dict_size_bytes: Some(dict),
                    force_v70: true,
                    ..Default::default()
                },
            )
            .unwrap();
            rar.add_bytes("a.bin", &a, 3).unwrap();
            rar.add_bytes("b.bin", &b, 3).unwrap();
            rar.close().unwrap();
        }
        let mut rar = RarArchive::open(&arc).unwrap();
        for (name, expected) in [("a.bin", &a), ("b.bin", &b)] {
            let entry = rar.get_entry(name).unwrap();
            assert_eq!(entry.header.comp_version, 1, "v70 header for {name}");
            assert_eq!(
                entry.header.dict_size_bytes,
                Some(dict),
                "declared dictionary round trip for {name}"
            );
            assert_eq!(&rar.read(name).unwrap(), expected, "bytes for {name}");
        }
    }

    // Same small dictionary without the seam: plain v50, no dict bytes.
    let dir = make_temp_dir();
    let arc = dir.path().join("v50.rar");
    {
        let mut rar = RarArchive::create_with_options(
            &arc,
            rar5::CreateOptions {
                dict_size_bytes: Some(6 * 1024 * 1024),
                force_v70: false,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("a.bin", b"plain v50", 3).unwrap();
        rar.close().unwrap();
    }
    let mut rar = RarArchive::open(&arc).unwrap();
    let entry = rar.get_entry("a.bin").unwrap();
    assert_eq!(entry.header.comp_version, 0, "still v50 without the seam");
    assert_eq!(entry.header.dict_size_bytes, None, "no v70 dict declared");
    assert_eq!(rar.read("a.bin").unwrap(), b"plain v50");
}

/// v70 + solid: the shared LZ window carries the DCX member state across
/// members; all members stay byte-identical. Every member is >= 4 MiB so
/// each declares the full 8 MiB dictionary (per-member 2x-file cap).
#[test]
fn v70_forced_solid_roundtrip() {
    let dir = make_temp_dir();
    let arc = dir.path().join("v70s.rar");
    let a = compressible(21, 4 * 1024 * 1024);
    let b = distant_copy(22, 2 * 1024 * 1024);
    let c = compressible(23, 4 * 1024 * 1024);
    {
        let mut rar = RarArchive::create_with_options(
            &arc,
            rar5::CreateOptions {
                solid: true,
                dict_size_bytes: Some(8 * 1024 * 1024),
                force_v70: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("a.bin", &a, 3).unwrap();
        rar.add_bytes("b.bin", &b, 3).unwrap();
        rar.add_bytes("c.bin", &c, 3).unwrap();
        rar.close().unwrap();
    }
    let mut rar = RarArchive::open(&arc).unwrap();
    assert_eq!(rar.namelist(), ["a.bin", "b.bin", "c.bin"]);
    for (name, expected) in [("a.bin", &a), ("b.bin", &b), ("c.bin", &c)] {
        let entry = rar.get_entry(name).unwrap();
        assert_eq!(entry.header.comp_version, 1, "v70 solid member {name}");
        assert_eq!(
            entry.header.dict_size_bytes,
            Some(8 * 1024 * 1024),
            "solid member {name} dictionary"
        );
        assert_eq!(&rar.read(name).unwrap(), expected, "solid bytes for {name}");
    }
}

/// v70 + multi-volume: DCX members split across volume boundaries and
/// reassemble byte-identically from the first volume. The member mixes
/// compressible and incompressible halves: it passes the
/// incompressibility probe (so the v70 compressed path runs) while its
/// packed size still exceeds one 2 MiB volume.
#[test]
fn v70_forced_multivolume_roundtrip() {
    let dir = make_temp_dir();
    let arc = dir.path().join("v70m.rar");
    let mut a = compressible(31, 8 * 1024 * 1024);
    a.extend_from_slice(&pseudo_random(8 * 1024 * 1024, 32));
    {
        let mut rar = RarArchive::create_with_options(
            &arc,
            rar5::CreateOptions {
                volume_size: Some(2 * 1024 * 1024),
                dict_size_bytes: Some(8 * 1024 * 1024),
                force_v70: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("a.bin", &a, 3).unwrap();
        rar.close().unwrap();
    }
    let volumes = rar5::discover_volumes(&arc);
    assert!(volumes.len() >= 2, "precondition: multi-volume set");
    let mut rar = RarArchive::open(&volumes[0]).unwrap();
    let entry = rar.get_entry("a.bin").unwrap();
    assert_eq!(entry.header.comp_version, 1, "v70 multi-volume member");
    assert_eq!(&rar.read("a.bin").unwrap(), &a);
}

/// v70 + file-level encryption: the payload encryption path is
/// independent of the dictionary, but the combination must still round
/// trip (chained CBC over DCX blocks).
#[test]
fn v70_forced_encrypted_roundtrip() {
    let dir = make_temp_dir();
    let arc = dir.path().join("v70e.rar");
    let a = compressible(41, 4 * 1024 * 1024);
    {
        let mut rar = RarArchive::create_with_options(
            &arc,
            rar5::CreateOptions {
                password: Some("s3cret".into()),
                dict_size_bytes: Some(6 * 1024 * 1024),
                force_v70: true,
                ..Default::default()
            },
        )
        .unwrap();
        rar.add_bytes("a.bin", &a, 3).unwrap();
        rar.close().unwrap();
    }
    let mut rar = RarArchive::open_with_password(&arc, "s3cret").unwrap();
    let entry = rar.get_entry("a.bin").unwrap();
    assert_eq!(entry.header.comp_version, 1, "v70 encrypted member");
    assert_eq!(&rar.read("a.bin").unwrap(), &a);
}
