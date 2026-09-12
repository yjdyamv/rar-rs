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
