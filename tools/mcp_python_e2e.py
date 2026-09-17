#!/usr/bin/env python3
"""End-to-end check of embedded Python generation through the real MCP path.

One `splatmcp-mcp` stdio session drives the running desktop app, which hosts the single
generation service and the embedded interpreter. Nothing here is mocked: the recipes run
in the app's CPython, the commits go into the app's document, and the frames come from the
live PlayCanvas viewer.

It checks, in order:

1. the app is reachable and the runtime reports the private interpreter and pinned packages
2. a compact submission generates 250 000 gaussians, commits a revision and exports a PLY
   with its recipe sidecar
3. the exported PLY reloads with the same point count, and a frame of the result is written
4. a component edit uses `expected_revision` and produces a new revision
5. a stale `expected_revision` is refused with `document_conflict`, and the newer content
   survives
6. a slow script is cancelled with an honest state, and a late result is never committed
7. the job log is pollable with a cursor, and repeated submissions of the same request are
   deduplicated instead of rerun

Usage:

    tools\\build.cmd -p splatmcp -p splatmcp-mcp
    tools\\provision_python.cmd
    python tools/mcp_python_e2e.py

Exit status is non-zero if any step fails, and every step prints what it saw, so a failure
can be read rather than inferred.
"""

from __future__ import annotations

import base64
import json
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))

from mcp_session import McpSession  # noqa: E402  (a sibling helper of this script)

DATA_DIR = ROOT / ".tmp" / "python-e2e"
OUT_DIR = ROOT / ".tmp"
APP = ROOT / "target" / "debug" / "splatmcp.exe"
SERVER = ROOT / "target" / "debug" / "splatmcp-mcp.exe"
RUNTIME = ROOT / ".python-runtime"

# Patience for one job. The 250k recipe commits in a couple of seconds; the limit is
# generous so a slow machine reports a wrong result rather than a spurious timeout.
JOB_TIMEOUT_SECONDS = 240.0

NOISE_RECIPE = ROOT / "examples" / "noise_cloud.py"

AXIS_RECIPE = r"""
import splatmcp


def generate(ctx):
    return splatmcp.axis_fixture()
"""

FOREVER_RECIPE = r"""
import time


def generate(ctx):
    while True:
        ctx.check_cancelled()
        time.sleep(0.05)
"""

failures: list[str] = []


class Session:
    """One MCP session with job helpers on top."""

    def __init__(self) -> None:
        env = dict(os.environ)
        env["SPLATMCP_DATA_DIR"] = str(DATA_DIR)
        env["SPLATMCP_PYTHON_HOME"] = str(RUNTIME)
        self.session = McpSession(SERVER, env, ROOT)
        self.session.initialize()

    def call(self, tool: str, arguments: dict) -> dict:
        result = self.session.request("tools/call", {"name": tool, "arguments": arguments})
        text = ""
        images: list[dict] = []
        for block in result.get("content", []):
            if block.get("type") == "text":
                text = text or block.get("text", "")
            elif block.get("type") == "image":
                images.append(block)
        if result.get("isError"):
            raise RuntimeError(f"{tool} failed: {text}")
        return {"text": text, "json": json.loads(text) if text else None, "images": images}

    def submit(self, **arguments) -> dict:
        return self.call("run_python_splat", arguments)["json"]

    def job(self, job_id: int, log_after: int = 0) -> dict:
        return self.call(
            "get_python_job", {"job_id": job_id, "log_after": log_after}
        )["json"]

    def wait_rendered(self, job_id: int, timeout: float = 60.0) -> dict:
        """Waits until the viewer has acknowledged a committed revision.

        A committed document is not proof that it is displayed, so a frame captured before
        this returns can show the previous revision - or nothing at all. The app tracks
        display separately for exactly this reason.
        """
        deadline = time.monotonic() + timeout
        view: dict = {}
        while time.monotonic() < deadline:
            view = self.job(job_id)
            state = (view.get("display") or {}).get("state")
            if state == "rendered":
                return view
            if state == "failed":
                raise RuntimeError(f"the viewer failed to display revision: {view.get('display')}")
            time.sleep(0.2)
        raise RuntimeError(
            f"the viewer never acknowledged job {job_id}'s revision; last display state "
            f"{(view.get('display') or {}).get('state')!r}"
        )

    def wait(self, job_id: int) -> dict:
        """Polls a job until it reaches a terminal state, accumulating its log."""
        deadline = time.monotonic() + JOB_TIMEOUT_SECONDS
        logs: list[dict] = []
        cursor = 0
        view: dict = {}
        while time.monotonic() < deadline:
            view = self.job(job_id, cursor)
            cursor = view.get("log_cursor", cursor)
            logs.extend(view.get("logs", []))
            if view.get("state") in {"committed", "cancelled", "failed", "conflict"}:
                view["logs"] = logs
                return view
            time.sleep(0.25)
        raise RuntimeError(f"job {job_id} did not finish within {JOB_TIMEOUT_SECONDS}s")

    def close(self) -> None:
        self.session.close()


