use rar_rs::rar50::headers::{
    DataChunk as HeadersDataChunk, FileHeader as HeadersFileHeader, RawBlock,
};
use rar_rs::rar50::{DataChunk as FacadeDataChunk, FileHeader as FacadeFileHeader};

#[test]
fn legacy_model_paths_resolve_to_the_same_types_and_methods() {
    let header_from_headers = HeadersFileHeader::default();
    let header_from_facade: FacadeFileHeader = header_from_headers;
    assert!(!header_from_facade.to_bytes().is_empty());

    let _: fn(&RawBlock, u64) -> rar_rs::RarResult<HeadersFileHeader> = HeadersFileHeader::from_raw;

    let chunk_from_headers = HeadersDataChunk {
        volume_index: 2,
        data_offset: 17,
        packed_size: 23,
        crc32_val: Some(42),
        is_final: true,
        extra_data: vec![1, 2, 3],
    };
    let chunk_from_facade: FacadeDataChunk = chunk_from_headers;
    assert_eq!(chunk_from_facade.volume_index, 2);
    assert_eq!(chunk_from_facade.data_offset, 17);
    assert_eq!(chunk_from_facade.packed_size, 23);
}
