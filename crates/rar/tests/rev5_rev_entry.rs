//! A REV5 `.rev` path passed to the rebuild entry point must route to the
//! RAR5 recovery codec, not the legacy RAR 1.5–4.x one. Regression for the
//! 8-byte `REV5_SIGNATURE` comparison that was truncated to 7 bytes and
//! misrouted every REV5 set into `rev3` (`no recovery volumes found`).

use std::fs;
use std::path::PathBuf;

use rar_rs::ArchiveWriter;

#[test]
fn rebuild_through_a_rev5_rev_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rcv.rar");
    let payload_a: Vec<u8> = (0..120_000u32).map(|i| (i % 251) as u8).collect();
    let payload_b: Vec<u8> = (0..60_000u32).map(|i| (i % 253) as u8).collect();
    {
        let mut rar = ArchiveWriter::create_with(
            &path,
            rar_rs::WriterOptions::default()
                .volume_size(60_000)
                .recovery_volume_count(2),
        )
        .unwrap();
        let opts0 = rar_rs::EntryWriteOptions::new()
            .compression_level(rar_rs::CompressionLevel::try_from(0u8).unwrap());
        rar.add_bytes("a.bin", &payload_a, opts0).unwrap();
        rar.add_bytes("b.bin", &payload_b, opts0).unwrap();
        rar.finish().unwrap();
    }
    let volumes = rar_rs::discover_volumes(&path);
    assert!(volumes.len() > 1, "precondition: multi-volume set");
    let rev = dir.path().join("rcv.part1.rev");
    assert!(rev.exists(), "precondition: .rev files present");

    // Delete a middle volume and rebuild it through the `.rev` entry point
    // (like `rar rc set.part1.rev`).
    let victim: PathBuf = volumes[1].clone();
    fs::remove_file(&victim).unwrap();
    let rebuilt = rar_rs::rebuild_missing_volumes(&rev)
        .expect("a REV5 .rev path must rebuild through the RAR5 codec");
    assert!(rebuilt.contains(&victim), "the missing volume is rebuilt");

    let volumes = rar_rs::discover_volumes(&path);
    let mut rar = rar_rs::ArchiveReader::open(&volumes[0]).unwrap();
    assert_eq!(
        rar.read_entry(rar.unique_entry("a.bin").unwrap()).unwrap(),
        payload_a
    );
    assert_eq!(
        rar.read_entry(rar.unique_entry("b.bin").unwrap()).unwrap(),
        payload_b
    );
}
