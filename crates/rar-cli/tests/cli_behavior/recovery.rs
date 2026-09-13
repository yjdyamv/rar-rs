use crate::support::{RAR_CLI, UNRAR_CLI};
/// `rar rv` on an existing volume set + `rar rc` round trip (WinRAR 7.23
/// semantics: bare count, capped at 10x the volume count).
#[test]
fn cli_rv_creates_recovery_volumes_and_rc_rebuilds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().join("mv");

    // A 10+ volume set (the writer zero-pads names to part01..partNN,
    // like WinRAR) covering both the default-percent and the count forms
    // of `rv`; pseudo-random bytes so the member actually spans the
    // -v100k volumes.
    let mut big = Vec::with_capacity(2_500_000);
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..2_500_000 {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        big.push((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8);
    }
    let src = dir.path().join("big.bin");
    std::fs::write(&src, &big).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-v100k", "-y"])
        .arg(&base)
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());
    let first = format!("{}.part01.rar", base.display());
    assert!(std::path::Path::new(&first).exists());

    // Default rv = 10% of the volume count (ceil).
    let nd = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".rar")
        })
        .count();
    assert!(nd >= 10, "expected a multi-volume set, got {nd} volumes");
    let status = std::process::Command::new(RAR_CLI)
        .args(["rv"])
        .arg(&first)
        .status()
        .unwrap();
    assert!(status.success());
    let default_count = (nd * 10).div_ceil(100); // ceil(10%)
    assert!(
        std::path::Path::new(&format!("{}.part{default_count:02}.rev", base.display())).exists()
    );
    assert!(
        !std::path::Path::new(&format!(
            "{}.part{:02}.rev",
            base.display(),
            default_count + 1
        ))
        .exists()
    );

    // Count form, embedded token (`rv3`) -> 3 .rev files.
    let status = std::process::Command::new(RAR_CLI)
        .args(["rv3"])
        .arg(&first)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(std::path::Path::new(&format!("{}.part03.rev", base.display())).exists());
    assert!(!std::path::Path::new(&format!("{}.part04.rev", base.display())).exists());

    // Delete a volume and rebuild it with `rc`; the archive must test OK.
    let vol3 = format!("{}.part03.rar", base.display());
    std::fs::remove_file(&vol3).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["rc"])
        .arg(&first)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(std::path::Path::new(&vol3).exists());
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["t"])
        .arg(&first)
        .status()
        .unwrap();
    assert!(status.success());

    // Percent form via the subcommand positional (`rv 50%`) -> ceil(50%).
    let status = std::process::Command::new(RAR_CLI)
        .args(["rv"])
        .arg(&first)
        .arg("50%")
        .status()
        .unwrap();
    assert!(status.success());
    let expected = (nd as u32 * 50).div_ceil(100) as usize;
    assert!(std::path::Path::new(&format!("{}.part{expected:02}.rev", base.display())).exists());
    assert!(
        !std::path::Path::new(&format!("{}.part{:02}.rev", base.display(), expected + 1)).exists()
    );
}

/// `rar rv` / `rar rc` round trip on a legacy RAR4 (`.rar`/`.rNN`) volume
/// set: the recovery volume gets WinRAR's legacy full-parity layout with
/// the counts in the file name, and a deleted volume is rebuilt exactly.
#[test]
fn cli_rv_and_rc_roundtrip_rar4_volume_sets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut big = vec![0u8; 400_000];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for b in &mut big {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *b = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8;
    }
    let src = dir.path().join("rnd.bin");
    std::fs::write(&src, &big).unwrap();
    let base = dir.path().join("mv4.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-m0", "-v100k", "-idq"])
        .arg(&base)
        .arg(&src)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());

    let status = std::process::Command::new(RAR_CLI)
        .args(["rv", "-idq"])
        .arg(&base)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar rv must accept a RAR4 set");
    let rev = dir.path().join("mv44_1_1.rev");
    assert!(rev.exists(), "legacy RAR4 rev name expected: {rev:?}");

    // Delete a middle volume and rebuild it byte-identically.
    let victim = dir.path().join("mv4.r00");
    let saved = std::fs::read(&victim).unwrap();
    std::fs::remove_file(&victim).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["rc", "-idq"])
        .arg(&base)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "rar rc must rebuild a RAR4 set");
    assert_eq!(std::fs::read(&victim).unwrap(), saved);

    // The rebuilt set must pass our own test.
    let status = std::process::Command::new(UNRAR_CLI)
        .args(["t", "-idq"])
        .arg(&base)
        .status()
        .unwrap();
    assert!(status.success());
}

/// `a -ma4 -v -rv2` creates the legacy recovery volumes at close time and
/// official-style `rc` finds them after a volume loss.
#[test]
fn cli_ma4_create_with_rv_creates_recovery_volumes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut big = vec![0u8; 400_000];
    let mut x: u64 = 0xDEAD_BEEF_CAFE_F00D;
    for b in &mut big {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *b = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8;
    }
    let src = dir.path().join("rnd.bin");
    std::fs::write(&src, &big).unwrap();
    let base = dir.path().join("rv4.rar");
    let status = std::process::Command::new(RAR_CLI)
        .args(["a", "-ma4", "-m0", "-v100k", "-rv2", "-idq"])
        .arg(&base)
        .arg(&src)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(dir.path().join("rv44_2_1.rev").exists());
    assert!(dir.path().join("rv44_2_2.rev").exists());

    let victim = dir.path().join("rv4.r01");
    let saved = std::fs::read(&victim).unwrap();
    std::fs::remove_file(&victim).unwrap();
    let status = std::process::Command::new(RAR_CLI)
        .args(["rc", "-idq"])
        .arg(&base)
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(&victim).unwrap(), saved);
}
