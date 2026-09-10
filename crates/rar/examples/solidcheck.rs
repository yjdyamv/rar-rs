//! Temporary diagnostic: does a solid chain actually share the window
//! between members? Writes N members that each start with the same
//! incompressible block, both non-solid and solid, and prints per-member
//! packed sizes and solid flags.

use std::path::Path;

use rar_rs::{ArchiveReader, ArchiveWriter, EntryWriteOptions, SolidMode, WriterOptions};

fn lcg(len: usize, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    (0..len)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            (s >> 33) as u8
        })
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args().nth(1).expect("output directory");
    let dir = Path::new(&out);
    std::fs::create_dir_all(dir)?;

    let shared = lcg(1024 * 1024, 4242);
    let filler = b"the solid chain shares one sliding window across consecutive members. "
        .iter()
        .copied()
        .cycle()
        .take(4 * 1024 * 1024)
        .collect::<Vec<u8>>();

    let paths: Vec<_> = (0..4)
        .map(|i| {
            let mut data = shared.clone();
            data.extend_from_slice(&filler);
            data.extend_from_slice(&shared);
            let p = dir.join(format!("m{i}.bin"));
            std::fs::write(&p, &data)?;
            Ok::<_, std::io::Error>(p)
        })
        .collect::<Result<_, _>>()?;

    for mode in [
        ("non-solid", SolidMode::Disabled),
        ("solid", SolidMode::Continuous),
    ] {
        let path = dir.join(format!("{}.rar", mode.0));
        let mut writer =
            ArchiveWriter::create_with(&path, WriterOptions::new().solid_mode(mode.1))?;
        for p in &paths {
            writer.add_path(p, EntryWriteOptions::new())?;
        }
        writer.finish()?;
        println!(
            "== {:>10}: {:>10} bytes total",
            mode.0,
            std::fs::metadata(&path)?.len()
        );
        let reader = ArchiveReader::open(&path)?;
        for entry in reader.entries() {
            println!(
                "   {} packed={} solid={} method={}",
                entry.name(),
                entry.compressed_size(),
                entry.comp_solid(),
                entry.method_name()
            );
        }
    }
    Ok(())
}