def check(label: str, condition: bool, detail: object = "") -> None:
    """Records a check, printing what was actually seen."""
    marker = "ok  " if condition else "FAIL"
    print(f"  [{marker}] {label}" + (f" -> {detail}" if detail != "" else ""))
    if not condition:
        failures.append(label)


def start_app() -> subprocess.Popen:
    env = dict(os.environ)
    env["SPLATMCP_DATA_DIR"] = str(DATA_DIR)
    env["SPLATMCP_PYTHON_HOME"] = str(RUNTIME)
    DATA_DIR.mkdir(parents=True, exist_ok=True)
    (DATA_DIR / "bridge.json").unlink(missing_ok=True)
    log = (OUT_DIR / "python-e2e-app.log").open("w", encoding="utf-8")
    process = subprocess.Popen([str(APP)], env=env, cwd=str(ROOT), stdout=log, stderr=log)
    # Give the app time to bind the bridge and warm the interpreter up.
    time.sleep(6.0)
    return process


def write_image(block: dict, name: str) -> Path:
    mime = block.get("mimeType") or block.get("mime_type") or "image/png"
    extension = "jpg" if "jpeg" in mime else "png"
    path = OUT_DIR / f"{name}.{extension}"
    path.write_bytes(base64.b64decode(block.get("data", "")))
    return path


