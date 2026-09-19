//! Cross-layer dependency matrix snapshot.
//!
//! `architecture_boundaries.rs` pins the layering *rules* we have reasoned
//! about (role facades stay off `format`, `format` stays off `archive`, the
//! layers below `format` do not depend on it, …). This test pins the whole
//! *edge set*: every `crate::<layer>` reference between two top-level modules,
//! rendered as `from -> to` lines and compared with the checked-in snapshot
//! `docs/dependency-matrix.txt`.
//!
//! Why an edge set and not counts: counts churn on every legitimate call, and
//! a snapshot that churns gets blindly regenerated. The set only changes when
//! a *new* layer-to-layer dependency appears (or one disappears) — exactly the
//! event that deserves a review. When that happens, either fix the coupling or
//! update the snapshot in the same commit:
//!
//! ```sh
//! UPDATE_DEPENDENCY_MATRIX=1 cargo test -p rar-rs --test dependency_matrix
//! ```
//!
//! The scan mirrors `architecture_boundaries`: line-based (a reference inside
//! an expression is a dependency like an import), comments and test modules
//! exempt, and the layer list read from `lib.rs` rather than hard-coded so a
//! new module cannot slip in as an unlisted layer.

use std::collections::BTreeSet;
use std::fmt::Write as _;
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

/// The crate's top-level modules, read from `lib.rs` (`pub(crate) mod fs;`,
/// `pub mod codec;`, `mod engine;`, …) so the list cannot drift from the tree.
fn declared_layers(lib_rs: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(lib_rs).expect("read lib.rs");
    let mut layers = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((_, rest)) = line.split_once("mod ") else {
            continue;
        };
        let Some(name) = rest.strip_suffix(';') else {
            continue;
        };
        if !line.starts_with("mod ")
            && !line.starts_with("pub mod ")
            && !line.starts_with("pub(crate) mod ")
        {
            continue;
        }
        let name = name.trim();
        if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            layers.push(name.to_string());
        }
    }
    layers.sort();
    layers.dedup();
    layers
}

/// `(from, to)` pairs for every `crate::<layer>` reference in the shipped
/// source lines.
fn edges(src_dir: &Path, layers: &[String]) -> BTreeSet<(String, String)> {
    let mut sources = Vec::new();
    rust_sources_below(src_dir, &mut sources);
    let mut found = BTreeSet::new();
    for path in sources {
        let rel = path
            .strip_prefix(src_dir)
            .expect("source below src")
            .display()
            .to_string()
            .replace('\\', "/");
        // Test-only code: `tests.rs`, any `tests/` subdirectory, and the body
        // of an inline `mod tests { … }` block (conventionally the file tail).
        if rel.ends_with("tests.rs") || rel.contains("tests/") {
            continue;
        }
        let from = match rel.split_once('/') {
            // A directory module: the layer is its name.
            Some((first, _)) => first.to_string(),
            // A top-level file (`wire.rs`, `detect.rs`, `lib.rs`, …): the layer
            // is its stem, so the root vocabulary each owns stays visible
            // instead of collapsing into one `(root)` row.
            None => rel.trim_end_matches(".rs").to_string(),
        };
        let text = std::fs::read_to_string(&path).expect("read source");
        for line in text.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("mod tests") && trimmed.ends_with('{') {
                break;
            }
            if trimmed.starts_with("//") {
                continue;
            }
            for to in layers {
                if from != *to && line.contains(&format!("crate::{to}")) {
                    found.insert((from.clone(), to.clone()));
                }
            }
        }
    }
    found
}

fn render(src_dir: &Path, layers: &[String]) -> String {
    let edges = edges(src_dir, layers);
    let mut out = String::new();
    out.push_str(
        "# Cross-layer references in crates/rar/src (one `from -> to` edge per\n\
         # line; comments and test modules excluded; `(none)` = no outbound edge).\n\
         # Checked by `tests/dependency_matrix.rs`; regenerate with\n\
         # `UPDATE_DEPENDENCY_MATRIX=1 cargo test -p rar-rs --test dependency_matrix`.\n\
         #\n\
         # Rows come from lib.rs's `mod …;` declarations (directory modules by\n\
         # name, top-level files by stem; `lib.rs` is the `(crate root)` row). The\n\
         # documented order is `archive -> format -> engine -> {codec, crypto, fs,\n\
         # model, options}` with `detect`/`version`/`vint`/`time`/`error` as\n\
         # leaves; `recovery` sits above `format`. An edge that looks like a\n\
         # violation of that order is a review event, not a snapshot update.\n\
         #\n\
         # layer              -> depends on\n",
    );
    for layer in layers {
        let targets: Vec<String> = edges
            .iter()
            .filter(|(from, _)| from == layer)
            .map(|(_, to)| to.clone())
            .collect();
        if targets.is_empty() {
            let _ = writeln!(out, "{layer:<18} -> (none)");
        } else {
            let _ = writeln!(out, "{layer:<18} -> {}", targets.join(", "));
        }
    }
    let lib_targets: Vec<String> = edges
        .iter()
        .filter(|(from, _)| from == "lib")
        .map(|(_, to)| to.clone())
        .collect();
    if !lib_targets.is_empty() {
        let _ = writeln!(out, "{:<18} -> {}", "(crate root)", lib_targets.join(", "));
    }
    out
}

#[test]
fn cross_layer_edges_match_the_checked_in_snapshot() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src_dir = manifest_dir.join("src");
    let snapshot_path = manifest_dir.join("../../docs/dependency-matrix.txt");
    let layers = declared_layers(&src_dir.join("lib.rs"));
    assert!(
        layers.len() > 15,
        "the lib.rs layer scan found only {} modules: {:?}",
        layers.len(),
        layers
    );
    let actual = render(&src_dir, &layers);

    if std::env::var_os("UPDATE_DEPENDENCY_MATRIX").is_some() {
        std::fs::write(&snapshot_path, &actual).expect("write docs/dependency-matrix.txt");
        return;
    }

    let expected = std::fs::read_to_string(&snapshot_path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    if actual == expected {
        return;
    }
    let actual_lines: Vec<&str> = actual.lines().collect();
    let expected_lines: Vec<&str> = expected.lines().collect();
    let mut diff = String::new();
    for line in &actual_lines {
        if !expected_lines.contains(line) {
            let _ = writeln!(diff, "+ {line}");
        }
    }
    for line in &expected_lines {
        if !actual_lines.contains(line) {
            let _ = writeln!(diff, "- {line}");
        }
    }
    panic!(
        "the cross-layer edge set changed:\n{diff}\n\
         If the new edge is a real coupling, fix it or record why it is wanted in \
         `docs/ARCHITECTURE.md`. If it is only a moved/renamed reference, refresh the \
         snapshot with `UPDATE_DEPENDENCY_MATRIX=1 cargo test -p rar-rs --test dependency_matrix` \
         and review the diff."
    );
}
