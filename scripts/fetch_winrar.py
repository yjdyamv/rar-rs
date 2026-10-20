#!/usr/bin/env python3
"""Fetch the RAR/WinRAR builds the generated test corpus drives.

The legacy test suites build their archives at run time with an official
`Rar.exe` (see `crates/rar/tests/support/generate.rs`), so a checkout with an
empty cache skips most of the RAR 1.5-4.x decode coverage. This script fills
`<cache>/<version>/` from rarlab so CI can run those suites for real. It is the
single place that knows which release serves which purpose; `docs/testing.md`
documents the policy.

Usage:
    python3 scripts/fetch_winrar.py                # versions the suites need
    python3 scripts/fetch_winrar.py 2-90 7-23      # explicit subset
    python3 scripts/fetch_winrar.py --all          # every known release
    python3 scripts/fetch_winrar.py --check        # verify, download nothing
    python3 scripts/fetch_winrar.py --list         # print the table

Environment:
    SA_WINRAR_CACHE  destination root (default: <repo>/.cache/winrar)
    SA_WINRAR_SRC    download cache    (default: <repo>/.cache/winrar-src)

Both official package shapes are self-extracting: the Windows installers are
PE stubs wrapping a RAR4 payload (up to 5.00) or a RAR5 payload (5.50 and
newer), the Linux ones are plain `.tar.gz`. Extracting therefore needs 7-Zip or
a `Rar.exe` new enough for the payload — an old cached release can only unpack
installers of its own era, so candidates are tried newest first. Releases with
no Linux build (2.90, 3.00) are reported as unavailable instead of failing the
run: the recipes that need them self-skip.
"""

from __future__ import annotations

import http.client
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CACHE = Path(os.environ.get("SA_WINRAR_CACHE") or REPO_ROOT / ".cache" / "winrar")
SRC = Path(os.environ.get("SA_WINRAR_SRC") or REPO_ROOT / ".cache" / "winrar-src")
BASE_URL = "https://www.rarlab.com/rar"

# version -> (Windows installer, Linux x64 tarball or None)
#
# The directory names are `<major>-<minor>`, the spelling
# `crates/rar/tests/support/generate.rs` uses for its tool constants.
TABLE: dict[str, tuple[str, str | None]] = {
    "2-90": ("wrar290.exe", None),
    "3-00": ("wrar300.exe", None),
    "3-10": ("wrar310.exe", None),
    "3-20": ("wrar320.exe", None),
    "3-30": ("wrar330.exe", None),
    "3-40": ("wrar340.exe", None),
    "3-50": ("wrar350.exe", None),
    "3-60": ("wrar360.exe", None),
    "3-70": ("wrar370.exe", None),
    "3-71": ("wrar371.exe", None),
    "3-80": ("wrar380.exe", None),
    "3-90": ("wrar390.exe", None),
    "3-93": ("wrar393.exe", "rarlinux-x64-3.9.3.tar.gz"),
    "4-00": ("winrar-x64-400.exe", None),
    "4-11": ("winrar-x64-411.exe", None),
    "4-20": ("winrar-x64-420.exe", "rarlinux-x64-4.2.0.tar.gz"),
    "5-00": ("winrar-x64-500.exe", None),
    "5-50": ("winrar-x64-550.exe", "rarlinux-x64-5.5.0.tar.gz"),
    "5-91": ("winrar-x64-591.exe", "rarlinux-x64-5.9.1.tar.gz"),
    "6-23": ("winrar-x64-623.exe", "rarlinux-x64-623.tar.gz"),
    "7-23": ("winrar-x64-723.exe", "rarlinux-x64-723.tar.gz"),
}

# What the generated corpus actually asks for: 2-90 (the only release that
# still compresses format 20), 3-00 (legacy `.r00` + `<base>4_3_N.rev` naming),
# 4-20 (last `cf` command, modern `.rev` naming), 6-23 (last release that can
# write `-ma4`), 7-23 (current).
DEFAULT_VERSIONS = os.environ.get(
    "SA_WINRAR_VERSIONS", "2-90 3-00 4-20 6-23 7-23"
).split()

WINDOWS = os.name == "nt"
RAR_NAMES = ("Rar.exe", "rar")
UNRAR_NAMES = ("UnRAR.exe", "unrar")
SEVEN_ZIP_CANDIDATES = (
    "7z",
    "7za",
    "7zr",
    r"C:\Program Files\7-Zip\7z.exe",
    r"C:\Program Files (x86)\7-Zip\7z.exe",
)
WINRAR_CANDIDATES = (
    r"C:\Program Files\WinRAR\Rar.exe",
    r"C:\Program Files (x86)\WinRAR\Rar.exe",
)


