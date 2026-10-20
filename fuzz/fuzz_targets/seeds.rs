//! Write the generated seed corpus to `fuzz/corpus/<target>/` so a libFuzzer
//! run starts from the same shapes the standalone loop uses.
//!
//! The seeds are built by our own writer (no archives are checked in), so this
//! binary is the only way to obtain files for `cargo fuzz run`.

use std::path::Path;

fn write(target: &str, seeds: &[&[u8]]) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus").join(target);
    std::fs::create_dir_all(&dir).expect("create corpus dir");
    for (index, seed) in seeds.iter().enumerate() {
        let path = dir.join(format!("seed-{index:02}.bin"));
        std::fs::write(&path, seed).expect("write seed");
    }
    let total: usize = seeds.iter().map(|seed| seed.len()).sum();
    println!("{target}: {} seeds, {total} bytes -> {}", seeds.len(), dir.display());
}

fn main() {
    let all = rar_rs_fuzz::corpus_all();
    write("parse", &rar_rs_fuzz::corpus_parse());
    write("crypto", &all);
    write("recovery", &all);
    write("rev", &all);
    write("write", &all);
    write("rewrite", &all);

    let legacy: Vec<&[u8]> = rar_rs_fuzz::corpus_legacy()
        .iter()
        .map(Vec::as_slice)
        .collect();
    write("legacy", &legacy);
}
