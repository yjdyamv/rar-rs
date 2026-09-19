//! Layering invariants, checked on the shipped source lines.
//!
//! The dependency direction is `archive` → `format` → `engine` →
//! `{codec, crypto, fs, model, options}` (see `docs/ARCHITECTURE.md`), with
//! `detect`/`version`/`vint` as root vocabulary. `archive` owns the
//! method-shaped seam the facades call (`archive/ops.rs`); `format`'s family
//! code is free functions over `&mut dyn Engine`.
//!
//! These checks are deliberately line-based and comment/test exempt: a
//! reference spelled inside an expression is a dependency like an import, and
//! an `impl RarArchive` block in `format` once hid the inversion exactly
//! because it was not a `use` statement.

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

/// Lines of each `.rs` file, skipping two things that are not dependencies:
///
/// - comment lines (a doc-comment mention of `crate::format` is prose, not a
///   dependency), and
/// - the body of an inline `mod tests { ... }` block, plus every `tests.rs`
///   file and `tests/` subdirectory. Test code may cross layers freely; these
///   invariants describe the shipped code.
fn dependency_lines(sources: &[PathBuf], base: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for path in sources {
        let file = path
            .strip_prefix(base)
            .unwrap_or(path)
            .display()
            .to_string()
            .replace('\\', "/");
        // Test-only code: the external module form (`mod tests;` ->
        // `tests.rs`) and any `tests/` subdirectory.
        if file.ends_with("tests.rs") || file.contains("tests/") {
            continue;
        }
        let text = std::fs::read_to_string(path).expect("read source");
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

/// Every `crate::<layer>` reference in `sources`, as
/// `(file, trimmed line, layer)`.
fn layer_references_in(
    sources: &[PathBuf],
    base: &Path,
    layers: &[&str],
) -> Vec<(String, String, String)> {
    let mut offenders = Vec::new();
    for (file, line) in dependency_lines(sources, base) {
        for layer in layers {
            if line.contains(&format!("crate::{layer}")) {
                offenders.push((file.clone(), line.trim().to_string(), (*layer).to_string()));
            }
        }
    }
    offenders
}

/// [`layer_references_in`] for a whole directory, with paths relative to it.
fn layer_references(dir: &Path, layers: &[&str]) -> Vec<(String, String, String)> {
    let mut sources = Vec::new();
    rust_sources_below(dir, &mut sources);
    layer_references_in(&sources, dir, layers)
}

/// The identifiers a `pub use <module>::{ ... };` block re-exports at the
/// crate root. Used to catch a dependency spelled through a re-export
/// (`crate::DictionarySize`) rather than the module path
/// (`crate::archive::…`) — the exact hole that let `format` keep naming
/// `archive` after the `impl RarArchive` blocks were removed.
fn crate_root_reexports(lib_rs: &Path, module: &str) -> Vec<String> {
    let text = std::fs::read_to_string(lib_rs).expect("read lib.rs");
    let needle = format!("pub use {module}::{{");
    let Some(start) = text.find(&needle) else {
        return Vec::new();
    };
    let rest = &text[start + needle.len()..];
    let end = rest.find("};").expect("terminated re-export block");
    rest[..end]
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|token| {
            !token.is_empty()
                && token
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        })
        .map(str::to_string)
        .collect()
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

/// The per-family container code sits *below* the archive engine, so no
/// `format` source may name `archive` — neither through the module path nor
/// through a crate-root re-export of an `archive` item. `RarArchive` used to
/// be reached through `impl RarArchive` blocks living in `format`; those are
/// gone, and this pins that they cannot come back.
#[test]
fn format_does_not_depend_on_the_archive_engine() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut layers = vec!["archive".to_string()];
    layers.extend(crate_root_reexports(
        &manifest_dir.join("src/lib.rs"),
        "archive",
    ));
    let layers = layers.iter().map(String::as_str).collect::<Vec<_>>();
    let offenders = layer_references(&manifest_dir.join("src/format"), &layers);
    assert!(
        offenders.is_empty(),
        "format must not name archive (module path or re-exported item); the family code takes `&mut dyn Engine`: {offenders:?}"
    );
}

/// Everything `format` is allowed to sit on must not reference it back:
/// `engine` (the `Engine` seam), the codec/crypto/fs/model leaves, the
/// option layer, and the root vocabulary (`detect`/`version`/`vint`).
/// Together with `format_does_not_depend_on_the_archive_engine` this makes
/// the documented order a one-way acyclic chain up to `archive`.
#[test]
fn layers_below_format_do_not_depend_on_it() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    for dir in [
        "src/engine",
        "src/codec",
        "src/crypto",
        "src/fs",
        "src/model",
    ] {
        rust_sources_below(&manifest_dir.join(dir), &mut sources);
    }
    for file in [
        "src/options.rs",
        "src/detect.rs",
        "src/version.rs",
        "src/vint.rs",
        "src/parallel.rs",
        "src/io_util.rs",
        "src/write_progress.rs",
        "src/crc32.rs",
    ] {
        sources.push(manifest_dir.join(file));
    }
    let offenders = layer_references_in(&sources, manifest_dir, &["format"]);
    assert!(
        offenders.is_empty(),
        "layers below format must not depend on it: {offenders:?}"
    );
}
