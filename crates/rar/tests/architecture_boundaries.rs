use std::path::{Path, PathBuf};

fn rust_sources_below(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read source directory") {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            rust_sources_below(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn rar40_does_not_import_models_from_rar50_headers() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_dir = manifest_dir.join("src").join("rar40");
    let mut sources = Vec::new();
    rust_sources_below(&source_dir, &mut sources);

    let offenders: Vec<_> = sources
        .into_iter()
        .filter(|path| {
            std::fs::read_to_string(path)
                .expect("read RAR4 source")
                .contains("rar50::headers")
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "RAR4 must use crate::model instead of rar50::headers: {offenders:?}"
    );
}
