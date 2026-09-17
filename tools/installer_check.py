#!/usr/bin/env python3
"""Verifies the built SplatMCP installer the way a user would use it.

It installs the NSIS installer silently into a scratch directory, checks the installed
layout, and then drives the installed application over the real MCP stdio path to prove
that generation works **without any development runtime**:

* the installed app is started with `SPLATMCP_DATA_DIR` pointing at a scratch directory and
  `SPLATMCP_PYTHON_HOME` **unset**, so it can only use the runtime inside the installation
* `python_runtime_info` must report `source: bundled_resource` and the installed path
* a NumPy recipe must generate, commit and render, which exercises the bundled interpreter,
  NumPy, the array conversion and the viewer swap end to end

Nothing here uses `.python-runtime` or `target/`; the only inputs are the installer, the
installed MCP server, and the private runtime that came out of the installer.

Usage:

    tools\\build_installer.cmd            # produces the installer first
    python tools/installer_check.py
    python tools/installer_check.py --keep    # leave the installation in place to inspect
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))

from mcp_session import McpSession  # noqa: E402  (a sibling helper of this script)

NOISE_RECIPE = "examples/noise_cloud.py"
JOB_TIMEOUT_SECONDS = 300.0

failures: list[str] = []


def normalise(path: str | Path) -> str:
    """A path comparable regardless of the Windows verbatim prefix and separator style."""
    text = str(path)
    if text.startswith("\\\\?\\"):
        text = text[4:]
    return text.replace("/", "\\").rstrip("\\").lower()


def check(label: str, condition: bool, detail: object = "") -> None:
    """Records a check, printing what was actually seen."""
    marker = "ok  " if condition else "FAIL"
    print(f"  [{marker}] {label}" + (f" -> {detail}" if detail != "" else ""))
    if not condition:
        failures.append(label)


def find_installer() -> Path | None:
    """The most recently built NSIS installer."""
    candidates = sorted(
        (ROOT / "target" / "release" / "bundle" / "nsis").glob("*-setup.exe"),
        key=lambda path: path.stat().st_mtime,
        reverse=True,
    )
    return candidates[0] if candidates else None


def install(installer: Path, target: Path) -> None:
    """Runs the installer silently.

    Tauri's NSIS installer accepts `/S` for a silent install and `/D=` for the destination,
    which is what makes an unattended check possible. `/D=` must be last and unquoted.
    """
    if target.exists():
        shutil.rmtree(target)
    target.mkdir(parents=True)
    print(f"  installing into {target}")
    completed = subprocess.run(
        [str(installer), "/S", f"/D={target}"],
        capture_output=True,
        text=True,
        timeout=900,
    )
    if completed.returncode != 0:
        print(completed.stdout[-2000:])
        print(completed.stderr[-2000:])
        raise SystemExit(f"the installer exited with code {completed.returncode}")
    # The NSIS process returns before the last file is flushed.
    for _ in range(60):
        if list(target.glob("*.exe")):
            break
        time.sleep(0.5)


def app_process(app: Path, data_dir: Path, log: Path) -> subprocess.Popen:
    """Starts the installed app with only its own installation visible.

    `SPLATMCP_PYTHON_HOME` is deliberately absent: a packaged app must not depend on the
    machine it is installed on, and this is what proves it.
    """
    env = {key: value for key, value in os.environ.items() if key != "SPLATMCP_PYTHON_HOME"}
    env["SPLATMCP_DATA_DIR"] = str(data_dir)
    handle = log.open("w", encoding="utf-8")
    return subprocess.Popen([str(app)], env=env, cwd=str(app.parent), stdout=handle, stderr=handle)


def session(server: Path, data_dir: Path) -> McpSession:
    env = {key: value for key, value in os.environ.items() if key != "SPLATMCP_PYTHON_HOME"}
    env["SPLATMCP_DATA_DIR"] = str(data_dir)
    started = McpSession(server, env, ROOT)
    started.initialize()
    return started


def call(started: McpSession, tool: str, arguments: dict) -> dict:
    result = started.request("tools/call", {"name": tool, "arguments": arguments})
    text = ""
    images: list[dict] = []
    for block in result.get("content", []):
        if block.get("type") == "text":
            text = text or block.get("text", "")
        elif block.get("type") == "image":
            images.append(block)
    if result.get("isError"):
        raise RuntimeError(f"{tool} failed: {text}")
    return {"json": json.loads(text) if text else None, "images": images}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--keep", action="store_true", help="leave the installation in place")
    parser.add_argument(
        "--target",
        type=Path,
        default=ROOT / ".tmp" / "installed",
        help="where to install (default: .tmp\\installed)",
    )
    args = parser.parse_args()

    installer = find_installer()
    if installer is None:
        print("no installer found; run tools\\build_installer.cmd first")
        return 1

    print(f"1. installer\n  {installer.name} ({installer.stat().st_size / 1e6:.0f} MB)")
    install(installer, args.target)

    print("\n2. installed layout")
    app = args.target / "splatmcp.exe"
    server = args.target / "mcp" / "splatmcp-mcp.exe"
    runtime = args.target / "python-runtime"
    interpreter = runtime / "python.exe"
    manifest = runtime / "runtime-manifest.json"
    check("the application is installed", app.is_file(), app.name)
    check("the MCP server is installed next to it", server.is_file(), server.name)
    check("the Python runtime is installed", interpreter.is_file())
    check("the runtime carries its manifest", manifest.is_file())
    check(
        "the runtime carries its licences",
        (runtime / "THIRD-PARTY-LICENSES").is_dir(),
        (runtime / "THIRD-PARTY-LICENSES"),
    )
    check(
        "the installed runtime is not the build tree's",
        not str(runtime.resolve()).startswith(str((ROOT / ".python-runtime").resolve())),
        runtime.resolve(),
    )

    print("\n3. the installed runtime stands alone")
    # This is the check that matters for the bundling: an interpreter inside the
    # installation, importing every pinned package without the development runtime.
    env = {key: value for key, value in os.environ.items() if key != "SPLATMCP_PYTHON_HOME"}
    probe = subprocess.run(
        [
            str(interpreter),
            "-c",
            "import json, sys\n"
            "import numpy, scipy, PIL\n"
            "print(json.dumps({'python': sys.version.split()[0], 'numpy': numpy.__version__,\n"
            "                  'scipy': scipy.__version__, 'pillow': PIL.__version__, 'path': sys.path}))\n",
        ],
        capture_output=True,
        text=True,
        env=env,
    )
    if probe.returncode != 0:
        print(probe.stderr[-3000:])
        check("the installed interpreter imports its packages", False, "see stderr above")
    else:
        report = json.loads(probe.stdout.strip().splitlines()[-1])
        check("the installed interpreter imports its packages", True, report["python"])
        check("numpy is present", report["numpy"] == "2.3.3", report["numpy"])
        check("scipy is present", report["scipy"] == "1.16.2", report["scipy"])
        check("pillow is present", report["pillow"] == "11.3.0", report["pillow"])
        outside = [
            entry
            for entry in report["path"]
            if entry and not Path(entry).resolve().is_relative_to(runtime.resolve())
        ]
        check("every import path is inside the installation", not outside, outside)

    print("\n4. the installed application serves generation")
    data_dir = ROOT / ".tmp" / "installed-appdata"
    if data_dir.exists():
        shutil.rmtree(data_dir)
    data_dir.mkdir(parents=True)
    log = ROOT / ".tmp" / "installed-app.log"
    process = app_process(app, data_dir, log)
    started = None
    try:
        time.sleep(8.0)
        started = session(server, data_dir)

        info = call(started, "python_runtime_info", {})["json"]
        check("the app reports the runtime ready", info.get("ready") is True, info.get("error"))
        check(
            "the runtime comes from the installation's resources",
            info.get("source") == "bundled_resource",
            info.get("source"),
        )
        check(
            "the reported interpreter is the installed one",
            normalise(info.get("interpreter", "")).startswith(normalise(runtime)),
            info.get("interpreter"),
        )
        check(
            "the reported root is the installed runtime",
            normalise(info.get("root", "")) == normalise(runtime),
            info.get("root"),
        )

        # A real generation job through the installed stack.
        export = ROOT / ".tmp" / "installed-cloud.ply"
        export.unlink(missing_ok=True)
        receipt = call(
            started,
            "run_python_splat",
            {
                "request_id": "installed-1",
                "code": (
                    "import numpy as np\n"
                    "import splatmcp\n"
                    "\n"
                    "def generate(ctx):\n"
                    "    n = 250000\n"
                    "    rng = ctx.rng()\n"
                    "    positions = np.stack([rng.normal_array(n, 0.0, 0.35) for _ in range(3)], axis=1).astype(np.float32)\n"
                    "    ctx.log('built %d gaussians in the installed app' % n)\n"
                    "    return splatmcp.batch(\n"
                    "        positions=positions,\n"
                    "        scales=np.stack([rng.array(n, 0.002, 0.02) for _ in range(3)], axis=1).astype(np.float32),\n"
                    "        rotations=np.tile(np.array([1.0, 0.0, 0.0, 0.0], dtype=np.float32), (n, 1)),\n"
                    "        colors=np.stack([rng.array(n, 0.0, 1.0) for _ in range(3)], axis=1).astype(np.float32),\n"
                    "        opacity=rng.array(n, 0.4, 1.0).astype(np.float32),\n"
                    "        component_id='installed',\n"
                    "        recipe='installer_check',\n"
                    "        seed=ctx.seed,\n"
                    "    )\n"
                ),
                "seed": 7,
                "file_name": "installed.ply",
                "export_path": str(export),
            },
        )["json"]
        job_id = receipt["job_id"]

        deadline = time.monotonic() + JOB_TIMEOUT_SECONDS
        cursor = 0
        view: dict = {}
        logs: list[dict] = []
        while time.monotonic() < deadline:
            view = call(started, "get_python_job", {"job_id": job_id, "log_after": cursor})["json"]
            cursor = view.get("log_cursor", cursor)
            logs.extend(view.get("logs", []))
            if view.get("state") in {"committed", "cancelled", "failed", "conflict"}:
                break
            time.sleep(0.25)
        for line in logs:
            print(f"     [{line['level']}] {line['text']}")
        check("the job committed", view.get("state") == "committed", view.get("error"))
        check("it produced 250 000 gaussians", view.get("point_count") == 250000, view.get("point_count"))
        check("the exported PLY was written", export.is_file() and export.stat().st_size > 0, export)
        check("the viewer rendered the revision", (view.get("display") or {}).get("state") == "rendered", view.get("display"))

        # A frame proves the viewer really shows the generated geometry.
        call(started, "set_camera", {"fit": True})
        frame = call(started, "get_screenshot", {"width": 640})
        check("a frame of the generated cloud was captured", bool(frame["images"]))
        if frame["images"]:
            shot = ROOT / ".tmp" / "installed-cloud.png"
            shot.write_bytes(base64.b64decode(frame["images"][0].get("data", "")))
            check("the frame is not empty", shot.stat().st_size > 2000, f"{shot.stat().st_size} bytes")

        check(
            "the app log names the bundled runtime",
            "bundled_resource" in log.read_text(encoding="utf-8", errors="replace"),
            log,
        )
    finally:
        if started is not None:
            started.close()
        process.terminate()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.kill()

    if not args.keep:
        shutil.rmtree(args.target, ignore_errors=True)

    print()
    if failures:
        print(f"installer_check: {len(failures)} check(s) failed:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print("installer_check: all checks passed")
    print(f"the installer at {installer.name} is usable on a machine without Python")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
