# RAR4 recovery volumes (`.rev`) — implemented (2026-09)

Status: **resolved**. `rar rv` / `rar rc` (and `a … -rv[N]`) now read and
write the legacy RAR 1.5–4.x recovery volumes; the format below was
reverse-engineered from WinRAR 5.91/6.23/7.23 output and cross-validated
byte-for-byte in both directions.

## Format

A legacy `.rev` file is raw Reed-Solomon parity over the volume set: for
every byte offset, the bytes of all data volumes form one GF(2^8) RS
codeword (8-bit field, primitive polynomial `0x11d`), and each recovery
volume stores one parity symbol per offset. The codec lives in
`crates/rar/src/recovery/rev3/rs8.rs` (ported from the reference `rars`
implementation and verified against WinRAR's parity bytes).

Two on-disk layouts exist, and WinRAR picks by the archive's generation:

| layout | tail of the file | parity range | names |
| --- | --- | --- | --- |
| **trailer** (RAR 4.20+, volumes end in zero bytes) | 7 bytes: `[data_count-1, recovery_count-1, recovery_index, CRC32-LE(payload + first three)]` | `0 .. len-7`; a rebuilt volume's last 7 bytes are zero | `.partN.rar` sets: `{base}.partNN.rev`; `.rar`/`.rNN` sets: `{base}N.rev` |
| **legacy** (RAR 3.0-era volumes without zero tails) | full parity, no trailer | whole file | `{base}<data>_<rec>_<index+1>.rev` (new-naming sets keep the part infix: `{base}.part<data>_<rec>_<index>.rev`) |

WinRAR's 20-byte `ENDARC` (flags `0x400e`/`0x400f`, data = prefix CRC32 +
volume number + eight zero bytes) is what makes a volume end in zeros and
selects the trailer layout; our own RAR4 writer emits a 7-byte `ENDARC`
with a live tail, so `rv` writes the legacy full-parity layout for our
sets — exactly what WinRAR 7.23 writes for them too.

## Repair (`rc`)

- Accepts any existing data volume or `.rev` file of the set.
- Enumerates the volume slots from the metadata counts and rebuilds
  missing volumes (up to `recovery_count`); a missing last volume is
  truncated at its `ENDARC` block.
- Locates *damaged* volumes with the RS syndromes (Berlekamp-Massey, up
  to `floor(recovery_count / 2)` unknown damaged volumes), renames them to
  `*.bad` and writes the rebuilt volume in place, mirroring WinRAR.
- Trailer-format rebuilds zero the unprotected seven-byte tail, exactly
  like WinRAR.

## Validation

- `tests/rar4_rev3.rs`: build/rebuild round trips (first/middle/last
  volume missing, two recovery volumes, damaged volume, trailer layout)
  plus the RAR 3.00 and 4.20 fixtures from the `rars` corpus.
- `cli_behavior/recovery.rs`: CLI `rv`/`rc` round trip and `a -ma4 -v -rv`
  create-time generation.
- `winrar_interop/recovery.rs`: our `.rev` bytes are **identical** to
  WinRAR 7.23's for the same volumes (legacy and trailer layouts); WinRAR
  `rc` rebuilds from our files and our `rc` rebuilds from WinRAR's,
  byte-for-byte.
