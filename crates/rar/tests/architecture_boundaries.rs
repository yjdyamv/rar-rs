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

/// The RAR5 read/write paths are self-contained: family dispatching lives in
/// `format::shared::extract`, so no RAR5 source may reach into the legacy
/// families (the reverse direction is pinned by
/// `rar40_does_not_import_models_from_rar50_headers`).
#[test]
fn rar50_does_not_import_the_legacy_families() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_dir = manifest_dir.join("src").join("format").join("rar5");
    let mut sources = Vec::new();
    rust_sources_below(&source_dir, &mut sources);

    let offenders: Vec<_> = sources
        .into_iter()
        .filter(|path| {
            let text = std::fs::read_to_string(path).expect("read RAR5 source");
            text.contains("format::rar4")
                || text.contains("format::rar13")
                || text.contains("format::{rar4")
                || text.contains("format::{rar13")
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "RAR5 must not depend on the legacy families: {offenders:?}"
    );
}

/// Lines of every `.rs` file below `dir`, skipping two things that are not
/// dependencies:
///
/// - comment lines (a doc-comment mention of `crate::format` is prose, not a
///   dependency), and
/// - the body of an inline `mod tests { ... }` block, plus every `tests.rs`
///   file and `tests/` subdirectory. Test code may cross layers freely; these
///   invariants describe the shipped code.
///
/// Scanning whole lines rather than `use` statements matters: a reference
/// spelled inside an expression (`crate::format::rar4::create::f(..)`) is a
/// dependency exactly like the import form, and the `use`-only form of this
/// check silently missed several of them.
fn dependency_lines_under(dir: &Path) -> Vec<(String, String)> {
    let mut sources = Vec::new();
    rust_sources_below(dir, &mut sources);
    let mut out = Vec::new();
    for path in sources {
        let file = path
            .strip_prefix(dir)
            .expect("source below dir")
            .display()
            .to_string();
        // Test-only code: the external module form (`mod tests;` ->
        // `tests.rs`) and any `tests/` subdirectory.
        if file.ends_with("tests.rs") || file.contains("tests/") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read source");
        for line in text.lines() {
            let trimmed = line.trim_start();
            // The inline form (`mod tests {`), conventionally the file tail.
            if trimmed.starts_with("mod tests") && trimmed.ends_with('{') {
                break;
            }
            if trimmed.starts_with("//") {
                continue;
            }
            out.push((file.clone(), line.to_string()));
        }
    }
    out
}

/// Every `crate::<layer>` reference under `dir`, as
/// `(file, trimmed line, layer)`.
fn layer_references(dir: &Path, layers: &[&str]) -> Vec<(String, String, String)> {
    let mut offenders = Vec::new();
    for (file, line) in dependency_lines_under(dir) {
        for layer in layers {
            if line.contains(&format!("crate::{layer}")) {
                offenders.push((file.clone(), line.trim().to_string(), (*layer).to_string()));
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
    let facades = ["reader.rs", "writer.rs", "editor.rs"];
    let offenders = layer_references(
        &manifest_dir.join("src/archive"),
        &["format", "codec", "crypto", "recovery"],
    )
    .into_iter()
    .filter(|(file, _, _)| facades.contains(&file.as_str()))
    .collect::<Vec<_>>();
    assert!(
        offenders.is_empty(),
        "role facades must not reference format/codec/crypto/recovery: {offenders:?}"
    );
}

/// Filesystem and model policy are leaf layers: they must not reach archive,
/// format, codec, crypto or recovery — nothing above them.
#[test]
fn fs_and_model_policy_do_not_depend_upward() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for leaf in ["src/fs", "src/model"] {
        let offenders = layer_references(
            &manifest_dir.join(leaf),
            &["archive", "format", "codec", "crypto", "recovery"],
        );
        assert!(
            offenders.is_empty(),
            "{leaf} must not depend upward: {offenders:?}"
        );
    }
}
