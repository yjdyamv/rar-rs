//! Editing and repairing: rewrite members in place, protect the archive with a
//! recovery record, then repair it after damage.
//!
//! Run with `cargo run --example edit_and_repair`.

use rar_rs::{ArchiveEditor, ArchiveVersion, ArchiveWriter, EditPlan, EntryWriteOptions};

fn main() -> rar_rs::RarResult<()> {
    let dir = std::env::temp_dir().join("rar-rs-edit-and-repair");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    // Incompressible-ish payloads, so the archive is comfortably larger than
    // the 512-byte sector grid the recovery record protects.
    let mut state = 0x2545_f491u32;
    let mut noise = |len: usize| -> Vec<u8> {
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 16) as u8
            })
            .collect()
    };
    std::fs::write(dir.join("report.txt"), noise(4000))?;
    std::fs::write(dir.join("data.bin"), noise(4000))?;

    // A legacy RAR4 archive with a 10% recovery record: the record protects
    // every 512-byte sector before it (tags + parity), which is what `rar r`
    // repairs from.
    let archive = dir.join("protected.rar");
    let mut writer = ArchiveWriter::create_with(
        &archive,
        rar_rs::WriterOptions::default()
            .compression(ArchiveVersion::V29)
            .recovery_percent(10),
    )?;
    writer.add_path(dir.join("report.txt"), EntryWriteOptions::new())?;
    writer.add_path(dir.join("data.bin"), EntryWriteOptions::new())?;
    writer.finish()?;

    // Edit: one plan, one atomic rewrite. Deleting, renaming, the archive
    // comment and the recovery strength can be mixed; IDs issued before the
    // rewrite go stale, so re-resolve names afterwards.
    let mut editor = ArchiveEditor::open(&archive)?;
    let data = editor.unique_entry("data.bin")?;
    let comment = b"handed off 2026-09".to_vec();
    let report = editor.apply(
        EditPlan::new()
            .rename(data, "payload.bin")
            .set_comment(comment.clone())
            .set_recovery(25),
    )?;
    println!("renamed {} member(s)", report.renamed());
    drop(editor);

    // Damage one sector inside the protected prefix and repair it from the
    // record. The report names every damaged sector, so an intact archive
    // (`is_intact()`) is distinguishable from damage the record cannot reach.
    let damaged = dir.join("damaged.rar");
    let mut bytes = std::fs::read(&archive)?;
    bytes[700..712].fill(0x5a);
    std::fs::write(&damaged, &bytes)?;
    let fixed = dir.join("fixed.rar");
    let repair = rar_rs::repair_legacy_archive_path(&damaged, &fixed)?;
    assert!(repair.repaired);
    for sector in &repair.sectors {
        println!(
            "sector {} (offsets {:X}...{:X}) damaged - {}",
            sector.index,
            sector.offset,
            sector.offset + 512,
            if sector.recovered {
                "data recovered"
            } else {
                "cannot recover data"
            }
        );
    }
    println!("repaired copy: {}", fixed.display());

    // Rebuild: when no record can help, the surviving members are decoded out
    // of the damaged archive into a fresh one (this is what `rar r` falls back
    // to). Header damage is skipped past; members that fail to verify are
    // dropped and reported.
    let rebuilt = dir.join("rebuilt.rar");
    let reconstruct = rar_rs::reconstruct_archive_path(&damaged, &rebuilt, None)?;
    println!(
        "rebuilt {} member(s), dropped {:?}, header damage skipped: {}",
        reconstruct.recovered().len(),
        reconstruct.dropped(),
        reconstruct.skipped_damage()
    );
    Ok(())
}
