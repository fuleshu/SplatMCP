# Embedded Python generation

Procedural 3DGS authoring: an agent or the desktop user submits a compact Python recipe,
the app's embedded CPython builds the Gaussians with NumPy, and the result appears in the
existing PlayCanvas viewer as a new document revision.

This document is the developer guide for that path: how it is built, what the array
contract is, how to write a recipe, how jobs behave, and what is deliberately out of
scope. The formal structure, interaction and lifecycle are held as Adashi UML artifacts
attached to the SplatMCP software system.

## What it is not

- **No reconstruction pipeline.** Nothing here turns images or video into 3D. Python is a
  way to *construct* geometry from the agent's spatial reasoning. An image loaded with
  Pillow can supply reference colours; it never starts a generative model.
- **No security sandbox.** A recipe is local code execution. It can import anything the
  private runtime provides and touch the filesystem. Restricting imports is not offered as
  a security boundary, and no tool claims the interpreter is one.
- **No hard memory limit.** Code, parameter, log, output-point and owned-buffer budgets are
  enforced. NumPy or PyTorch allocations outside them are not: the tools report that
  honestly instead of advertising a process-wide cap.

## Where it lives

| Piece | Location |
| --- | --- |
| Array contract, validation, job registry, geometry, runtime discovery | `crates/splatmcp-python/src/` |
| PyO3 binding, `splatmcp` module, tracebacks | `crates/splatmcp-python/src/embedded/` |
| Bridge methods `python_*` | `crates/splatmcp-bridge/src/protocol.rs` |
| MCP tools | `crates/splatmcp-mcp/src/tools/python.rs`, registered in `crates/splatmcp-mcp/src/lib.rs` |
| App host, Tauri commands, document revisions | `src-tauri/src/python.rs`, `src-tauri/src/document.rs` |
| Generation panel | `ui/python-panel.js`, `ui/index.html` |
| Recipes | `examples/` |

`crates/splatmcp-python` has an `embedded` feature (on by default) that pulls in PyO3 and
rust-numpy. Everything else - the array contract, job states, geometry helpers, runtime
discovery - compiles and tests without an interpreter, so the rest of the workspace is
never blocked by a missing Python.

## The private runtime

The app never uses whatever `python.exe` is first on `PATH`.

| Context | Runtime location |
| --- | --- |
| Installed app | `<installation>/python-runtime`, placed there by the installer |
| Provisioned app | `<app data>/python-runtime`, for a machine without the installer |
| Development | `SPLATMCP_PYTHON_HOME`, e.g. the tested `.python-runtime` at the repository root |
| Build only | `<repository>/.python-runtime`, found by `crates/splatmcp-python/build.rs` and handed to PyO3 as `PYO3_PYTHON` |

Discovery tries `SPLATMCP_PYTHON_HOME`, then the bundled resources, then the app data
directory, and never `PATH`. `python_runtime_info` reports `source` as
`bundled_resource`, `application` or `development_override`, which is the first thing to
check when a packaged app behaves differently from a development build.

### The runtime the installer ships

`tools\build_installer.cmd` runs `tools\stage_python_runtime.py`, which assembles the
runtime into `src-tauri/resources/python-runtime` before Tauri packages it. The Windows
*embeddable* package is used rather than the development virtual environment, because a
virtual environment is a few kilobytes of configuration pointing at the base installation
its interpreter came from - it cannot be shipped to a machine with no Python.

The staged layout is self-contained, and every entry of its module search path is *relative
to the runtime*, which is what makes one manifest correct both where it was built and where
it was installed:

| Entry | Contents |
| --- | --- |
| `python313.zip` | the standard library |
| `.` | the extension modules (`_ctypes.pyd`, `_ssl.pyd`, ...) |
| `Lib/site-packages` | the pinned packages |

Staging also prunes test suites and byte-code caches (~60 MB), collects the redistributed
packages' licences into `THIRD-PARTY-LICENSES/`, and ends with a verification step that runs
the assembled interpreter and fails the build if it cannot import NumPy, SciPy and Pillow or
if any import path it reports points outside its own directory. A machine with no Python is
the acceptance case, and `tools\installer_check.py` proves it by installing the built
installer and driving the installed app with `SPLATMCP_PYTHON_HOME` unset.

```sh
tools\provision_python.cmd                # create .python-runtime and pin its packages
set SPLATMCP_PYTHON_HOME=C:\src\SplatMCP\.python-runtime
```

