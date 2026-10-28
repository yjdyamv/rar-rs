//! Member order and directory-entry shape, measured against WinRAR.
//!
//! WinRAR sorts the members of a solid archive by extension, then by base
//! name, and appends the directory entries last, deepest level first. The
//! expectations below were captured from `rar a` (6.23) on the same tree; a
//! stored archive (`-m0`) and `-ds` keep the input order instead.

use crate::support::{RAR_CLI, make_temp_dir};
use rar_rs::ArchiveReader;

/// Create the fixture tree used by the ordering tests and return the argument
/// list, deliberately unsorted, with the directory last.
fn fixture(root: &std::path::Path) -> Vec<&'static str> {
    for name in ["z.txt", "a.txt", "m.log", "B.LOG", "c.bin"] {
        std::fs::write(root.join(name), format!("content of {name}\n")).unwrap();
    }
    std::fs::create_dir_all(root.join("sub").join("inner")).unwrap();
    std::fs::write(root.join("sub").join("nested.txt"), b"n\n").unwrap();
    std::fs::write(root.join("sub").join("deep.cpp"), b"d\n").unwrap();
    std::fs::write(root.join("sub").join("inner").join("f.txt"), b"i\n").unwrap();
    vec!["z.txt", "a.txt", "m.log", "B.LOG", "c.bin", "sub"]
}

/// Member names in archive order.
fn member_order(archive: &std::path::Path) -> Vec<String> {
    ArchiveReader::open(archive)
        .unwrap()
        .entries()
        .map(|entry| entry.name().to_string())
        .collect()
}

fn create_with(root: &std::path::Path, switches: &[&str]) -> Vec<String> {
    let archive = root.join("out.rar");
    let status = std::process::Command::new(RAR_CLI)
        .arg("a")
        .arg("-idq")
        .arg("-o+")
        .args(switches)
        .arg(&archive)
        .args(fixture(root))
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "{switches:?} must succeed");
    member_order(&archive)
}

const SOLID: [&str; 3] = ["-ma5", "-s", "-m3"];

/// A solid archive groups its members by extension (case-insensitively) and
/// sorts each group by base name; the directory entries follow, deepest
/// level first. Captured from WinRAR for this exact tree.
#[test]
fn cli_solid_members_follow_winrars_group_order() {
    let dir = make_temp_dir();
    assert_eq!(
        create_with(dir.path(), &SOLID),
        [
            "c.bin",
            "sub/deep.cpp",
            "B.LOG",
            "m.log",
            "a.txt",
            "sub/inner/f.txt",
            "sub/nested.txt",
            "z.txt",
            "sub/inner",
            "sub",
        ]
    );
}

/// Directories are stored without a trailing separator — the directory flag
/// marks them, exactly like WinRAR (`lb` used to print `sub\`).
#[test]
fn cli_directory_members_carry_no_trailing_separator() {
    let dir = make_temp_dir();
    let names = create_with(dir.path(), &SOLID);
    assert!(
        names.iter().all(|name| !name.ends_with('/')),
        "directory names must not carry a separator: {names:?}"
    );
    assert!(names.iter().any(|name| name == "sub"), "{names:?}");
    assert!(names.iter().any(|name| name == "sub/inner"), "{names:?}");
}

/// A stored archive (`-m0`) and `-ds` keep the input order (the directory's
/// own contents still come out in scan order), like WinRAR.
#[test]
fn cli_stored_and_no_sort_keep_the_input_order() {
    let dir = make_temp_dir();
    let expected = [
        "z.txt",
        "a.txt",
        "m.log",
        "B.LOG",
        "c.bin",
        "sub/deep.cpp",
        "sub/inner/f.txt",
        "sub/nested.txt",
        "sub/inner",
        "sub",
    ];
    assert_eq!(create_with(dir.path(), &["-ma5", "-s", "-m0"]), expected);
    let dir = make_temp_dir();
    assert_eq!(
        create_with(dir.path(), &["-ma5", "-s", "-m3", "-ds"]),
        expected
    );
}

/// The members an update appends are sorted the same way, even for a stored
/// (`-m0`) archive and without `-s`: WinRAR always orders them by extension.
#[test]
fn cli_update_appends_members_in_winrars_order() {
    let dir = make_temp_dir();
    std::fs::write(dir.path().join("a.txt"), b"alpha\n").unwrap();
    std::fs::write(dir.path().join("c.bin"), b"charlie\n").unwrap();
    std::fs::write(dir.path().join("z.txt"), b"zeta\n").unwrap();

    let archive = dir.path().join("upd.rar");
    let create = std::process::Command::new(RAR_CLI)
        .args(["a", "-idq", "-ma5", "-m3"])
        .arg(&archive)
        .arg("a.txt")
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(create.success());

    // Arguments deliberately unsorted: z, then a, then c.
    let update = std::process::Command::new(RAR_CLI)
        .args(["u", "-idq", "-o+", "-ma5", "-m0"])
        .arg(&archive)
        .args(["z.txt", "a.txt", "c.bin"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(update.success());
    assert_eq!(
        member_order(&archive),
        ["a.txt", "c.bin", "z.txt"],
        "the appended members follow the extension-then-name order"
    );
}
