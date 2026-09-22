//! Packed-data chunk model: one member's data slice within one volume.

/// Describes a contiguous slice of packed file data within one volume.
///
/// Multi-volume archives split a file's packed data across multiple volumes.
#[derive(Clone, Debug)]
pub struct DataChunk {
    /// Zero-based index of the volume holding this slice.
    pub volume_index: usize,
    /// Offset of the slice within that volume's file.
    pub data_offset: u64,
    /// Stored (packed) size of the slice in bytes.
    pub packed_size: u64,
    /// The fragment's stored checksum: RAR5 non-final heads and RAR 1.5–4.x
    /// split heads carry a per-fragment CRC, the final head the member's
    /// whole-data checksum. `None` for RAR 1.3/1.4 (16-bit rolling
    /// checksums are not shown per fragment).
    pub crc32_val: Option<u32>,
    /// Whether this slice is the member's last (whose checksum covers the
    /// whole member, not just the fragment).
    pub is_final: bool,
    /// Raw extra-area bytes attached to this fragment's header, undecoded.
    pub extra_data: Vec<u8>,
}
