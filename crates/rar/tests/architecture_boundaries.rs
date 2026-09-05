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
    let source_dir = manifest_dir.join("src").join("format").join("rar4");
    let mut sources = Vec::new();
    rust_sources_below(&source_dir, &mut sources);

    let offenders: Vec<_> = sources
        .into_iter()
        .filter(|path| {
            std::fs::read_to_string(path)
                .expect("read RAR4 source")
                .contains("rar5::headers")
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "RAR4 must use crate::model instead of rar5 headers: {offenders:?}"
    );
}

/// Every `use` line under `dir` that contains one of `forbidden`.
fn use_lines_with(dir: &Path, forbidden: &[&str]) -> Vec<(String, String)> {
    let mut sources = Vec::new();
    rust_sources_below(dir, &mut sources);
    let mut offenders = Vec::new();
    for path in sources {
        for line in std::fs::read_to_string(&path).unwrap().lines() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("use ") {
                continue;
            }
            for token in forbidden {
                if line.contains(token) {
                    offenders.push((
                        path.strip_prefix(dir).unwrap().display().to_string(),
                        line.trim().to_string(),
                    ));
                }
            }
        }
    }
    offenders
}

/// The typed role facades (reader/writer/editor) orchestrate the legacy
/// facade; they must not reach format, codec, crypto or recovery internals
/// directly. Those seams stay inside `archive/mod`, `create`,
/// `transaction` and the format modules.
#[test]
fn role_facades_stay_off_format_and_codec_internals() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let forbidden = [
        "crate::format",
        "crate::codec",
        "crate::codec",
        "crate::crypto",
        "crate::recovery",
    ];
    let offenders = use_lines_with(&manifest_dir.join("src/archive"), &forbidden)
        .into_iter()
        .filter(|(file, _)| {
            matches!(
                file.as_str(),
                "reader.rs" | "writer.rs" | "editor.rs" | "tests.rs"
            )
        })
        .collect::<Vec<_>>();
    assert!(
        offenders.is_empty(),
        "role facades must not import format/codec internals: {offenders:?}"
    );
}

/// Filesystem and model policy are leaf layers: they must not import
/// archive/format/codec internals (nothing above them).
#[test]
fn fs_and_model_policy_do_not_depend_upward() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let forbidden = [
        "crate::archive",
        "crate::format",
        "crate::codec",
        "crate::codec",
        "crate::crypto",
        "crate::recovery",
    ];
    for leaf in ["src/fs", "src/model"] {
        let offenders = use_lines_with(&manifest_dir.join(leaf), &forbidden);
        assert!(
            offenders.is_empty(),
            "{leaf} must not depend upward: {offenders:?}"
        );
    }
}
