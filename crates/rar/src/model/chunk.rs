/// Describes a contiguous slice of packed file data within one volume.
///
/// Multi-volume archives split a file's packed data across multiple volumes.
#[derive(Clone, Debug)]
pub struct DataChunk {
    pub volume_index: usize,
    pub data_offset: u64,
    pub packed_size: u64,
    pub crc32_val: Option<u32>,
    pub is_final: bool,
    pub extra_data: Vec<u8>,
}