`provision_python.cmd` installs the pinned set and writes `runtime-manifest.json` into the
runtime (through `tools/write_runtime_manifest.py`), which is what `python_runtime_info`
reports before an interpreter is even started.

### The runtime is private on purpose

An embedded CPython otherwise inherits two things it should not: the installation it was
linked against, and the **invoking user's own `site-packages`**. This was observed, not
theorised: a recipe doing `import torch` succeeded on the development machine because a
CUDA PyTorch build sat in `%APPDATA%\Python\Python313\site-packages`, while the app
shipped no torch at all.

So the manifest records the interpreter's `sys.path` as provisioned - its standard library
plus this runtime's own `site-packages` - and the embedded layer installs exactly that list,
dropping every other entry. `foreign` in the manifest records what provisioning excluded,
and `python_runtime_info` reports a non-empty `foreign` list as an actionable warning
rather than passing it silently. When a runtime records no path at all (an ad-hoc
interpreter), the runtime's import directories are only prepended, and the report says so.

`crates/splatmcp-python/tests/embedded_generation.rs` asserts both directions: the manifest
records a path, and the interpreter running a job sees no per-user directory and no
`site-packages` outside the private runtime.

Pinned set (recorded in the manifest, not assumed):

| Package | Version | Role |
| --- | --- | --- |
| CPython | 3.13 | interpreter (64-bit Windows is the primary acceptance platform) |
| numpy | 2.3.3 | typed arrays, vectorised generation, seeded RNG |
| scipy | 1.16.2 | interpolation, splines, spatial queries, rotations |
| pillow | 11.3.0 | image loading, masks, reference colour sampling |
| torch | optional | never installed by default; no CUDA runtime or weights are downloaded |

When the runtime is missing the app still starts and the viewer, open, save and edit tools
keep working. Python operations report `python_runtime_unavailable` with the next step.

## The array contract

A recipe returns `splatmcp.batch(...)`. Every field is *activated* data, not a file-native
encoding, and validation runs on the raw values before any constructor can clamp them.

| Field | Shape | Unit / range |
| --- | --- | --- |
| `positions` | `(N, 3)` float32 | metres, document space |
| `scales` | `(N, 3)` float32 | strictly positive radii in metres (not PLY `ln(scale)`) |
| `rotations` | `(N, 4)` float32 | unit quaternion `(w, x, y, z)` |
| `colors` | `(N, 3)` float32 | linear RGB in `0..=1` |
| `opacity` | `(N,)` float32 | `0..=1` (not a logit) |
| `component_id`, `recipe`, `seed` | optional | provenance, recorded with the result |

Rejected with `invalid_batch`: inconsistent lengths, non-finite positions, non-positive
scales, zero-norm quaternions, colours or opacities outside `0..=1` beyond a `1e-3` float
tolerance. Quaternions are rescaled to unit length (the core model does the same), and
values inside the tolerance band are clamped. Nothing is silently repaired.

### Coordinate conventions

- **Document space** (what a batch holds, what PLY stores): right-handed, metres,
  `+X` right, `+Y` down, `+Z` forward.
- **Viewer space**: the PlayCanvas viewer imports a PLY and applies a 180° rotation about
  X, so viewer space is `(x, -y, -z)` of document space. That flip is its own inverse.
- `splatmcp.y_up_to_document(p)` and `splatmcp.document_to_viewer(p)` expose both
  directions, and `splatmcp.CONVENTIONS` carries the same text for a script to log.

`crates/splatmcp-python/src/conventions.rs` builds an **asymmetric axis fixture**: three
short arrows along `+X`, `+Y`, `+Z` in distinct colours plus an offset marker. A double
flip, a swapped axis or a mis-signed conversion changes its bounds, which is how the test
suite catches the class of bug that a symmetric model hides.

## Writing a recipe

```python
import numpy as np
import splatmcp

def generate(ctx):                      # the entry point is configurable
    n = int(ctx.params.get("count", 1000))
    rng = ctx.rng()                     # seeded from the job
    positions = np.stack([rng.normal_array(n, 0.0, 0.3) for _ in range(3)], axis=1).astype(np.float32)
    ctx.progress(0.5, "sampled")
    ctx.check_cancelled()               # cooperative cancellation
    return splatmcp.batch(
        positions=positions,
        scales=np.full((n, 3), 0.01, dtype=np.float32),
        rotations=np.tile(np.array([1, 0, 0, 0], dtype=np.float32), (n, 1)),
        colors=np.full((n, 3), 0.7, dtype=np.float32),
        opacity=np.ones(n, dtype=np.float32),
        component_id="cloud",
        seed=ctx.seed,
    )
```

