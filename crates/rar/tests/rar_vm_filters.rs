//! RAR 3.x/4.x VM filter decode: standard filters, generic RARVM bytecode
//! and PPMd-embedded filter records. Fixtures come from the `rars` fixture
//! corpus (`tests/fixtures/rar15_40/rarvm`, MIT OR Apache-2.0).

use rar_rs::ArchiveReader;

const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rar40/rarvm/");

fn fixture(name: &str) -> String {
    format!("{FIX}{name}")
}

/// Minimal CRC32 (IEEE) so the tests can pin the decoded bytes.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn read_member(path: &str, name: &str) -> Vec<u8> {
    let mut archive = ArchiveReader::open(path).unwrap();
    let id = archive.unique_entry(name).unwrap();
    archive
        .read_entry(id)
        .unwrap_or_else(|error| panic!("reading {name} from {path}: {error}"))
}

#[test]
fn solid_e8_filters_use_member_relative_offsets() {
    let expected_lead = std::fs::read(fixture("solid_e8_filter_lead.txt")).unwrap();
    let expected_exe = std::fs::read(fixture("solid_e8_filter_payload.exe")).unwrap();
    let archive = ArchiveReader::open(fixture("solid_e8_filter_member_offset.rar")).unwrap();

    let entries: Vec<_> = archive
        .entries()
        .map(|entry| {
            (
                entry.name().to_string(),
                entry.compressed_size(),
                entry.size(),
            )
        })
        .collect();
    assert_eq!(
        entries,
        vec![
            ("lead.txt".to_string(), 76, 800),
            ("tiny_e8e9.exe".to_string(), 295, 5_884),
        ]
    );
    drop(archive);

    assert_eq!(
        read_member(&fixture("solid_e8_filter_member_offset.rar"), "lead.txt"),
        expected_lead
    );
    assert_eq!(
        read_member(
            &fixture("solid_e8_filter_member_offset.rar"),
            "tiny_e8e9.exe"
        ),
        expected_exe
    );
}

#[test]
fn vm_filter_control_stream_accepts_32_bit_encoded_integers() {
    let data = read_member(&fixture("vm_encoded_u32_filter.rar"), "bsdcat.exe");
    assert!(!data.is_empty());
    assert_eq!(crc32(&data), 0x4db1_0349);
}

#[test]
fn non_standard_vm_filter_uses_generic_executor() {
    let data = read_member(
        &fixture("generic_delta_padding_mutation.rar"),
        "itanium_synthetic_bundles.bin",
    );
    assert_eq!(data.len(), 1_048_576);
    assert_eq!(crc32(&data), 0x3908_6451);
}

#[test]
fn ppmd_embedded_vm_filter_is_applied() {
    // Generated with RAR 3.00 using `-m5 -mc10:16t+ -mce+`: the filter
    // record rides PPMd escape 3 instead of LZ symbols.
    let data = read_member(
        &fixture("ppmd_embedded_vm_filter.rar"),
        "ppmd_branch_mix.bin",
    );
    assert_eq!(data.len(), 710_400);
    assert_eq!(crc32(&data), 0xa0fa_ad59);
}

#[test]
fn real_executable_filter_archive_decodes() {
    let data = read_member(&fixture("filter_bsdcat_exe.rar"), "bsdcat.exe");
    assert_eq!(crc32(&data), 0x4db1_0349);
}

#[test]
fn vm_delta_filter_accepts_more_than_thirty_two_channels() {
    let mut expected = Vec::with_capacity(400 * 64);
    for row in 0..400u32 {
        for channel in 0..64u32 {
            expected.push((channel * 7 + row * 3) as u8);
        }
    }
    let data = read_member(&fixture("delta_64_channels.rar"), "delta64.bin");
    assert_eq!(data, expected);
}
