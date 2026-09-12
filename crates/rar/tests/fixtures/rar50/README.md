# rar50 fixtures

- `winrar5_multiple_files.rar` — RAR5 archive created by WinRAR, vendored
  from the libarchive test suite
  (`test_read_format_rar5_multiple_files.rar`, BSD-2-Clause licensed,
  <https://github.com/libarchive/libarchive>). Used by the interop suite to
  prove we read genuine WinRAR output byte-identically.
- `tail-match-362.bin` — LZ tail-match regression input (362 bytes of
  structured data that once tripped a panic in the match finder).
- `winrar5_ntfs_stream.rar` / `winrar5_ntfs_stream_p.rar` — genuine WinRAR
  7.23 `rar a -os` output (plain and `-ppw`, password `pw`) of a single
  `data.txt` carrying a `:meta` alternate data stream; used by the archive
  unit tests to pin the "STM" record layout (plaintext CRC32, per-stream
  ENCR record) and the read-time password check.