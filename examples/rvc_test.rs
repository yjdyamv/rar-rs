use rar5::RarArchive;
fn main() {
    // 5 data volumes expected (random data, 32k volumes); ask for 10 .rev
    // files → must auto-cap at the data volume count.
    let mut ar = rar5::RarArchive::create_multivolume_with_recovery_count(
        "/tmp/opencode/revtest/cnt.part1.rar",
        32768,
        10,
    )
    .unwrap();
    ar.add_as("/tmp/opencode/revtest/src/a.bin", "a.bin", 0)
        .unwrap();
    ar.add_as("/tmp/opencode/revtest/src/b.txt", "b.txt", 0)
        .unwrap();
    ar.close().unwrap();
    println!("done");
}
