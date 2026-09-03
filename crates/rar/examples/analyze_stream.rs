//! Dissect the symbol stream of a RAR5 archive member (WinRAR or ours).
//!
//! Usage: cargo run --release --features parallel --example analyze_stream -- <file.rar>...
//!
//! Prints, per member: container info (method, dict, sizes), then a per-block
//! table (block bytes, table symbols, literal/match/repeat/filter counts) and
//! global length/distance histograms. The same input compressed by WinRAR and
//! by rar-rs can be compared to locate parse-level differences.
use std::io::{Read, Seek, SeekFrom};

fn main() {
    for path in std::env::args().skip(1) {
        analyze(&path);
    }
}

fn analyze(path: &str) {
    let rar = match rar5::RarArchive::open(path) {
        Ok(r) => r,
        Err(e) => {
            println!("{path}: cannot open: {e}");
            return;
        }
    };
    let entries = rar.list().to_vec();
    let mut f = std::fs::File::open(path).unwrap();
    println!("== {path}: {} member(s)", entries.len());
    for e in &entries {
        let h = &e.header;
        println!(
            "  [{}] method={} dict=2^{} solid={} packed={} unpacked={}",
            h.name, h.comp_method, h.comp_dict_size, h.comp_solid, h.packed_size, h.unpacked_size
        );
        if h.packed_size == 0 || h.unpacked_size == 0 || h.comp_solid {
            println!("    (skipped: no data / solid chain)");
            continue;
        }
        f.seek(SeekFrom::Start(h.data_offset)).unwrap();
        let mut buf = vec![0u8; h.packed_size as usize];
        f.read_exact(&mut buf).unwrap();
        let variant = rar5::ArchiveVersion::from_v70(h.comp_version == 1); // RAR7 v70
        match rar5::codec::lzss_huff::analyze_stream(
            &buf,
            h.unpacked_size,
            h.comp_dict_size,
            variant,
        ) {
            Ok(a) => print_analysis(&a, &buf),
            Err(err) => println!("    analyze: {err}"),
        }
    }
    let _ = rar; // keep the archive handle alive (entry list borrows it)
}

fn print_analysis(a: &rar5::codec::lzss_huff::StreamAnalysis, packed: &[u8]) {
    let n = a.blocks.len();
    println!(
        "    unpacked={} packed={} blocks={}",
        a.unpacked,
        packed.len(),
        n
    );
    // Aggregate table symbol counts.
    let mut nc = Vec::new();
    let mut dc = Vec::new();
    let mut ldc = Vec::new();
    let mut rc = Vec::new();
    let mut tabled = 0usize;
    let mut lit = 0u64;
    let mut mat = 0u64;
    let mut cmat = 0u64;
    let mut rep = 0u64;
    for b in &a.blocks {
        lit += b.literals;
        mat += b.matches;
        cmat += b.cache_matches;
        rep += b.repeats;
        if b.table_present {
            tabled += 1;
            nc.push(b.nc);
            dc.push(b.dc);
            ldc.push(b.ldc);
            rc.push(b.rc);
        }
    }
    let fmt = |v: &[usize]| -> String {
        if v.is_empty() {
            "n/a".into()
        } else {
            let avg: f64 = v.iter().sum::<usize>() as f64 / v.len() as f64;
            format!(
                "min={} max={} avg={:.1}",
                v.iter().min().unwrap(),
                v.iter().max().unwrap(),
                avg
            )
        }
    };
    println!(
        "    symbols: lit={} match={} cache={} repeat={} filter={}; tabled_blocks={}/{}",
        lit,
        mat,
        cmat,
        rep,
        a.blocks.iter().map(|b| b.filters).sum::<u64>(),
        tabled,
        n
    );
    println!(
        "    tables NC {} | DC {} | LDC {} | RC {}",
        fmt(&nc),
        fmt(&dc),
        fmt(&ldc),
        fmt(&rc)
    );
    println!(
        "    len_hist  [<2,2,3,4-15,16-63,64-255,256-1023,1024+]: {:?}",
        a.len_hist
    );
    println!(
        "    dist_hist [<4K,4K-64K,64K-1M,1M-4M,4M+]:     {:?}",
        a.dist_hist
    );
    println!(
        "    short_dist len2 [<16,<256,<4K,<64K,64K+]: {:?} | len3 {:?}",
        a.short_dist[0], a.short_dist[1]
    );
    // Block-size distribution summary.
    let mut sizes: Vec<u32> = a.blocks.iter().map(|b| b.block_size).collect();
    sizes.sort_unstable();
    let (mi, mx) = (*sizes.first().unwrap(), *sizes.last().unwrap());
    let avg: f64 = sizes.iter().map(|&s| s as f64).sum::<f64>() / sizes.len() as f64;
    let med = sizes[sizes.len() / 2];
    println!(
        "    block_bytes: min={} med={} avg={:.0} max={}",
        mi, med, avg, mx
    );
    // Literal share per block (the parse's shape).
    let lit_share: Vec<u64> = a
        .blocks
        .iter()
        .map(|b| (b.literals * 100).checked_div(b.out_bytes).unwrap_or(100))
        .collect();
    let p50 = lit_share.iter().filter(|&&v| v >= 50).count();
    let p85 = lit_share.iter().filter(|&&v| v >= 85).count();
    let p95 = lit_share.iter().filter(|&&v| v >= 95).count();
    println!(
        "    blocks by literal share: >=50%: {}  >=85%: {}  >=95%: {} (of {})",
        p50, p85, p95, n
    );
}
