/// RAR4 archive format support (read-only).
///
/// Clean-room implementation based on libarchive's
/// archive_read_support_format_rar.c (BSD-2-Clause).
pub mod constants;
pub mod decoder;
pub mod headers;
