# Command-Line Reference

rar-rs ships two binaries, `rar` and `unrar`, modelled on the WinRAR 7.x
console tools. Every official command is implemented. Switches follow
WinRAR 7.23 semantics; unsupported switches are either rejected or accepted
as no-ops where WinRAR does the same. This page is the usage reference.

---

## `rar` — create and modify archives

```
rar <command> [switches] archive[.rar] [files...]

For extract commands (`x`/`e`), every `files...` argument is a **member
selector** (full stored path or basename); the destination directory is set
with `--dest <path>` (default `.`). A name matching nothing is a hard error,
never a silent dump into a `<name>/` folder.
```

### Commands

| Command | Alias | Action |
|---|---|---|
| `a` | | Add files (creates the archive if missing); `a -f`/`a -u` behave like the `f`/`u` commands, `-k` locks the result and `-z<file>` sets the comment |
| `u` | | Update: add missing files, replace newer ones |
| `f` | | Freshen: update existing members only |
| `m` | | Move: add files, then erase the sources |
| `mf` | | Move files only: the tree is archived like `m`, but directories are left on disk |
| `d` | | Delete members without rebuilding the archive |
| `rn` | | Rename archived members |
| `ch` | | Change parameters (`-cl`/`-cu` name case conversion) |
| `k` | | Lock the archive (read-only) |
| `rr` | | Add an inline recovery record (percent is a trailing argument: `rar rr archive.rar 10`, default 10%) |
| `rv[N]` | | Create `.rev` recovery volumes for an existing set (`rv3` / `rv10%`, default 10%) |
| `r` | | Repair the archive with its recovery record |
| `rc` | | Rebuild missing volumes from `.rev` files |
| `s` | | Convert the archive to self-extracting (SFX) |
| `s-` | | Strip the SFX module from an SFX archive |
| `c` | | Set the archive comment (stdin, or `-z<file>`) |
| `cw` | | Write the archive comment to stdout |
| `cf` | | Set/remove a per-member comment (RAR4 only; the library rejects it for RAR5, which has no member-comment block) |
| `p` | | Print a member to stdout |
| `x` | | Extract with full paths |
| `e` | | Extract without paths (flat) |
| `t` | | Test archive contents |
| `v` | | Verbose list |
| `l` | | List contents |
| `lb` | | List bare (names only) |
| `lt` | | List technical (sizes/CRC/mtime); `lta` accepted as an alias |
| `vb` / `vt` | | Verbose bare / verbose technical (`vta` alias accepted) |
| `i` | | Show archive info (file/dir counts, total & packed size, ratio) |
| `i<string>` | | Find a string inside members (`ic`/`ih` variants); **`i` alone is Info, not search** |

Global flags: `-y` (assume yes), `--quiet` (`-idq`), `--err` (`-ierr`),
`--work-dir <path>` (`-w<path>`).

### Compression & format

