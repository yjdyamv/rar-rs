#![allow(dead_code)] // every test binary links the whole module; each uses a subset
//! Shared interop-test helpers: the vendored fixture manifest plus raw
//! RAR5 block scanners used by the byte-level assertions.

use std::io::Write;
use std::path::Path;

pub const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/rar50/winrar5_multiple_files.rar"
);
pub const FIXTURE_FILES: [(&str, &str); 4] = [
    (
        "test1.bin",
        "7d89f86f9f69d744ffff3fc043e15bf89fc3ffc134ffcbb31d164a99bb8b67b0",
    ),
    (
        "test2.bin",
        "f81e6fceeeab366306b23466bf6bb3aac2875e0906dc20a8652be0696ceb15a2",
    ),
    (
        "test3.bin",
        "5e621f2b6ce8fed758c3df8221f994eda55d1e432c7cc4349c34a30ec2e1c43d",
    ),
    (
        "test4.bin",
        "2627f40180217252956edb9a426e8d3e344adaf89019d3bccbe04f6c3416dcdd",
    ),
];

pub fn sha256(data: &[u8]) -> String {
    // sha2 is a dependency of the library; expose it via the already-linked
    // crates. digest 0.11's output no longer formats as hex, so encode it
    // manually.
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn make_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

pub fn write_repeated(path: &Path, byte: u8, len: usize) {
    let mut f = std::fs::File::create(path).expect("create file");
    let chunk = vec![byte; 1 << 20];
    let mut left = len;
    while left > 0 {
        let n = left.min(chunk.len());
        f.write_all(&chunk[..n]).expect("write file");
        left -= n;
    }
}

// ── RAR5 block-scanning helpers (test-only) ────────────────────────────────

pub fn read_vint(data: &[u8], mut off: usize) -> (u64, usize) {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let b = data[off];
        off += 1;
        value |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (value, off)
}

pub struct BlockInfo {
    pub(crate) start: usize,
    pub(crate) header_len: usize,
    pub(crate) block_type: u64,
    #[allow(dead_code)]
    pub(crate) flags: u64,
    #[allow(dead_code)]
    pub(crate) extra_size: u64,
    #[allow(dead_code)]
    pub(crate) data_size: u64,
    pub(crate) body: Vec<u8>,
}

pub fn scan_blocks(data: &[u8]) -> Vec<BlockInfo> {
    // Cross the library's block-envelope seam (the same reader the archive
    // scanner uses) instead of re-implementing the envelope format.
    let mut blocks = Vec::new();
    let mut cursor = std::io::Cursor::new(data);
    cursor.set_position(8); // skip the rar signature
    while let Ok(Some(meta)) = rar_rs::rar50::headers::read_block(&mut cursor, None) {
        blocks.push(BlockInfo {
            start: meta.block_start as usize,
            header_len: (meta.data_offset - meta.block_start - 4) as usize,
            block_type: meta.block_type,
            flags: meta.flags,
            extra_size: 0,
            data_size: meta.raw.data_size,
            body: meta.raw.header_data,
        });
        let last = meta.block_type == 0x05;
        cursor.set_position(meta.data_end);
        if last {
            break;
        }
    }
    blocks
}

pub fn service_name(body: &[u8]) -> Option<String> {
    let (_, mut q) = read_vint(body, 0);
    let (flags, n) = read_vint(body, q);
    q = n;
    if flags & 0x0001 != 0 {
        let (_, n) = read_vint(body, q);
        q = n;
    }
    if flags & 0x0002 != 0 {
        let (_, n) = read_vint(body, q);
        q = n;
    }
    // file flags, unpacked size, attributes, compression info, host OS
    for _ in 0..5 {
        let (_, n) = read_vint(body, q);
        q = n;
    }
    let (name_len, n) = read_vint(body, q);
    q = n;
    if q + name_len as usize > body.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&body[q..q + name_len as usize]).into_owned())
}

pub fn first_file_data_offset(data: &[u8]) -> usize {
    for block in scan_blocks(data) {
        if block.block_type == 0x02 {
            return block.start + block.header_len;
        }
    }
    panic!("no file block found");
}

pub fn main_header_locator(data: &[u8]) -> (u64, Option<u64>, Option<u64>) {
    for block in scan_blocks(data) {
        if block.block_type == 0x01 {
            let (_, mut q) = read_vint(&block.body, 0);
            let (flags, n) = read_vint(&block.body, q);
            q = n;
            let mut extra_size = 0u64;
            if flags & 0x0001 != 0 {
                let (v, n) = read_vint(&block.body, q);
                extra_size = v;
                q = n;
            }
            if flags & 0x0002 != 0 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            // archive flags vint, then extra area
            let (_, n) = read_vint(&block.body, q);
            q = n;
            let extra = &block.body[q..q + extra_size as usize];
            let (_, mut e) = read_vint(extra, 0);
            let (rec_type, n) = read_vint(extra, e);
            e = n;
            assert_eq!(rec_type, 0x01, "locator record");
            let (loc_flags, n) = read_vint(extra, e);
            e = n;
            let qo = if loc_flags & 0x0001 != 0 {
                let (v, n) = read_vint(extra, e);
                e = n;
                Some(v)
            } else {
                None
            };
            let rr = if loc_flags & 0x0002 != 0 {
                let (v, _) = read_vint(extra, e);
                Some(v)
            } else {
                None
            };
            return (loc_flags, qo, rr);
        }
    }
    panic!("no main header found");
}

