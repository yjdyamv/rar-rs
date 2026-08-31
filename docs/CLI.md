# Command-Line Reference

rar-rs ships two binaries, `rar` and `unrar`, modelled on the WinRAR 7.x
console tools. Every official command is implemented. Switches follow
WinRAR 7.23 semantics; unsupported switches are either rejected (e.g.
`-ma4`) or accepted as no-ops where WinRAR does the same. This page is the usage
reference.

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
| `a` | | Add files (creates the archive if missing) |
| `u` | | Update: add missing files, replace newer ones |
| `f` | | Freshen: update existing members only |
| `m` | | Move: add files, then erase the sources |
| `d` | | Delete members without rebuilding the archive |
| `rn` | | Rename archived members |
| `ch` | | Change parameters (`-cl`/`-cu` name case conversion) |
| `k` | | Lock the archive (read-only) |
| `rr[N]` | | Add an inline recovery record (N = percent) |
| `rv[N]` | | Create `.rev` recovery volumes for an existing set |
| `r` | | Repair the archive with its recovery record |
| `rc` | | Rebuild missing volumes from `.rev` files |
| `s` | | Convert the archive to self-extracting (SFX) |
| `s-` | | Strip the SFX module from an SFX archive |
| `c` | | Set the archive comment (stdin, or `-z<file>`) |
| `cw` | | Write the archive comment to stdout |
| `p` | | Print a member to stdout |
| `x` | | Extract with full paths |
| `e` | | Extract without paths (flat) |
| `t` | | Test archive contents |
| `v` | | Verbose list |
| `l` | | List contents |
| `lb` | | List bare (names only) |
| `lt` | | List technical (sizes/CRC/mtime) |
| `vb` / `vt` | | Verbose bare / verbose technical |
| `i` | | Show archive info (file/dir counts, total & packed size, ratio) |
| `i<string>` | | Find a string inside members (`ic`/`ih` variants); **`i` alone is Info, not search** |

Global flags: `-y` (assume yes), `--quiet` (`-idq`), `--err` (`-ierr`),
`--work-dir <path>` (`-w<path>`).

### Compression & format

| Switch | Meaning |
|---|---|
| `-m0` … `-m5` | Compression level (Store … Best) |
| `-ma5` | RAR5 format (default; inert like WinRAR's own `-ma5`) |
| `-ma7` | Force RAR7 (v70) members at any dictionary size — an extension beyond WinRAR 7.23, which only switches to v70 above a 4 GiB dictionary |
| `-ma4` | Rejected with "Unknown option", matching WinRAR 7.23 |
| `-md<size>` | Dictionary size (incl. RAR7 >4 GiB when `-ma7`); follows `-md`, default 32 MiB, capped at 2× member size |
| `-mdx<size>` | Decompression dictionary cap (default 4 GiB) |
| `-mt<threads>` | Compression/decompression thread count |
| `-s` / `-ds` | Solid archive / disable solid sorting |
| `-ms<list>` | List of file types to store without compressing |
| `-mcl` | Long-distance matching (WinRAR hidden switch) — automatic at `-m2`…`-m5`; the `-mcl` switch is accepted (no-op) because long-range matching is always on for those levels, matching WinRAR 7.23 |
| filters | Automatic output filters: x86 `E8`/`E8E9` for code **and delta (multimedia) for correlated multi-channel data** (audio PCM, raw bitmaps, database pages) are applied per-member before LZSS and written as non-solid filter members; both decode byte-for-byte under WinRAR/UnRAR |

### Encryption & integrity

| Switch | Meaning |
|---|---|
| `-p<password>` / `-p-` | Set / clear password (file-level AES-256) |
| `-hp<password>` | Encrypt headers too (`-hp`); multi-volume sets repeat the plaintext encryption header on every volume |
| `-htb` | BLAKE2sp hash records (verified on read) |
| `-htc` | CRC32 hash (default; accepted) |

### Volumes & recovery

| Switch | Meaning |
|---|---|
| `-v<size>` | Multi-volume (e.g. `-v1m` ≈ 1 MB, `-v100k` ≈ 100 KB); sets of 10+ volumes use zero-padded `part01` names like WinRAR |
| `-rr[N]` / `-rv[N]` | Inline recovery record / recovery volumes; N = count or `N%` percent, default 10%, capped at 10× the volume count |
| `-qo+` / `-qo-` | Enable / disable quick-open records |

### Paths, time & misc

`-r`/`-r0`/`-r-` (recurse), `-ep`/`-ep1`/`-ep2`/`-ep3`/`-ep4<path>` (path
strip), `-ap<path>` (archive path prefix), `-x`/`-x@` (exclude),
`-n`/`-n@` (include), `-ed`/`-as`/`-ad`/`-am` (empty dirs / sync / per-attr
dir / move-to-archive), `-ol`/`-oh` (store sym/hard links as links),
`-op<path>`/`-or` (output path / auto-rename), `-os`/`-ow` (NTFS streams /
owner), `-df`/`-kb`/`-si<name>` (delete sources / keep broken / stdin
member), `-ta`/`-tb`/`-tn`/`-to` (time filters), `-tl`/`-tk` (set archive
time to newest / keep), `-ts[mca][±,1]` (three timestamps), `-tsp` (preserve
source access time), `-ver[n]` (versioning), `-ag[fmt]` (auto-name),
`-z<file>`/`-c-` (comment file / no comment), `-y`/`-o±` (yes / overwrite
mode), `-ierr`/`-ilog`/`-iver`, `-cfg-`/`-sc<charset>`.

Switches that are Windows-only or interactive in WinRAR (e.g. `-ac`, `-dh`,
`-dr`, `-dw`, `-ieml`, `-ioff`, `-isnd`, `-ri`, `-mlp`, `-oc`, `-oni`,
`-oi`) are **accepted as no-ops**. `-log` and `-om` are not implemented.

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
rar a -v100m -rv backup.part1.rar data/
rar rv backup.part1.rar          # (re)create .rev volumes
rar rc backup.part1.rar          # rebuild a missing volume from .rev

# Delete / rename without rebuilding
rar d backup.rar old.log
rar rn backup.rar old.txt new.txt

# Encrypt (file-level and header-level)
rar a -pSecret secret.rar docs/
rar a -hpSecret secret.rar docs/

# Recovery record + repair
rar rr10 backup.rar
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
scales with the archive size — not with the remaining data. Solid archives
recompress only the chain affected by the deletion; inline recovery records
are dropped and the quick-open record is rebuilt, matching the official
`rar d`.