def main() -> int:
    for required, hint in [
        (APP, "run tools\\build.cmd -p splatmcp -p splatmcp-mcp"),
        (SERVER, "run tools\\build.cmd -p splatmcp -p splatmcp-mcp"),
        (RUNTIME, "run tools\\provision_python.cmd"),
    ]:
        if not required.exists():
            print(f"missing {required}: {hint}")
            return 1

    app = start_app()
    session = None
    try:
        session = Session()
        print("\n1. runtime and the private interpreter")
        info = session.call("python_runtime_info", {})["json"]
        packages = {entry["name"]: entry for entry in info.get("packages", [])}
        check("the runtime is ready", info.get("ready") is True, info.get("error"))
        check(
            "the interpreter is the private runtime",
            str(RUNTIME).lower() in info.get("interpreter", "").lower(),
            info.get("interpreter"),
        )
        check("numpy is available", packages.get("numpy", {}).get("available") is True)
        check(
            "torch is reported missing rather than present",
            packages.get("torch", {}).get("available") is False,
        )
        check(
            "the budgets are declared",
            info.get("limits", {}).get("max_points", 0) >= 500_000,
            info.get("limits"),
        )

        print("\n2. a compact submission generates 250k gaussians")
        export = OUT_DIR / "python-e2e-noise.ply"
        export.unlink(missing_ok=True)
        receipt = session.submit(
            request_id="e2e-noise",
            script_path=str(NOISE_RECIPE.relative_to(ROOT)).replace("\\", "/"),
            params={"count": 250000},
            seed=42,
            file_name="python-e2e-noise.ply",
            export_path=".tmp/python-e2e-noise.ply",
        )
        check("the submission is accepted", receipt.get("state") == "queued", receipt)
        view = session.wait(receipt["job_id"])
        check("the job committed", view.get("state") == "committed", view.get("error"))
        check("it produced 250 000 gaussians", view.get("point_count") == 250000)
        check("it reports a revision", isinstance(view.get("revision"), int), view.get("revision"))
        rendered = session.wait_rendered(receipt["job_id"])
        check(
            "the viewer acknowledged the generated revision",
            rendered.get("displayed_revision") == view.get("revision"),
            (rendered.get("displayed_revision"), view.get("revision")),
        )
        check(
            "the script's log was captured",
            any("generated 250000" in line["text"] for line in view["logs"]),
        )
        check("no display failure was reported", view.get("display", {}).get("state") != "failed", view.get("display"))
        check("the export wrote a PLY", export.is_file() and export.stat().st_size > 0, export)
        sidecar = export.with_name(export.name + ".recipe.json")
        check("the recipe sidecar was written", sidecar.is_file(), sidecar)
        check("a compute failure was not reported", view.get("error") is None, view.get("error"))
        revision = view.get("revision")
        # The noise job above displayed without an explicit wait, because its frame was not
        # captured; from here on every capture waits for the acknowledgement first.

        print("\n3. the exported PLY reloads and the viewer shows it")
        reloaded = session.call("load_splat", {"path": str(export.relative_to(ROOT)).replace("\\", "/")})["json"]
        check("the exported PLY parses", reloaded.get("point_count") == 250000, reloaded.get("point_count"))
        session.call("set_camera", {"fit": True})
        frame = session.call("get_screenshot", {"width": 640})
        shot = write_image(frame["images"][0], "python-e2e-cloud")
        check("a frame of the generated cloud was captured", shot.is_file(), shot)
        check(
            "the frame is not an empty render",
            len(frame["images"][0].get("data", "")) > 2000,
            f"{len(frame['images'][0].get('data', ''))} base64 chars",
        )
        reported = frame["json"]
        check("the frame reports its size", reported.get("width") == 640, reported.get("width"))

        print("\n4. a component edit uses its expected revision")
        # An edit quotes the revision it starts from, which is what a caller reads from
        # `splat_info` - loading the exported file above advanced the revision, so the
        # current one is read again here rather than remembered.
        identity = session.call("splat_info", {"points": 0})["json"]
        current_revision = (identity.get("document") or {}).get("revision")
        check("splat_info reports the document revision", isinstance(current_revision, int), identity.get("document"))
        edit = session.submit(
            request_id="e2e-axes",
            code=AXIS_RECIPE,
            component_id="axes",
            expected_revision=current_revision,
            seed=0,
        )
        edited = session.wait(edit["job_id"])
        check("the edit committed", edited.get("state") == "committed", edited.get("error"))
        check("it named its component", edited.get("component_id") == "axes", edited.get("component_id"))
        check(
            "it advanced the revision",
            edited.get("revision") == current_revision + 1,
            edited.get("revision"),
        )
        # Wait for the viewer to acknowledge the new revision before framing it: capturing
        # earlier can photograph the previous geometry.
        rendered = session.wait_rendered(edit["job_id"])
        check(
            "the viewer acknowledged the new revision",
            rendered.get("display", {}).get("state") == "rendered",
            rendered.get("display"),
        )
        check(
            "the acknowledgement names the revision that was committed",
            rendered.get("displayed_revision") == edited.get("revision"),
            (rendered.get("displayed_revision"), edited.get("revision")),
        )
        session.call("set_camera", {"fit": True})
        axes_frame = session.call("get_screenshot", {"width": 480, "format": "jpeg", "quality": 85})
        axes_shot = write_image(axes_frame["images"][0], "python-e2e-axes")
        check("a frame of the edited revision was captured", axes_shot.is_file(), axes_shot)

        # One view is not enough for the axis fixture: +Z points along the default camera's
        # view direction, so it projects onto the origin and cannot be told apart from +X
        # there. Orbiting separates all three coloured arrows, which is what makes the
        # fixture a real check of the conventions rather than a picture of two axes.
        session.call("set_camera", {"azimuth": 55, "elevation": 22, "distance": 2.4})
        orbit_frame = session.call("get_screenshot", {"width": 480, "format": "jpeg", "quality": 85})
        orbit_shot = write_image(orbit_frame["images"][0], "python-e2e-axes-orbit")
        check("a second angle of the fixture was captured", orbit_shot.is_file(), orbit_shot)
        check(
            "the fixture is small and asymmetric, not an empty scene",
            len(orbit_frame["images"][0].get("data", "")) > 1500,
            f"{len(orbit_frame['images'][0].get('data', ''))} base64 chars",
        )

        print("\n5. a stale revision is refused instead of overwriting")
        stale = session.submit(
            request_id="e2e-stale",
            code=AXIS_RECIPE,
            component_id="axes",
            expected_revision=current_revision,
        )
        stale_view = session.wait(stale["job_id"])
        check("the stale edit conflicted", stale_view.get("state") == "conflict", stale_view.get("state"))
        check(
            "the conflict is reported as a document conflict",
            (stale_view.get("error") or {}).get("code") == "document_conflict",
            stale_view.get("error"),
        )
        current = session.call("splat_info", {"points": 0})["json"]
        check("the newer content survived", current.get("point_count") == edited.get("point_count"), current.get("point_count"))

        print("\n6. a slow script is cancelled with an honest state")
        slow = session.submit(request_id="e2e-slow", code=FOREVER_RECIPE, display=False)
        # Let it start, then cancel while it is running.
        started = time.monotonic()
        running = None
        while time.monotonic() - started < 60:
            running = session.job(slow["job_id"])
            if running.get("state") == "running":
                break
            time.sleep(0.2)
        check("the slow script is running", running.get("state") == "running", running.get("state"))
        cancelled = session.call("cancel_python_job", {"job_id": slow["job_id"]})["json"]
        print(f"  cancel reply: {cancelled}")
        final = session.wait(slow["job_id"])
        check("the job ended cancelled", final.get("state") == "cancelled", final.get("state"))
        check(
            "the cancellation is reported as its own code",
            (final.get("error") or {}).get("code") == "job_cancelled",
            final.get("error"),
        )
        check("nothing was published for the cancelled job", final.get("display", {}).get("state") == "not_requested")
        after = session.call("splat_info", {"points": 0})["json"]
        check(
            "the document kept the committed revision",
            after.get("point_count") == edited.get("point_count"),
            after.get("point_count"),
        )

        print("\n7. request identity: deduplication, and a reused id with new content")
        repeat = session.submit(
            request_id="e2e-axes",
            code=AXIS_RECIPE,
            component_id="axes",
            expected_revision=current_revision,
            seed=0,
        )
        check("an identical retry returns the original job", repeat.get("deduplicated") is True, repeat)
        check("it is the same job id", repeat.get("job_id") == edit["job_id"], repeat.get("job_id"))
        cursor_view = session.job(edit["job_id"], final.get("log_cursor", 0))
        check("a log cursor returns only newer lines", cursor_view.get("logs") == [], cursor_view.get("logs"))

        # The same id with different content is refused: it would otherwise silently rerun
        # a job the caller believes already ran.
        try:
            session.submit(
                request_id="e2e-axes",
                code=AXIS_RECIPE,
                component_id="other",
                expected_revision=current_revision,
                seed=0,
            )
            check("a reused request id with new content is refused", False, "it was accepted")
        except (RuntimeError, SystemExit) as error:
            # The MCP server reports a refused call as a JSON-RPC error, which the session
            # helper raises as SystemExit; a tool-level failure comes back as RuntimeError.
            # Both are checked the same way, by the code in the message.
            check(
                "a reused request id with new content is refused",
                "request_conflict" in str(error),
                str(error),
            )

        print("\n8. the tool surface stays compact")
        listing = session.session.request("tools/list").get("tools", [])
        encoded = json.dumps(listing)
        check("all twelve tools are exposed", len(listing) == 12, len(listing))
        # The same per-tool budget the crate's unit test enforces. The raw JSON here is
        # slightly larger than the server's compact serialisation, so the check is a little
        # stricter than the one it mirrors, which is the safe direction.
        budget = len(listing) * 1600
        check(
            "the listing stays within its budget",
            len(encoded) <= budget,
            f"{len(encoded)} bytes of {budget}",
        )
        check(
            "the python tools are documented",
            all(any(tool["name"] == name for tool in listing) for name in
                ["python_runtime_info", "run_python_splat", "get_python_job", "cancel_python_job"]),
        )
    finally:
        if session is not None:
            session.close()
        app.terminate()
        try:
            app.wait(timeout=10)
        except subprocess.TimeoutExpired:
            app.kill()

    print()
    if failures:
        print(f"python_e2e: {len(failures)} check(s) failed:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print("python_e2e: all checks passed")
    print(f"frames and the exported PLY are in {OUT_DIR}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