### Iterative editing: `display` and `frame`

Two request flags exist because an agent editing a model and a user looking at one want
different things:

| Flag | `true` (default) | `false` |
| --- | --- | --- |
| `display` | the committed revision is published to the viewer | the revision is committed and the viewer is left alone, so the displayed model does not change |
| `frame` | the camera re-frames the new content | the camera keeps its position and orientation, so only the geometry that changed looks different |

`frame` is ignored when `display` is `false`, and both are reported back: a job that was not
displayed reports `display: not_requested`, so a caller can tell "committed quietly" apart
from "committed and shown". Publishing happens after the commit and only when `display` is
true, which is why the flag cannot be ignored in the middle of a commit.

`ctx` provides:

| Member | Purpose |
| --- | --- |
| `ctx.params` | the request's parameters, as plain Python data |
| `ctx.seed`, `ctx.job_id`, `ctx.max_points`, `ctx.entry_point` | job identity |
| `ctx.rng()` | deterministic RNG: `unit`, `range`, `jitter`, `quaternion`, `array`, `normal_array` |
| `ctx.check_cancelled()` | raises `splatmcp.Cancelled` at a checkpoint |
| `ctx.progress(fraction, message)` | bounded progress |
| `ctx.log(message, level)` | bounded log line; `print()` and stderr are captured too |
| `ctx.source()` | read-only snapshot of the document being edited, or `None` |
| `ctx.remaining_seconds` | seconds left before the deadline |

Module level helpers: `splatmcp.batch`, `merge`, `sample_surface`, `sample_curve`,
`frame_quaternion`, `axis_fixture`, `y_up_to_document`, `document_to_viewer`,
`check_cancelled`, `progress`, `log`.

`sample_surface` and `sample_curve` run in Rust, so a script does not re-derive orientation
maths: a surface sample's local `+X` follows the tangent and local `+Z` the normal, which
is what makes a sampled sheet read as a sheet.

Each job runs in a **fresh module namespace**. Imported libraries stay loaded, and a recipe
must not assume anything survives between jobs beyond that.

### Logs

A job's log holds its own output as well as its explicit lines. `sys.stdout` and `sys.stderr`
point at the job's log for the duration of the job, so `print(...)`, `sys.stdout.write(...)`
and anything a library writes to stderr all appear - stdout at `info`, stderr at `warning`,
in the order they were written, with a partially written line completed by the next write.
The streams are restored on every path out, including a script that raises or replaces
`sys.stdout` itself, so one job cannot swallow the next one's output.

Nothing is lost to a log bound either: the buffer drops its oldest lines and the reply sets
`log_truncated`, rather than silently trimming the middle.

The tests in `tests/embedded_generation.rs` serialise themselves on one interpreter: the
app runs one job at a time, so a test suite that ran several jobs at once would be testing
something the app never does.

### Reproducibility

The script hash and content hash identify a request. The same `request_id` with identical
content returns the original job; a reused `request_id` with different content is refused
with `request_conflict`. The seed plus the recipe reproduces the geometry exactly on the
same pinned runtime - full bitwise determinism across different BLAS or GPU builds is not
promised.

`export_path` writes a PLY plus a `<name>.ply.recipe.json` sidecar (script hash, parameters,
seed, runtime fingerprint, document identity). `Save` in the app writes the same sidecar
next to a saved file. A plain PLY round trip therefore preserves geometry but **not**
recipe, component or seed metadata; that lives in the sidecar.

## Jobs

```
queued -> running -> validating -> committing -> committed
   |         |            |            |
   |         |            |            +-> conflict   (the document moved on)
   |         |            +-> failed   (invalid arrays or over budget)
   |         +-> failed   (script error) / cancel_requested -> cancelled
   +-> cancelled         (cancelled before it started)
```

Display is a separate axis: `not_requested`, `pending`, `rendered` or `failed`. A committed
revision is not proof that the viewer rendered it, so `get_python_job` reports both, and a
display or export failure never changes the compute result - a retry cannot silently rerun a
generation that already succeeded.

**Cancellation is cooperative.** A queued job is cancelled immediately and never enters the
interpreter. A running job becomes `cancel_requested` and stops at its next checkpoint; a
long native NumPy or PyTorch call can delay that, `cancel_python_job` says whether execution
is still unwinding, and a late result is discarded rather than committed. No thread is
killed and the interpreter is never claimed to be free while an old script is unwinding.

