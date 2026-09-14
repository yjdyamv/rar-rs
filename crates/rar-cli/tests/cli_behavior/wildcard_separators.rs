//! Wildcard and mask path-separator handling. WinRAR accepts `\` wherever
//! `/` is accepted in wildcard arguments and masks; stored names are always
//! `/`-normalized. The backslash forms are Windows-only because on Unix a
//! backslash is a literal filename character.

use crate::support::{RAR_CLI, cli_names, make_temp_dir, make_tree};
use std::path::Path;

fn run(args: &[&str], cwd: &Path) -> std::process::Output {
    std::process::Command::new(RAR_CLI)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn run_ok(args: &[&str], cwd: &Path) {
    let out = run(args, cwd);
    assert!(
        out.status.success(),
        "{args:?} failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The `/` forms keep working on every platform, for wildcard arguments and
/// for `-x` / `-n` masks.
#[test]
fn cli_wildcard_args_and_masks_accept_forward_slashes() {
    let dir = make_temp_dir();
    make_tree(dir.path());

    run_ok(&["a", "-idq", "slash.rar", "sub/*.txt"], dir.path());
    assert_eq!(cli_names(&dir.path().join("slash.rar")), ["sub/f3.txt"]);

    run_ok(
        &["a", "-idq", "-ep1", "slash_ep1.rar", "sub/*.txt"],
        dir.path(),
    );
    assert_eq!(cli_names(&dir.path().join("slash_ep1.rar")), ["f3.txt"]);

    run_ok(
        &["a", "-idq", "-xsub/*.bin", "slash_x.rar", "sub"],
        dir.path(),
    );
    assert_eq!(
        cli_names(&dir.path().join("slash_x.rar")),
        ["sub", "sub/f3.txt"]
    );

    run_ok(
        &["a", "-idq", "-nsub/*.txt", "slash_n.rar", "sub"],
        dir.path(),
    );
    assert_eq!(cli_names(&dir.path().join("slash_n.rar")), ["sub/f3.txt"]);
}

#[cfg(windows)]
#[test]
fn cli_wildcard_args_accept_backslashes_on_windows() {
    let dir = make_temp_dir();
    make_tree(dir.path());

    run_ok(&["a", "-idq", "bslash.rar", "sub\\*.txt"], dir.path());
    assert_eq!(cli_names(&dir.path().join("bslash.rar")), ["sub/f3.txt"]);

    run_ok(
        &["a", "-idq", "-ep1", "bslash_ep1.rar", "sub\\*.txt"],
        dir.path(),
    );
    assert_eq!(cli_names(&dir.path().join("bslash_ep1.rar")), ["f3.txt"]);
}

#[cfg(windows)]
#[test]
fn cli_masks_accept_backslashes_on_windows() {
    let dir = make_temp_dir();
    make_tree(dir.path());

    run_ok(
        &["a", "-idq", "-xsub\\*.txt", "bslash_x.rar", "sub"],
        dir.path(),
    );
    assert_eq!(
        cli_names(&dir.path().join("bslash_x.rar")),
        ["sub", "sub/f4.bin"]
    );

    run_ok(
        &["a", "-idq", "-nsub\\*.txt", "bslash_n.rar", "sub"],
        dir.path(),
    );
    assert_eq!(cli_names(&dir.path().join("bslash_n.rar")), ["sub/f3.txt"]);
}