| Switch | Meaning |
|---|---|
| `-m0` … `-m5` | Compression level (Store … Best) |
| `-ma5` | RAR5 format (default; v50, with a > 4 GiB `-md` keeping WinRAR's auto v50/v70 semantics) |
| `-ma7` | Force RAR7 (v70) members at any dictionary size — an extension beyond WinRAR 7.23, which only switches to v70 above a 4 GiB dictionary |
| `-ma4` | Legacy RAR3/4 container (v29): full write-side since 2026-09 — STORE + LZSS m1–m5 + PPMd, VM filters, -hp, RR, solid chains; byte-verified against WinRAR 6.23 (the last RAR4 producer) and read by 7.23/UnRAR |
| `-ma13` / `-ma14` | DOS-era RAR 1.3/1.4 container (`RE~^`, v14), single volume: STORE + Unpack15 m1–m5, solid chains, archive comments and `-p` member encryption; `-hp`/recovery/volumes/quick-open/owner/streams/BLAKE2sp/dictionaries are rejected. Extension beyond WinRAR 7.23 (which cannot write this container); UnRAR 7.23 reads the output |
| `-ma2` | Legacy RAR 2.x container (v20) member writer |
| `-ma15` | Legacy RAR 1.5 container (v15) member writer |
| `-md<size>` | Dictionary size (incl. RAR7 >4 GiB when `-ma7`); follows `-md`, default 32 MiB, capped at 2× member size |
| `-mdx<size>` | Decompression dictionary cap (default 4 GiB) |
| `-mt<threads>` | Compression/decompression thread count |
| `-s` / `-ds` | Solid archive / disable solid sorting |
| `-ms<list>` | List of file types to store without compressing |
| `-mcl` | Long-distance matching (WinRAR hidden switch) — automatic at `-m2`…`-m5`; the `-mcl` switch is accepted (no-op) because long-range matching is always on for those levels, matching WinRAR 7.23 |
| `-mc[ch][mode][+/-]` | Advanced filter policy: `-mc-` disables every filter, `-mcd-`/`-mce-` disable delta/x86, `-mcd+`/`-mce+` force them on all data (`-mcd<N>+` picks the delta channel count, 1–31); `-mcl±`/`-mcx±` are accepted without effect (long-range always on, exhaustive search not implemented) |
| filters | Automatic output filters: x86 `E8`/`E8E9` for code **and delta (multimedia) for correlated multi-channel data** (audio PCM, raw bitmaps, database pages) are applied per-member before LZSS and written as non-solid filter members; both decode byte-for-byte under WinRAR/UnRAR |

### Encryption & integrity

| Switch | Meaning |
|---|---|
| `-p<password>` / `-p-` | Set / clear password (file-level; AES-256 for RAR5, the legacy per-generation ciphers for RAR 1.5–4.x) |
| `-hp<password>` | Encrypt headers too (`-hp`); multi-volume sets repeat the plaintext encryption header on every volume |
| `-htb` | BLAKE2sp hash records (verified on read) |
| `-htc` | CRC32 hash (default; accepted) |

### Volumes & recovery

| Switch | Meaning |
|---|---|
| `-v<size>` | Multi-volume (e.g. `-v1m` ≈ 1 MB, `-v100k` ≈ 100 KB); sets of 10+ volumes use zero-padded `part01` names like WinRAR |
| `-rr[N]` | Inline recovery record; N = count or `N%` percent, default 10% (the `-rv` switch below takes a **required** value, no default) |
| `-rv<N\|N%>` | Recovery volumes; capped at 10× the volume count. RAR4 sets (`-ma4`) use the legacy `.rev` layout: trailer format (`base.partNN.rev` / `baseN.rev`) when the volumes end in zero bytes (WinRAR-created sets), legacy full-parity format (`base<data>_<rec>_<idx>.rev`) otherwise; silently skipped when the archive ends up single-volume |
| `-qo[-|+]` | Quick-open records: `-qo`/`-qo+` enable, `-qo-` disables (opt-in) |

### Paths, time & misc

`-r`/`-r0`/`-r-` (recurse), `-ep`/`-ep1`/`-ep2`/`-ep3`/`-ep4<path>` (path
strip), `-ap<path>` (archive path prefix), `-x`/`-x@` (exclude),
`-n`/`-n@` (include), `-ed`/`-as`/`-ad`/`-am` (empty dirs / sync / append archive
name to dest / archive metadata), `-ol`/`-ol-`/`-ola`/`-oh` (store symlinks as
redirects / skip links when archiving and extracting / extract links with
dangerous targets as-is (disables the link safety checks); hard links:
hard-link groups store the first path and redirect the rest — Windows and
Unix, RAR5 only),
`-op<path>`/`-or` (output path / auto-rename), `-os`/`-ow` (NTFS streams /
owner), `-om[-|1][=ext;ext]` (propagate the archive's Mark of the Web to
extracted files: zone value only, every field with `1`, optional extension
filter; Windows only),
`-oi[0-4][:<minsize>]` (identical files as references: `-oi`/`-oi1`
store the first file and reference the rest, `-oi2` announces the groups,
`-oi3`/`-oi4` list them and create no archive; default 64 KiB minimum,
RAR5 only), `-df`/`-kb`/`-si<name>` (delete sources / keep broken / stdin
member), `-ta`/`-tb`/`-tn`/`-to` (time filters), `-tl`/`-tk` (set archive
time to newest / keep), `-ts[mca][±,1]` (three timestamps), `-tsp` (preserve
source access time), `-ver[n]` (versioning), `-ag[fmt]` (auto-name),
`-z<file>`/`-c-` (comment file / no comment), `-y`/`-o±` (yes / overwrite
mode), `-ierr`/`-ilog`/`-iver`, `-cfg-`/`-sc<charset>`.

Switches that are Windows-only or interactive in WinRAR (`-ac`, `-ai`,
`-ao`, `-e[+]<attr>`, `-dh`, `-ieml`, `-ioff`, `-isnd`, `-ri`, `-mlp`,
`-oc`, `-oni`, `-am[s,r]`, `-vp`, `-sc`) are **accepted as no-ops** on
every command of both binaries, like WinRAR's own parser. `-os` (NTFS
streams) is implemented on Windows:
create stores the file's alternate data streams as `STM` records (each
stream is encrypted with the archive password when `-p`/`-hp` is set) and
extraction restores them; on other platforms it is a no-op. `-dr` (recycle
bin) and `-dw` (wipe) are **rejected with an error** rather than silently
ignored, since they would otherwise imply source deletion. `-me<par>`
(including the undocumented `-mes`) is accepted as a no-op.
`-log[AFPU]*[=name]` (rar only; UnRAR rejects it like the official binary)
writes archive names (`A`), processed member names (`F`), appending with
`P` and UTF-16LE output with `U` to a log file (default `rarinfo.log`).

---

## `unrar` — extract, list, test, print

```
unrar <command> [-p<password>] [--dest <path>] archive[.rar] [names...]
```

The destination directory is set with `--dest <path>` (default `.`); every
`names...` argument is a **member selector** (full stored path or basename). A
name matching nothing is a hard error, never a silent dump into a `<name>/`
folder.

| Command | Action |
|---|---|
| `x` | Extract with full paths |
| `e` | Extract without paths (flat) |
| `l` | List contents |
| `t` | Test integrity |
| `p` | Print a member to stdout |
| `v` / `lb` / `lt` / `vb` / `vt` | Verbose list / list bare / list technical / verbose bare / verbose technical |

`unrar` accepts the same password/path/time switches as `rar` where they
apply (e.g. `-p<password>`, `-o±`, `-y`, `-kb`).

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
(header + compressed payload) are copied byte-for-byte, so the operation
scales with the archive size — not with the remaining data. RAR5 solid
archives recompress only the chain affected by the deletion. Solid RAR4
archives are fully repacked (decode → re-encode), matching WinRAR 7.21+.
Inline recovery records are dropped and the quick-open record is rebuilt.
