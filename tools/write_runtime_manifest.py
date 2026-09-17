"""Writes runtime-manifest.json for a provisioned Python runtime.

The manifest does more than list versions:

* `sys_path` is the interpreter's module search path with the *user's own* site-packages
  removed. The embedded interpreter installs exactly this list, which is what stops a
  generation job from importing a package the app never shipped.
* `foreign` records whatever had to be dropped, so the removal is visible rather than
  silent.

Usage:

    python tools/write_runtime_manifest.py <runtime-dir> [interpreter]

`tools\\provision_python.cmd` calls it after installing the pinned packages.
"""

import json
import pathlib
import site
import sys
import sysconfig


def describe(interpreter_prefix: str) -> dict:
    """Builds the manifest for the interpreter currently running this script."""
    import numpy
    import PIL
    import scipy

    user_site = site.getusersitepackages()
    user_path = pathlib.Path(user_site) if user_site else None
    kept = []
    dropped = []
    for entry in sys.path:
        if not entry:
            continue
        path = pathlib.Path(entry)
        if user_path is not None and path == user_path:
            dropped.append(entry)
        else:
            kept.append(entry)

    return {
        "python": sys.version.split()[0],
        "packages": {
            "numpy": numpy.__version__,
            "scipy": scipy.__version__,
            "pillow": PIL.__version__,
        },
        # Torch is never installed by default and never needed for baseline acceptance.
        "optional": ["torch"],
        "sys_path": kept,
        "foreign": dropped,
        "base_prefix": sys.base_prefix,
        "platform": sysconfig.get_platform(),
        "interpreter_prefix": interpreter_prefix,
    }


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__)
        return 2
    runtime = pathlib.Path(argv[1]).resolve()
    if not runtime.is_dir():
        print(f"{runtime} is not a directory")
        return 1

    manifest = describe(sys.prefix)
    target = runtime / "runtime-manifest.json"
    target.write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(json.dumps(manifest, indent=2))
    print(f"wrote {target}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
