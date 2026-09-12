//! Low-level wire toolkit.
//!
//! Promoted from the former `raw` feature (ADR 0007): the RAR5 block
//! envelope reader, varints, the archive model structs, the recovery /
//! parity builders and the encryption primitives. The in-tree integration
//! tests and the fuzz workspace build on these, so they are part of the
//! supported surface.
//!
//! They are still low-level: nothing here is needed by the
//! [`ArchiveReader`](crate::ArchiveReader) /
//! [`ArchiveWriter`](crate::ArchiveWriter) /
//! [`ArchiveEditor`](crate::ArchiveEditor) facades. Prefer the facades;
//! reach for `wire` when inspecting or synthesising raw block streams.

pub use crate::crypto::{EncryptionParams, decrypt_data, derive_keys, encrypt_data};
pub use crate::format::rar5::headers::{BlockMeta, DataChunk, FileHeader, RawBlock, read_block};
pub use crate::format::rar5::vint;
pub use crate::recovery::rar50::{
    build_structural_inline_recovery_data, crc64_rar_state, crc64_xz,
};
pub use crate::recovery::rev50::build_recovery_volume_file;
