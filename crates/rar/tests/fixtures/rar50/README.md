# rar50 fixtures

- `winrar5_multiple_files.rar` — RAR5 archive created by WinRAR, vendored
  from the libarchive test suite
  (`test_read_format_rar5_multiple_files.rar`, BSD-2-Clause licensed,
  <https://github.com/libarchive/libarchive>). Used by the interop suite to
  prove we read genuine WinRAR output byte-identically.
- `tail-match-362.bin` — LZ tail-match regression input (362 bytes of
  structured data that once tripped a panic in the match finder).