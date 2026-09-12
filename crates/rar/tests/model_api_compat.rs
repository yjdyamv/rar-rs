//! Wire model API: the promoted `wire` surface exposes the archive model
//! structs together with their parsing/serialization helpers (the former
//! `rar40`/`rar50` alias paths were retired with the `raw` feature, ADR 0007).

use rar_rs::wire::{DataChunk, FileHeader, RawBlock};

#[test]
fn wire_model_structs_expose_their_serialization_helpers() {
    let header = FileHeader::default();
    assert!(!header.to_bytes().is_empty());

    let _: fn(&RawBlock, u64) -> rar_rs::RarResult<FileHeader> = FileHeader::from_raw;

    let chunk = DataChunk {
        volume_index: 2,
        data_offset: 17,
        packed_size: 23,
        crc32_val: Some(42),
        is_final: true,
        extra_data: vec![1, 2, 3],
    };
    assert_eq!(chunk.volume_index, 2);
    assert_eq!(chunk.data_offset, 17);
    assert_eq!(chunk.packed_size, 23);
}