**Commits are revision checked.** A job states the revision it believes it is editing. The
comparison and the swap happen under one lock, so two concurrent jobs for one revision
produce one commit and one explicit `document_conflict` rather than silent last-writer-wins.
Concurrent loads and manual edits advance the revision too.

Budgets: script and parameter bytes, point count, log lines and bytes, queue depth, and a
deadline (default and maximum). `python_runtime_info` reports them all so a caller can plan
before submitting.

## Tool surface

| Tool | Input | Output |
| --- | --- | --- |
| `python_runtime_info` | - | readiness, interpreter path and version, package versions, limits, busy and queue depth |
| `run_python_splat` | `request_id`, exactly one of `code` / `script_path`, `entry_point`, `params`, `seed`, `document_id` + `expected_revision` + `component_id`, `file_name`, `display`, `frame`, `export_path`, `deadline_seconds` | job receipt with the job id, state and content hash |
| `get_python_job` | `job_id`, optional `log_after` | state, progress, timings, revision, point count, bounds, export, structured error, logs and the next log cursor |
| `cancel_python_job` | `job_id` | the actual state, whether execution is still unwinding, and what happened |

Requests stay compact: a 500 000-Gaussian job is a recipe plus parameters, never point
data. Replies never carry point arrays or base64 PLY. The bridge frame limit is unchanged at
64 MiB, and the app publishes generated geometry to the viewer as binary bytes addressed by
revision, not through MCP.

## Submission to inspection, by hand

```sh
tools\python_check.cmd                       # generation tests against the private runtime
tools\test_workspace.cmd                     # the whole workspace, with MSVC + runtime set up

python tools\mcp_session.py --call python_runtime_info "{}"
python tools\mcp_session.py --call run_python_splat ^
  "{\"request_id\":\"demo-1\",\"script_path\":\"examples/height_field.py\",\"params\":{\"size\":64},\"seed\":3}"
python tools\mcp_session.py --call get_python_job "{\"job_id\":1}"
python tools\mcp_session.py --call get_screenshot "{\"width\":640}"
python tools\mcp_session.py --call run_python_splat ^
  "{\"request_id\":\"demo-2\",\"code\":\"import splatmcp\\ndef generate(ctx):\\n    return splatmcp.axis_fixture()\\n\",\"component_id\":\"axes\"}"
python tools\mcp_session.py --call cancel_python_job "{\"job_id\":2}"
```

In the app, the **Python generation** panel runs the same service: script path with
Load/Save, editor, params, seed, component, expected revision, export path, display and
frame flags, Run/Cancel, a live job line and bounded logs. MCP-started jobs appear there and
UI-started jobs are visible to `get_python_job`.

## Measurement notes

Measured on the development machine with the pinned runtime, debug build, one 500 000-point
NumPy recipe (`examples/noise_cloud.py`): roughly 2.6 s from submission to committed
revision, including array conversion, validation and PLY serialisation for the export. Cold
interpreter start-up plus the required package imports happen once, at app start, and are
what the first job would otherwise pay for.

Treat these as the starting measurements the milestone asks for, not as guarantees. Peak RSS,
packaged runtime size and GPU swap timing are not yet recorded.

## Packaging notes

`cargo tauri build` prints a warning that the bundle identifier `com.splatmcp.app` ends with
`.app`, which collides with the macOS bundle extension. It is expected here: Windows x64 is
the packaging target, and the identifier has to keep matching `APP_DIR_NAME` in
`crates/splatmcp-bridge/src/paths.rs`, which is where the app data directory gets its name.

The installer is unsigned, so Windows SmartScreen warns on first run. Signing is a
distribution decision and is not attempted here.

## Known limits

- One interpreter per process, so the runtime chosen at startup is the runtime for the
  session. Discovery never falls back to `PATH`.
- The document revision counter is process local and lives in the app. Task #12 owns
  durable identities, content hashes and immutable snapshots; the service's `DocumentTarget`
  seam is where that lands, without changing the generation code.
- Component replacement commits the whole batch the recipe returns; named components and
  stable selections are task #14's model.
- Journaling, previews and undo/redo are task #13; a Python commit is currently atomic but
  not undoable from the bridge.
- The private runtime is assembled for Windows x64. Other platforms work in development
  through `SPLATMCP_PYTHON_HOME`, but packaged provisioning is Windows-first.
