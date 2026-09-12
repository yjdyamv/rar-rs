use crate::args::parse_size;
use crate::staging::update_archive_transactionally;

#[test]
fn size_parsing_checks_multiplication_overflow() {
    assert_eq!(parse_size("2k"), Ok(2 * 1024));
    assert!(parse_size("18446744073709551615g").is_err());
}

#[test]
fn failed_archive_transaction_preserves_original_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("archive.rar");
    std::fs::write(&archive, b"original").unwrap();

    let result = update_archive_transactionally(&archive, |staged| {
        std::fs::write(staged, b"damaged staged copy").unwrap();
        Err("injected append failure".into())
    });

    assert!(result.is_err(), "expected the injected failure");
    assert!(
        result
            .as_ref()
            .err()
            .is_some_and(|error| error.message().contains("injected append failure")),
        "unexpected error: {result:?}"
    );
    assert_eq!(std::fs::read(&archive).unwrap(), b"original");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn successful_archive_transaction_replaces_original_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("archive.rar");
    std::fs::write(&archive, b"original").unwrap();

    update_archive_transactionally(&archive, |staged| {
        std::fs::write(staged, b"replacement").map_err(|error| error.to_string())
    })
    .unwrap();

    assert_eq!(std::fs::read(&archive).unwrap(), b"replacement");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
