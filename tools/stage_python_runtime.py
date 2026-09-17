#!/usr/bin/env python3
"""Assembles the Python runtime that ships inside the SplatMCP installer.

Why the Windows *embeddable* package and not the development virtual environment:

* A virtual environment is only a few kilobytes of configuration that points at the base
  installation its `python.exe` came from. It has no standard library of its own, so it
  cannot be shipped to a machine that has no Python.
* The embeddable package is self-contained: its standard library lives in
  `python313.zip` inside the folder, so every path the app needs is *inside the runtime*
  and can be recorded relative to it.

The result is a runtime whose `sys.path` is exactly three relative entries - the standard
library zip, `DLLs` (the extension modules) and `Lib/site-packages` (the pinned packages) -
which `runtime-manifest.json` records for the app. The embedded interpreter installs that
list verbatim, so a recipe cannot import anything the installer did not ship.

Usage:

    python tools/stage_python_runtime.py                     # stage into src-tauri/resources
    python tools/stage_python_runtime.py --target <dir>      # stage somewhere else
    python tools/stage_python_runtime.py --keep-cache        # reuse the downloaded zip

`tools\\build_installer.cmd` calls it before packaging.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import urllib.request
import zipfile
from pathlib import Path

# The pinned set. Keep in step with tools/provision_python.cmd and
# docs/design/python-generation.md.
PYTHON_VERSION = "3.13.2"
EMBED_ZIP = f"python-{PYTHON_VERSION}-embed-amd64.zip"
EMBED_URL = f"https://www.python.org/ftp/python/{PYTHON_VERSION}/{EMBED_ZIP}"
PACKAGES = {
    "numpy": "2.3.3",
    "scipy": "1.16.2",
    "pillow": "11.3.0",
}

ROOT = Path(__file__).resolve().parent.parent

# Directories inside site-packages that are pure dead weight in an installer: test suites
# and byte-code caches. Removing them keeps the download smaller without changing any
# import a recipe can make.
PRUNE_DIRS = ("tests", "test", "__pycache__")
PRUNE_SUFFIXES = (".pyc", ".pyo")


def log(message: str) -> None:
    print(message, flush=True)


def download(url: str, destination: Path) -> None:
    """Downloads `url` to `destination` unless it is already there."""
    if destination.is_file() and destination.stat().st_size > 0:
        digest = hashlib.sha256(destination.read_bytes()).hexdigest()[:12]
        log(f"   cached {destination.name} ({digest})")
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    log(f"   downloading {url}")
    with urllib.request.urlopen(url, timeout=300) as response, destination.open("wb") as out:
        shutil.copyfileobj(response, out)
    log(f"   wrote {destination} ({destination.stat().st_size} bytes)")


def extract(archive: Path, target: Path) -> None:
    """Extracts the embeddable package, replacing whatever was there."""
    if target.exists():
        shutil.rmtree(target)
    target.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(archive) as bundle:
        bundle.extractall(target)
    names = sorted(entry.name for entry in target.iterdir())
    log(f"   extracted {len(names)} entries: {', '.join(names[:8])}"
        + (" ..." if len(names) > 8 else ""))


def install_packages(staging_site: Path, python: Path) -> None:
    """Installs the pinned wheels into the runtime's own `Lib/site-packages`.

    `--target` writes the wheel contents where they belong without creating a virtual
    environment, and `--only-binary :all:` refuses source distributions: a compiler is not
    part of the install story.
    """
    staging_site.mkdir(parents=True, exist_ok=True)
    requirements = [f"{name}=={version}" for name, version in PACKAGES.items()]
    command = [
        str(python),
        "-m",
        "pip",
        "install",
        "--disable-pip-version-check",
        "--no-input",
        "--only-binary",
        ":all:",
        "--target",
        str(staging_site),
        *requirements,
    ]
    log(f"   {' '.join(requirements)}")
    completed = subprocess.run(command, capture_output=True, text=True)
    if completed.returncode != 0:
        log(completed.stdout[-4000:])
        log(completed.stderr[-4000:])
        raise SystemExit(f"pip failed with exit code {completed.returncode}")
    for line in completed.stdout.splitlines():
        if "Installing collected packages" in line or "Successfully installed" in line:
            log(f"   {line.strip()}")


def write_pth(target: Path, entries: list[str]) -> None:
    """Writes the interpreter's `._pth` so the staged runtime runs standalone.

    An embeddable interpreter restricts `sys.path` to exactly the lines of this file. Using
    the *same* list the manifest gives the app means the staged `python.exe` is a faithful
    smoke test of what a packaged app will see.
    """
    for stale in target.glob("python*._pth"):
        stale.unlink()
    path = target / "python313._pth"
    path.write_text("\n".join(entries) + "\n", encoding="utf-8")
    log(f"   {path.name}: {entries}")


def collect_licenses(site: Path, target: Path) -> list[str]:
    """Copies the installed packages' own licence files into the runtime.

    The installer redistributes NumPy, SciPy and Pillow, so their licences (and the CPython
    licence that ships with the embeddable package) have to travel with the binary. The
    wheel metadata already carries them; this gathers them in one obvious place instead of
    leaving them scattered through `site-packages`.
    """
    destination = target / "THIRD-PARTY-LICENSES"
    if destination.exists():
        shutil.rmtree(destination)
    destination.mkdir(parents=True, exist_ok=True)

    collected: list[str] = []
    for dist_info in sorted(site.glob("*.dist-info")):
        package = dist_info.name.split("-")[0]
        sources = list((dist_info / "licenses").glob("**/*")) + list(dist_info.glob("LICENSE*"))
        written = 0
        for source in sources:
            if not source.is_file():
                continue
            # Flatten the path: the same licence can appear in `licenses/` and next to the
            # metadata, and a flat file name per package keeps the folder readable.
            name = f"{package}-{source.name}" if written == 0 else f"{package}-{written}-{source.name}"
            shutil.copyfile(source, destination / name)
            written += 1
        if written:
            collected.append(package)
    if (target / "LICENSE.txt").is_file():
        shutil.copy(target / "LICENSE.txt", destination / "python-LICENSE.txt")
        collected.append("python")
    log(f"   {len(collected)} licences: {', '.join(sorted(collected))}")
    return collected


def module_path_entries(target: Path) -> list[str]:
    """The module search path of a self-contained embeddable runtime.

    All three entries are *relative* to the runtime root, which is what makes the same
    manifest correct on any machine:

    * `python313.zip` - the standard library, which the embeddable package keeps zipped
    * `.` - the runtime root itself, where the embeddable package puts its extension
      modules (`_ctypes.pyd`, `_ssl.pyd`, ...). This is the entry a "DLLs" folder would
      cover in a full installation, and leaving it out is exactly how a staged runtime
      ends up unable to import `ctypes` - and therefore SciPy.
    * `Lib/site-packages` - the pinned packages installed into the runtime.
    """
    entries = []
    if (target / "python313.zip").is_file():
        entries.append("python313.zip")
    # A full installation keeps them in `DLLs`; the embeddable package keeps them at the
    # root. Both layouts are accepted so the same script can stage either.
    if (target / "DLLs").is_dir():
        entries.append("DLLs")
    if any(target.glob("*.pyd")):
        entries.append(".")
    if (target / "Lib" / "site-packages").is_dir():
        entries.append("Lib/site-packages")
    return entries


def prune(site: Path) -> int:
    """Removes test suites and byte-code caches; returns the bytes reclaimed."""
    reclaimed = 0
    for path in sorted(site.rglob("*"), key=lambda item: len(item.parts), reverse=True):
        try:
            if path.is_dir() and path.name in PRUNE_DIRS:
                reclaimed += directory_size(path)
                shutil.rmtree(path)
            elif path.is_file() and path.suffix in PRUNE_SUFFIXES:
                reclaimed += path.stat().st_size
                path.unlink()
        except OSError:
            # A read-only or locked file is not worth failing the staging for.
            continue
    return reclaimed


def directory_size(path: Path) -> int:
    total = 0
    for item in path.rglob("*"):
        try:
            if item.is_file():
                total += item.stat().st_size
        except OSError:
            continue
    return total


def smoke_test(target: Path, entries: list[str]) -> dict[str, str]:
    """Runs the staged interpreter and imports every required package.

    This is the point of the whole script: if the assembled runtime cannot import NumPy,
    SciPy and Pillow on its own, no installer built from it is worth shipping.
    """
    script = (
        "import json, sys\n"
        "import numpy, scipy, PIL\n"
        "print(json.dumps({\n"
        "    'python': sys.version.split()[0],\n"
        "    'numpy': numpy.__version__,\n"
        "    'scipy': scipy.__version__,\n"
        "    'pillow': PIL.__version__,\n"
        "    'prefix': sys.prefix,\n"
        "    'path': sys.path,\n"
        "}))\n"
    )
    completed = subprocess.run(
        [str(target / "python.exe"), "-c", script], capture_output=True, text=True
    )
    if completed.returncode != 0:
        log(completed.stdout[-2000:])
        log(completed.stderr[-4000:])
        raise SystemExit("the staged runtime cannot import its pinned packages")

    report = json.loads(completed.stdout.strip().splitlines()[-1])
    # Every entry must live inside the runtime, or the bundle would depend on the machine
    # it was built on.
    outside = [
        entry
        for entry in report["path"]
        if entry and not Path(entry).resolve().is_relative_to(target.resolve())
    ]
    if outside:
        raise SystemExit(f"the staged runtime reaches outside itself: {outside}")
    return report


def write_manifest(target: Path, entries: list[str], report: dict) -> None:
    """Writes the manifest the app reads, with paths relative to the runtime root."""
    manifest = {
        "python": report["python"],
        "packages": {
            "numpy": report["numpy"],
            "scipy": report["scipy"],
            "pillow": report["pillow"],
        },
        # Torch is never installed and never needed for baseline acceptance.
        "optional": ["torch"],
        # Relative on purpose: the same manifest then describes the runtime wherever the
        # installer put it, instead of only on the machine that built it.
        "sys_path": entries,
        "foreign": [],
        "layout": "windows_embeddable",
        "staged_from": EMBED_ZIP,
    }
    path = target / "runtime-manifest.json"
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    log(f"   {path.name}: sys_path={entries}")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--target",
        type=Path,
        default=ROOT / "src-tauri" / "resources" / "python-runtime",
        help="where to assemble the runtime (default: the Tauri resource directory)",
    )
    parser.add_argument(
        "--cache",
        type=Path,
        default=ROOT / ".tmp" / "downloads",
        help="where the embeddable package is cached",
    )
    parser.add_argument(
        "--python",
        type=Path,
        default=ROOT / ".python-runtime" / "Scripts" / "python.exe",
        help="interpreter whose pip downloads the pinned wheels",
    )
    args = parser.parse_args(argv[1:])

    if not args.python.is_file():
        print(f"{args.python} is missing; run tools\\provision_python.cmd first")
        return 1

    log(f"staging the Python runtime into {args.target}")
    log("[1/7] embeddable interpreter")
    archive = args.cache / EMBED_ZIP
    download(EMBED_URL, archive)

    log("[2/7] extract")
    extract(archive, args.target)

    log("[3/7] pinned packages")
    install_packages(args.target / "Lib" / "site-packages", args.python)

    log("[4/7] licences")
    collect_licenses(args.target / "Lib" / "site-packages", args.target)

    log("[5/7] prune")
    reclaimed = prune(args.target / "Lib" / "site-packages")
    log(f"   reclaimed {reclaimed / 1e6:.1f} MB of tests and caches")

    log("[6/7] module search path")
    entries = module_path_entries(args.target)
    write_pth(args.target, entries)

    log("[7/7] verify")
    report = smoke_test(args.target, entries)
    write_manifest(args.target, entries, report)
    log(
        f"   python {report['python']} | numpy {report['numpy']} | "
        f"scipy {report['scipy']} | pillow {report['pillow']}"
    )

    total = directory_size(args.target)
    log(f"runtime ready: {args.target} ({total / 1e6:.0f} MB)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