def die(message: str) -> "None":
    print(f"error: {message}", file=sys.stderr)
    sys.exit(1)


def version_key(version: str) -> tuple[int, ...]:
    return tuple(int(part) for part in version.split("-"))


def find_tool(directory: Path, names: tuple[str, ...]) -> Path | None:
    """First of `names` present in `directory`, ignoring case."""
    if not directory.is_dir():
        return None
    wanted = {name.lower() for name in names}
    for entry in sorted(directory.iterdir()):
        if entry.is_file() and entry.name.lower() in wanted:
            return entry
    return None


def cached_rars_newest_first() -> list[Path]:
    """Cached `Rar.exe` binaries, newest release first."""
    if not CACHE.is_dir():
        return []
    found = []
    for directory in sorted(
        (d for d in CACHE.iterdir() if d.is_dir() and "-" in d.name),
        key=lambda d: version_key(d.name),
        reverse=True,
    ):
        tool = find_tool(directory, RAR_NAMES)
        if tool is not None:
            found.append(tool)
    return found


def download(url: str, dest: Path, verify=None) -> None:
    """Fetch `url` into `dest`, retrying short or corrupt transfers.

    `verify` is called with the finished file and is expected to raise when the
    download is unusable (a tarball is checked by opening it, which catches the
    truncated transfers a flaky link produces).
    """
    if dest.is_file() and dest.stat().st_size > 0:
        if verify is None or _verified(verify, dest):
            return
        dest.unlink()
    dest.parent.mkdir(parents=True, exist_ok=True)
    partial = dest.with_name(dest.name + ".part")
    print(f"  downloading {url}")
    for attempt in range(1, 4):
        try:
            with urllib.request.urlopen(url, timeout=120) as response:
                expected = response.headers.get("Content-Length")
                with open(partial, "wb") as out:
                    shutil.copyfileobj(response, out)
            if expected is not None and partial.stat().st_size != int(expected):
                raise OSError(
                    f"short transfer: {partial.stat().st_size} of {expected} bytes"
                )
            if verify is not None and not _verified(verify, partial):
                raise OSError("downloaded file is corrupt")
            partial.replace(dest)
            return
        except (urllib.error.URLError, OSError, http.client.HTTPException):
            # rarlab resets the HTTP/2 stream on some of the historic files after
            # the last byte, so a transfer error does not mean the payload is
            # bad — trust the verifier over the transport.
            if verify is not None and _verified(verify, partial):
                partial.replace(dest)
                print("  transfer reset after the payload; kept the verified file")
                return
            partial.unlink(missing_ok=True)
            if attempt == 3:
                die(f"download failed: {url}")
            time.sleep(2 * attempt)


def _verified(verify, path: Path) -> bool:
    try:
        verify(path)
    except Exception:  # noqa: BLE001 - any parse failure means "redownload"
        return False
    return True


def _check_tarball(path: Path) -> None:
    """Read every member, which is the only way to notice a truncated gzip."""
    with tarfile.open(path) as archive:
        for member in archive:
            if member.isfile():
                handle = archive.extractfile(member)
                if handle is not None:
                    while handle.read(1 << 20):
                        pass


def seven_zip() -> Path | None:
    for candidate in SEVEN_ZIP_CANDIDATES:
        resolved = shutil.which(candidate)
        if resolved:
            return Path(resolved)
        if Path(candidate).is_file():
            return Path(candidate)
    return None