pub fn service_offset(data: &[u8], name: &str) -> usize {
    for block in scan_blocks(data) {
        if block.block_type == 0x03 && service_name(&block.body).as_deref() == Some(name) {
            return block.start;
        }
    }
    panic!("service {name} not found");
}

/// Parse the member name out of a file header body (block type 2).
pub fn file_header_name(body: &[u8]) -> String {
    let (_, mut q) = read_vint(body, 0);
    let (flags, n) = read_vint(body, q);
    q = n;
    if flags & 0x0001 != 0 {
        let (_, n) = read_vint(body, q);
        q = n;
    }
    if flags & 0x0002 != 0 {
        let (_, n) = read_vint(body, q);
        q = n;
    }
    let (file_flags, n) = read_vint(body, q);
    q = n;
    let (_, n) = read_vint(body, q); // unpacked size
    q = n;
    let (_, n) = read_vint(body, q); // attributes
    q = n;
    if file_flags & 0x0002 != 0 {
        q += 4; // mtime
    }
    if file_flags & 0x0004 != 0 {
        q += 4; // CRC32
    }
    let (_, n) = read_vint(body, q); // compression info
    q = n;
    let (_, n) = read_vint(body, q); // host OS
    q = n;
    let (name_len, n) = read_vint(body, q);
    q = n;
    String::from_utf8_lossy(&body[q..q + name_len as usize]).into_owned()
}

/// Absolute offset where the data area of the given member starts.
pub fn file_data_offset(data: &[u8], name: &str) -> usize {
    for block in scan_blocks(data) {
        if block.block_type == 0x02 && file_header_name(&block.body) == name {
            return block.start + block.header_len + 4;
        }
    }
    panic!("file block {name} not found");
}

/// Byte span `[start, end)` of the archive block (header + data) holding
/// the given member name.
pub fn file_block_span(data: &[u8], name: &str) -> (usize, usize) {
    // scan_blocks' header_len covers the size vint + body but not the
    // 4-byte header CRC32.
    for block in scan_blocks(data) {
        if block.block_type == 0x02 && file_header_name(&block.body) == name {
            return (
                block.start,
                block.start + block.header_len + 4 + block.data_size as usize,
            );
        }
    }
    panic!("file block {name} not found");
}

/// All members in archive order (skipping the main header).
pub fn member_names(data: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for block in scan_blocks(data) {
        if block.block_type == 0x02 {
            names.push(file_header_name(&block.body));
        }
    }
    names
}

/// Names cached inside a quick-open record payload.
pub fn qo_cached_names(data: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for block in scan_blocks(data) {
        if block.block_type == 0x03 && service_name(&block.body).as_deref() == Some("QO") {
            // scan_blocks' header_len excludes the 4-byte header CRC32.
            let data_start = block.start + block.header_len + 4;
            let payload = &data[data_start..data_start + block.data_size as usize];
            let mut p = 0;
            while p < payload.len() {
                p += 4; // entry CRC
                let (body_len, n) = read_vint(payload, p);
                p = n;
                let entry = &payload[p..p + body_len as usize];
                p += body_len as usize;
                let (_, mut q) = read_vint(entry, 0); // entry flags
                let (_, n) = read_vint(entry, q); // relative offset
                q = n;
                let (hdr_len, n) = read_vint(entry, q);
                q = n;
                let hdr = &entry[q..q + hdr_len as usize];
                // The cached header is a full block: CRC32(4) + size vint
                // + body.
                let (hsize, n) = read_vint(hdr, 4);
                let body = &hdr[n..n + hsize as usize];
                names.push(file_header_name(body));
            }
        }
    }
    names
}

pub fn service_exists(data: &[u8], name: &str) -> bool {
    scan_blocks(data)
        .iter()
        .any(|b| b.block_type == 0x03 && service_name(&b.body).as_deref() == Some(name))
}

/// Main header archive-level flags (parse the body manually so tests work
/// with and without a locator record).
pub fn archive_flags(data: &[u8]) -> u64 {
    for block in scan_blocks(data) {
        if block.block_type == 0x01 {
            let (_, mut q) = read_vint(&block.body, 0);
            let (flags, n) = read_vint(&block.body, q);
            q = n;
            if flags & 0x0001 != 0 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            if flags & 0x0002 != 0 {
                let (_, n) = read_vint(&block.body, q);
                q = n;
            }
            let (arch_flags, _) = read_vint(&block.body, q);
            return arch_flags;
        }
    }
    panic!("no main header");
}

pub fn compressible(seed: u8, n: usize) -> Vec<u8> {
    let pat: Vec<u8> = (0..64u8)
        .map(|i| i.wrapping_mul(7).wrapping_add(seed))
        .collect();
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        out.extend_from_slice(&pat);
    }
    out.truncate(n);
    out
}

pub fn with_stub(data: &[u8], stub_len: usize) -> Vec<u8> {
    let mut stub = vec![0u8; stub_len];
    for (i, b) in stub.iter_mut().enumerate() {
        *b = (i.wrapping_mul(31).wrapping_add(7) & 0xFF) as u8;
    }
    stub.extend_from_slice(data);
    stub
}
