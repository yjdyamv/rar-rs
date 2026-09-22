# Command-Line Reference

> Last verified: 2026-09-22 @ `55f8205`; switch coverage is checked against the
> clap surface, behavior against the tests and the official WinRAR 7.23 tools.

rar-rs ships two binaries, `rar` and `unrar`, modelled on the WinRAR 7.x console
tools. Every official command is implemented. Switches follow WinRAR 7.23
semantics; unsupported switches are either rejected or accepted as no-ops where
WinRAR does the same. This page is the usage reference.

---

## `rar` — create and modify archives

```
rar <command> [switches] archive[.rar] [files...]

For extract commands (`x`/`e`), every `files...` argument is a **member
selector**: a non-mask name must match the full stored path or name a
directory (selecting its subtree), while `*`/`?` masks also match the
basename anywhere in the tree. The destination directory is set with
`--dest <path>` (default `.`). A name matching nothing is a hard error,
never a silent dump into a `<name>/` folder.
```

### Commands

| Command     | Alias | Action                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| ----------- | ----- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `a`         |       | Add files (creates the archive if missing); `a -f`/`a -u` behave like the `f`/`u` commands, `-k` locks the result and `-z<file>` sets the comment                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `u`         |       | Update: add missing files, replace newer ones                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `f`         |       | Freshen: update existing members only                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `m`         |       | Move: add files, then erase the sources                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `mf`        |       | Move files only: the tree is archived like `m`, but directories are left on disk                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `d`         |       | Delete members without rebuilding the archive                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `rn`        |       | Rename archived members                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `ch`        |       | Change parameters (`-cl`/`-cu` name case conversion)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `k`         |       | Lock the archive (read-only)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `rr`        |       | Add an inline recovery record to an existing archive. WinRAR's `rr` writes a 3% record and ignores every requested strength (measured: 6.23 and 7.23, switch or positional), so the 3% default is matched and an explicit percentage (`rr archive.rar 20`) is our own extension. On `a`/`update`, a bare `-rr<N>` is the legacy RAR4 parity-sector count (`-rr10` writes exactly ten sectors at any archive size); `-rr<N>%` is a percentage; bare `-rr` is WinRAR's 3%. RAR5 records are sized by percent only, so a bare `-rr<N>` means that percent there. Mutually exclusive with `-rv`                                                                                                                          |
| `rv[N]`     |       | Create `.rev` recovery volumes for an existing set (`rv3` / `rv10%`, default 10%)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `r`         |       | Repair the archive with its recovery record, reporting WinRAR's per-sector line `Sector N (offsets XXX...YYY) damaged - data recovered` (hex offsets) for every damaged sector; damage parity cannot rebuild reads `cannot recover data`, then it asks `Reconstruct archive structure ? [Y]es, [N]o` (quiet mode rebuilds without asking) and exits 3, like WinRAR. With no record it reconstructs `rebuilt.<name>` from the members that still decode and verify, resyncing past corrupt RAR5/RAR4 headers (plaintext headers only; exit 3 when a header was lost for RAR5, 0 for RAR4, like WinRAR). RAR 1.3/1.4 prints the banner, says `Cannot repair archive with old format` and produces nothing, like WinRAR |
| `rc`        |       | Rebuild missing volumes from `.rev` files                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `s`         |       | Convert the archive to self-extracting (SFX)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `s-`        |       | Strip the SFX module from an SFX archive                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `c`         |       | Set the archive comment (stdin, or `-z<file>`)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `cw`        |       | Write the archive comment to stdout, or to a file with `cw archive file`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `cf`        |       | Set/remove a per-member comment (RAR4 only; the library rejects it for RAR5, which has no member-comment block)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `p`         |       | Print a member to stdout                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `x`         |       | Extract with full paths                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `e`         |       | Extract without paths (flat)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `t`         |       | Test archive contents                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `v`         |       | Verbose list (WinRAR's `Attributes/Size/Packed/Ratio/Date/Time/Checksum/Name` table; on a volume set only the opened volume's members, with `-->`/`<->`/`<--` fragment ratios and per-fragment `Pack-CRC32`)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `l`         |       | List contents (WinRAR's `Attributes/Size/Date/Time/Name` table; volume sets list only the opened volume's members)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `lb`        |       | List bare (names only)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `lt`        |       | List technical (WinRAR's per-member block: type, sizes, ratio, nanosecond mtime, attributes, CRC32, host OS, compression); `lta` accepted as an alias                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `vb` / `vt` |       | Verbose bare / verbose technical (`vta` alias accepted)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `i`         |       | Show archive info (file/dir counts, total & packed size, ratio)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `i<string>` |       | Find a string inside members (`ic`/`ih` variants); **`i` alone is Info, not search**                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |

Global flags: `-y` (assume yes), `--quiet` (`-idq`), `--err` (`-ierr`),
`--work-dir <path>` (`-w<path>`; the directory WinRAR uses for temporary files —
it must exist and never changes where outputs are written).

Quiet mode follows each official tool: `rar -idq` answers its own prompts with
Yes (an existing destination is replaced without asking), while `unrar -idq`
still asks — and, without a console, keeps the non-interactive default of
skipping. With a terminal and no `-y`/`-o±`/`-or`/`-f`/`-u`, extraction asks
`Y/N/A/R/Q` per existing file, like WinRAR's console mode.

### Input syntax (WinRAR parity)

- **List files**: any file/member argument starting with `@` names a plain text
  list (`rar a arc @files.lst`, `unrar x arc @members.lst`); `@` alone reads the
  list from stdin. `//` starts a comment, blank lines are ignored, and `-@`
  disables list processing (`-@+` re-enables). Lists are decoded as UTF-8 with a
  Latin-1 fallback; UTF-16 lists are detected by BOM. `-sc<charset>l` is
  accepted (its charset conversion is not applied).
- **Implicit `*.*`**: `rar a archive` with no files (and no `-si`) archives
  everything in the current directory, like WinRAR.
- **Creation names**: `rar a foo f.txt` writes `foo.rar` — a missing extension
  is filled in with `.rar`.
- **Member filters**: `t`, `v`, `l`, `lb`, `lt`, `vb`, `vt` (and the UnRAR
  equivalents) accept member names after the archive and process only the
  matches; a filter matching nothing is an error for `t` (exit 10) while the
  listing commands print an empty table and exit 0, like WinRAR.
- **Extraction destination**: the trailing argument is the destination when it
  ends with a path separator (`rar x arc.rar dest\`); an explicit `--dest` wins.
- `rar d archive` without members is a successful no-op, like WinRAR.

### Compression & format

| Switch                                                   | Meaning                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| -------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `-m0` … `-m5`                                            | Compression level (Store … Best)                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `-ma5`                                                   | RAR5 format (default; v50, with a > 4 GiB `-md` keeping WinRAR's auto v50/v70 semantics)                                                                                                                                                                                                                                                                                                                                                                   |
| `-ma7`                                                   | Force RAR7 (v70) members at any dictionary size — an extension beyond WinRAR 7.23, which only switches to v70 above a 4 GiB dictionary                                                                                                                                                                                                                                                                                                                     |
| `-ma4`                                                   | Legacy RAR3/4 container (v29): full write-side since 2026-09 — STORE + LZSS m1–m5 + PPMd, VM filters, -hp, RR, solid chains; byte-verified against WinRAR 6.23 (the last RAR4 producer) and read by 7.23/UnRAR                                                                                                                                                                                                                                             |
| `-ma13` / `-ma14`                                        | DOS-era RAR 1.3/1.4 container (`RE~^`, v14): STORE + Unpack15 m1–m5, solid chains, archive comments, `-p` member encryption and old-style `.rar`/`.r00` volume sets (`-v`, up to 901 volumes); `-hp`/recovery/quick-open/owner/streams/BLAKE2sp/dictionaries are rejected, and `-sfx` is rejected because official tools only recognize the legacy DOS stub. Extension beyond WinRAR 7.23 (which cannot write this container); UnRAR 7.23 reads the output |
| `-ma2`                                                   | Legacy RAR 2.x container (v20) member writer                                                                                                                                                                                                                                                                                                                                                                                                               |
| `-ma15`                                                  | Legacy RAR 1.5 container (v15) member writer                                                                                                                                                                                                                                                                                                                                                                                                               |
| `-md<size>`                                              | Dictionary size (incl. RAR7 >4 GiB when `-ma7`); follows `-md`, default 32 MiB, capped at 2× member size                                                                                                                                                                                                                                                                                                                                                   |
| `-mdx<size>`                                             | Decompression dictionary cap (default 4 GiB)                                                                                                                                                                                                                                                                                                                                                                                                               |
| `-mt<threads>`                                           | Compression/decompression thread count                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `-s` / `-ds`                                             | Solid archive / disable solid sorting                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `-s=d` / `-s=v` / `-s=e` (aliases `-sd` / `-sv` / `-se`) | Split the solid statistics: continuously (default), at each volume (`-sv`) or when the member extension changes (`-se`); `-sv` is rejected for RAR4 and both resets are rejected for RAR 1.3–2.x, whose chains cannot carry a reset flag. The switch is accepted (and ignored) on read commands, like WinRAR                                                                                                                                               |
| `-ms<list>`                                              | List of file types to store without compressing                                                                                                                                                                                                                                                                                                                                                                                                            |
| `-mcl`                                                   | Long-distance matching (WinRAR hidden switch) — automatic at `-m2`…`-m5`; the `-mcl` switch is accepted (no-op) because long-range matching is always on for those levels, matching WinRAR 7.23                                                                                                                                                                                                                                                            |
| `-mc[ch][mode][+/-]`                                     | Advanced filter policy: `-mc-` disables every filter, `-mcd-`/`-mce-` disable delta/x86, `-mcd+`/`-mce+` force them on all data (`-mcd<N>+` picks the delta channel count, 1–31); forcing both (`-mcd+ -mce+` / `-mcde+`) emits one non-overlapping filter per 64 KiB block, like WinRAR (an overlapping pair is what a reader rejects); `-mcl±`/`-mcx±` are accepted without effect (long-range always on, exhaustive search not implemented)             |
| filters                                                  | Automatic output filters: x86 `E8`/`E8E9` for code **and delta (multimedia) for correlated multi-channel data** (audio PCM, raw bitmaps, database pages) are applied per-member before LZSS and written as non-solid filter members; both decode byte-for-byte under WinRAR/UnRAR                                                                                                                                                                          |

### Encryption & integrity

| Switch                 | Meaning                                                                                                                                                                                                                                                                                                                                                    |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `-p<password>` / `-p-` | Set / clear password (file-level; AES-256 for RAR5, the legacy per-generation ciphers for RAR 1.5–4.x)                                                                                                                                                                                                                                                     |
| `-hp<password>`        | Encrypt headers too (`-hp`); multi-volume sets repeat the plaintext encryption header on every volume. RAR5 `-hp` editing re-encrypts every rewritten header, so `rn`/`ch`, archive comments (`c`/`cw`, `a -z`) and delete (single- and multi-volume) all work, as do RAR4 `-hp` comments; `-k`/lock stays refused (the encrypted main header cannot grow) |
| `-htb`                 | BLAKE2sp hash records, **replacing** the CRC32 field (verified on read)                                                                                                                                                                                                                                                                                    |
| `-htc`                 | CRC32 hash records (the default; accepted on every command, like WinRAR)                                                                                                                                                                                                                                                                                   |

### Volumes & recovery

| Switch       | Meaning                                                                                                                                                                                                                                                                                                                                                                                                  |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `-v<size>`   | Multi-volume (e.g. `-v1m` ≈ 1 MB, `-v100k` ≈ 100 KB); sets of 10+ volumes use zero-padded `part01` names like WinRAR                                                                                                                                                                                                                                                                                     |
| `-rr[N]`     | Inline recovery record; N = count or `N%` percent, default 10% (the `-rv` switch below takes a **required** value, no default)                                                                                                                                                                                                                                                                           |
| `-rv<N\|N%>` | Recovery volumes; at creation the count is capped at the data-volume count (the standalone `rv` command at 10×). RAR4 sets (`-ma4`) use the legacy `.rev` layout: trailer format (`base.partNN.rev` / `baseN.rev`) when the volumes end in zero bytes (WinRAR-created sets), legacy full-parity format (`base<data>_<rec>_<idx>.rev`) otherwise; silently skipped when the archive ends up single-volume |
| `-qo[-\|+]`  | Quick-open record; `-qo` writes it, `-qo-` disables it (the default, as in WinRAR's console `rar a`). Listing commands (`l`/`v`/`lt`/`lb`/`i`, and UnRAR's) read it when present and fall back to a full scan otherwise, like WinRAR                                                                                                                                                                     |

### Paths, time & misc

**Recursion, selection, destination**

- `-r` / `-r0` / `-r-` — recurse into subdirectories (with / without / none)
- `-ep` / `-ep1` / `-ep2` / `-ep3` / `-ep4<path>` — path strip
- `-ap<path>` — archive path prefix
- `-x` / `-x@<list>` — exclude; `-n` / `-n@<list>` — include
- `-ed` / `-as` — empty dirs / sync
- `-ad` / `-ad1` / `-ad2` / `-am` — append the archive name to the destination;
  extract into each archive's own directory (with / without a per-archive
  subdirectory); archive metadata
- `-op<path>` / `-or` — output path / auto-rename

**Links, streams, metadata**

- `-ol` / `-ol-` / `-ola` / `-oh` — store symlinks as redirects / skip links
  when archiving and extracting / extract links with dangerous targets as-is
  (`-ola` disables the link safety checks). With `-ol` a directory symlink or
  junction is stored as a redirect instead of walking its target — Windows
  writes a Windows symlink (2) or junction (3) like WinRAR. Hard links:
  hard-link groups store the first path and redirect the rest, and every
  redirect keeps the link's modification time (Windows and Unix, RAR5 only).
  Extraction recreates a junction as a real NTFS mount point.
- `-os` / `-ow` — NTFS streams / owner
- `-om[-|1][=ext;ext]` — propagate the archive's Mark of the Web to extracted
  files: zone value only, every field with `1`, optional extension filter
  (Windows only)
- `-oi[0-4][:<minsize>]` — identical files as references: `-oi`/`-oi1` store the
  first file and reference the rest, `-oi2` announces the groups, `-oi3`/`-oi4`
  list them and create no archive; default 64 KiB minimum (RAR5 only)
- `-sfx[name]` — create an SFX archive at create time
- `-z<file>` / `-c-` — comment file / no comment (`-z` is accepted and ignored
  outside the create and comment commands)

**Time**

- `-ta` / `-tb` / `-tn` / `-to` — time filters
- `-tl` / `-tk[<date>]` — set the archive time to newest / keep it, or set it to
  the given local date
- `-ts[mca][±,1]` — three timestamps; `-ts-` omits the time field for RAR5 and
  is ignored for RAR 1.3–4.x, whose fixed headers always carry DOS local time
- `-tsp` — preserve source access time

**Misc**

- `-df` / `-kb` / `-si<name>` — delete sources / keep broken / stdin member
- `-ver[n]` / `-ag[fmt]` — versioning / auto-name (local time)
- `-y` / `-o±` — yes / overwrite mode. On a console (stdin is a terminal),
  extraction without `-y`/`-o±`/`-or` asks before replacing each existing file
  (`Y`es / `N`o / `A`ll / `R`ename / `Q`uit), like WinRAR; a non-interactive run
  (piped stdin, CI) keeps WinRAR's non-interactive outcome and skips. `-o+`
  overwrites, `-o-` skips and `-or` auto-renames — none of them prompt
- `-ierr` / `-ilog` / `-iver`, `-cfg-`, `-sc<charset>`
- `--max-unpacked <size>` / `--max-total-unpacked <size>` — bound how much a
  disk extraction may write: the first rejects any member whose _declared_
  uncompressed size exceeds `<size>`, the second rejects a run whose total
  would. Sizes take an optional binary `k`/`m`/`g`/`t` suffix. **The default is
  unbounded, exactly like WinRAR/UnRAR** — this is the opt-in guard that keeps a
  decompression bomb from filling the disk. It applies to the files `x`/`e`
  write; `-so` creates no files and is not bounded. A per-member cap larger than
  the total cap is a usage error.
- `-me<par>` (including the undocumented `-mes`) — accepted as a no-op
- `-log[AFPU]*[=name]` — rar only (UnRAR rejects it, like the official binary):
  writes archive names (`A`), processed member names (`F`), appending with `P`
  and UTF-16LE output with `U` to a log file (default `rarinfo.log`)

**Accepted as no-ops** (Windows-only or interactive in WinRAR, like WinRAR's own
parser): `-ac`, `-ai`, `-ao`, `-e[+]<attr>`, `-dh`, `-ieml`, `-ioff`, `-isnd`,
`-ri`, `-mlp`, `-oc`, `-oni`, `-am[s,r]`, `-vp`, `-sc`. `-os` is a no-op off
Windows. **Rejected with an error** rather than silently ignored, because they
would imply destructive changes: `-dr` (recycle bin), `-dw` (wipe), `-vd` (erase
disk).

---

## `unrar` — extract, list, test, print

```
unrar <command> [-p<password>] [--dest <path>] archive[.rar] [names...]
```

The destination directory is set with `--dest <path>` (default `.`); every
`names...` argument is a **member selector**: a non-mask name must match the
full stored path or name a directory (selecting its subtree), while `*`/`?`
masks also match the basename anywhere in the tree. A name matching nothing is a
hard error, never a silent dump into a `<name>/` folder.

| Command                         | Action                                                                       |
| ------------------------------- | ---------------------------------------------------------------------------- |
| `x`                             | Extract with full paths                                                      |
| `e`                             | Extract without paths (flat)                                                 |
| `l`                             | List contents                                                                |
| `t`                             | Test integrity                                                               |
| `p`                             | Print a member to stdout                                                     |
| `v` / `lb` / `lt` / `vb` / `vt` | Verbose list / list bare / list technical / verbose bare / verbose technical |

`unrar` accepts the same password/path/time switches as `rar` where they apply
(e.g. `-p<password>`, `-o±`, `-y`, `-kb`), plus `--max-unpacked` /
`--max-total-unpacked` on `x`/`e` (see _Paths, time & misc_ above).

---

## Examples

```bash
# Create a 5-level archive
rar a -m5 backup.rar src/ notes.txt

# Force RAR7 members and a 1 GiB dictionary
rar a -ma7 -md1g backup.rar bigfile.bin

# Multi-volume, 100 MB per volume, with recovery volumes
rar a -v100m -rv10% backup.part1.rar data/
rar rv backup.part1.rar          # (re)create .rev volumes
rar rc backup.part1.rar          # rebuild a missing volume from .rev

# Delete / rename without rebuilding
rar d backup.rar old.log
rar rn backup.rar old.txt new.txt

# Encrypt (file-level and header-level)
rar a -pSecret secret.rar docs/
rar a -hpSecret secret.rar docs/

# Recovery record + repair
rar rr backup.rar 10                # or: rar a -rr10 backup.rar data/
rar r backup.rar                 # streaming repair

# SFX
rar s backup.rar                 # make self-extracting
rar s- backup.rar                # strip the SFX module

# Inspect
rar l  backup.rar
rar lt backup.rar                # technical list
rar i"TODO" backup.rar           # find a string
unrar x --dest out/ backup.rar   # extract all
unrar x --dest out/ backup.rar a.txt  # extract only a.txt
unrar t backup.rar               # test
```

### A note on `rar d`

`rar d` removes members without recompressing the rest: kept file blocks
(header + compressed payload) are copied byte-for-byte, so the operation scales
with the archive size — not with the remaining data. RAR5 solid archives
recompress only the chain affected by the deletion. Solid RAR4 archives are
fully repacked (decode → re-encode), matching WinRAR 7.21+. Inline recovery
records are dropped and the quick-open record is rebuilt.