def extract_windows_installer(installer: Path, dest: Path) -> bool:
    """Unpack a self-extracting Windows installer into `dest`."""
    seven = seven_zip()
    if seven is not None:
        shutil.rmtree(dest, ignore_errors=True)
        dest.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            [str(seven), "x", "-y", f"-o{dest}", str(installer)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if find_tool(dest, RAR_NAMES) is not None:
            return True

    # The installers are RAR SFX archives, so any Rar.exe can unpack them — but
    # an old release only understands the payload format of its own era.
    unpackers = cached_rars_newest_first()
    for candidate in WINRAR_CANDIDATES:
        if Path(candidate).is_file():
            unpackers.append(Path(candidate))
    for unpacker in unpackers:
        shutil.rmtree(dest, ignore_errors=True)
        dest.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            [str(unpacker), "x", "-y", "-o+", str(installer)],
            cwd=dest,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if find_tool(dest, RAR_NAMES) is not None:
            print(f"  unpacked with {unpacker}")
            return True
    return False


def install_windows(version: str, installer: Path) -> None:
    dest = CACHE / version
    with tempfile.TemporaryDirectory() as staging:
        stage = Path(staging)
        if not extract_windows_installer(installer, stage):
            die(
                f"could not extract {installer} (install 7-Zip or WinRAR, or "
                "point SA_WINRAR_CACHE at an existing tree)"
            )
        rar = find_tool(stage, RAR_NAMES)
        unrar = find_tool(stage, UNRAR_NAMES)
        if rar is None or unrar is None:
            die(f"{installer}: no Rar.exe/UnRAR.exe in the payload")
        dest.mkdir(parents=True, exist_ok=True)
        shutil.copy2(rar, dest / "Rar.exe")
        shutil.copy2(unrar, dest / "UnRAR.exe")
        sfx = find_tool(stage, ("Default.SFX", "default.sfx"))
        if sfx is not None:
            shutil.copy2(sfx, dest / "Default.SFX")
        readme = find_tool(stage, ("Rar.txt",))
        if readme is not None:
            shutil.copy2(readme, dest / "Rar.txt")


def install_linux(version: str, tarball: Path) -> None:
    dest = CACHE / version
    dest.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as staging:
        stage = Path(staging)
        with tarfile.open(tarball) as archive:
            if hasattr(tarfile, "data_filter"):
                archive.extractall(stage, filter="data")
            else:  # Python < 3.11.4
                archive.extractall(stage)
        rar = next((p for p in stage.rglob("rar") if p.is_file()), None)
        unrar = next((p for p in stage.rglob("unrar") if p.is_file()), None)
        if rar is None or unrar is None:
            die(f"{tarball}: no rar/unrar binaries")
        shutil.copy2(rar, dest / "rar")
        shutil.copy2(unrar, dest / "unrar")
        (dest / "rar").chmod(0o755)
        (dest / "unrar").chmod(0o755)
        for name in ("default.sfx", "rar.txt"):
            found = next((p for p in stage.rglob(name) if p.is_file()), None)
            if found is not None:
                shutil.copy2(found, dest / name)


def have_version(version: str) -> bool:
    dest = CACHE / version
    rar_names = RAR_NAMES if WINDOWS else ("rar", "Rar.exe")
    unrar_names = UNRAR_NAMES if WINDOWS else ("unrar", "UnRAR.exe")
    return (
        find_tool(dest, rar_names) is not None
        and find_tool(dest, unrar_names) is not None
    )


def fetch_version(version: str) -> bool:
    """Install one release. Returns False when this host has no build for it."""
    if have_version(version):
        print(f"{version}: cached")
        return True
    if version not in TABLE:
        die(f"unknown version '{version}' (try --list)")
    windows_installer, linux_tarball = TABLE[version]
    if WINDOWS:
        download(f"{BASE_URL}/{windows_installer}", SRC / windows_installer)
        print(f"{version}: extracting {windows_installer}")
        install_windows(version, SRC / windows_installer)
    elif linux_tarball is not None:
        download(
            f"{BASE_URL}/{linux_tarball}",
            SRC / linux_tarball,
            verify=_check_tarball,
        )
        print(f"{version}: extracting {linux_tarball}")
        install_linux(version, SRC / linux_tarball)
    else:
        print(
            f"{version}: no Linux build published on rarlab — skipped",
            file=sys.stderr,
        )
        return False
    if not have_version(version):
        die(f"{version}: tools missing after extraction")
    print(f"{version}: ok")
    return True


def listing() -> None:
    print(f"{'VERSION':<7} {'WINDOWS':<24} LINUX")
    for version, (windows_installer, linux_tarball) in TABLE.items():
        print(f"{version:<7} {windows_installer:<24} {linux_tarball or '-'}")


def check(versions: list[str]) -> int:
    missing = 0
    for version in versions:
        if have_version(version):
            print(f"{version}: ok")
        else:
            print(f"{version}: MISSING", file=sys.stderr)
            missing += 1
    return missing


def main(argv: list[str]) -> int:
    if not argv:
        versions = DEFAULT_VERSIONS
    elif argv[0] in ("--list", "-l"):
        listing()
        return 0
    elif argv[0] == "--all":
        versions = list(TABLE)
    elif argv[0] == "--check":
        return 1 if check(argv[1:] or DEFAULT_VERSIONS) else 0
    else:
        versions = argv

    print(f"cache: {CACHE} ({'windows' if WINDOWS else 'linux'})")
    unavailable = 0
    # Newest first: one working extractor is enough for the whole run, and only
    # a recent release can unpack the recent installers.
    for version in sorted(set(versions), key=version_key, reverse=True):
        if not fetch_version(version):
            unavailable += 1
    if unavailable:
        print(
            f"note: {unavailable} version(s) have no build for this host; "
            "the recipes that need them self-skip",
            file=sys.stderr,
        )
    print("done")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
