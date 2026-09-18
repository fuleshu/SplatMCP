# SplatMCP

A 3D Gaussian Splat (3DGS) editor with an MCP server attached.

- **Viewer / editor shell** — a Tauri 2 desktop app that opens `.ply` Gaussian splats, renders them with PlayCanvas, and re-exports them.
- **MCP server** — a standalone `stdio` binary (`splatmcp-mcp`) built on the official [Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk) (`rmcp`), so any MCP client / agent harness can create, edit, look at and capture splats.
- **Bridge** — `splatmcp-bridge`, a loopback request/response service the app hosts and the MCP server calls. It is what lets a stdio server move the camera of a window it did not start.
- **Core library** — `splatmcp-core`, the in-memory splat model, PLY import/export, splat authoring and edit operations (fixed colour, SH degree 0 only; higher SH bands are dropped, never stored). Its versioned [Gaussian data and coordinate contract](docs/design/gaussian-contract.md) defines the axes, units, quaternion order, colour space and validation every other part follows, and its [document identity, revisions and snapshots](docs/design/document-identity.md) define what a document id, a revision and a snapshot handle mean.
- **Python generation** — `splatmcp-python`, an embedded CPython executor the app hosts. An agent or the desktop panel submits a compact NumPy recipe, the app builds the Gaussians locally and shows them as a new document revision. See [docs/design/python-generation.md](docs/design/python-generation.md).

Everything is one Cargo workspace; there is no npm/Node build step (the frontend is plain ES modules with a vendored PlayCanvas build).

## Repository layout

```
Cargo.toml                    workspace root (members: crates/*, src-tauri)
crates/splatmcp-core/         splat model, PLY read/write, authoring, edit ops
crates/splatmcp-bridge/       loopback protocol, server, client, app-data paths
crates/splatmcp-mcp/          MCP server (lib: splatmcp_mcp, bin: splatmcp-mcp)
crates/splatmcp-python/       embedded CPython executor, array contract, job service
src-tauri/                    desktop app (bin: splatmcp), tauri.conf.json, capabilities
ui/                           frontend: index.html, main.js, python-panel.js, viewer/camera/capture/bridge.js
examples/                     Python generation recipes
docs/design/                  the formal design of each milestone
tools/                        scripted clients and live checks (Python + cmd)
```

| Crate | Package | Targets |
| --- | --- | --- |
| `crates/splatmcp-core` | `splatmcp-core` | library `splatmcp_core`, integration test `interop` |
| `crates/splatmcp-bridge` | `splatmcp-bridge` | library `splatmcp_bridge`, integration test `round_trip` |
| `crates/splatmcp-mcp` | `splatmcp-mcp` | library `splatmcp_mcp`, binary `splatmcp-mcp`, integration test `app_link` |
| `crates/splatmcp-python` | `splatmcp-python` | library `splatmcp_python` (feature `embedded`, on by default), integration test `embedded_generation` |
| `src-tauri` | `splatmcp` | binary `splatmcp` (the desktop app) |

## How the two processes fit together

An MCP client spawns `splatmcp-mcp.exe` and speaks MCP over stdio. The desktop app is a
separate, long-lived GUI process, so the two are connected like this:

```
MCP client --stdio--> splatmcp-mcp --loopback TCP--> SplatMCP app --webview event--> PlayCanvas viewer
                           |                                |
                           +-- splatmcp-core                +-- bridge.json / settings.json
                               (authoring, edits, PLY)          in the app data directory
```

1. At start-up the app binds the bridge on `127.0.0.1` (OS-assigned port), generates a
   token, and publishes them in `bridge.json` inside the app data directory
   (`%LOCALAPPDATA%\com.splatmcp.app` on Windows, `$XDG_DATA_HOME`/`~/.local/share`
   elsewhere). `SPLATMCP_DATA_DIR` overrides the location.
2. On the first tool call that needs the app, the MCP server reads `bridge.json` and
   connects. If no app is running it starts one (found next to the server binary, or via
   `SPLATMCP_APP`) and waits for a fresh descriptor.
3. Viewer requests (camera, capture) are forwarded into the webview and answered by
   `ui/bridge.js`; document requests are answered by the app, which holds the single copy
   of the displayed splat.
4. On exit the app removes its descriptor, so a stale file never points at a dead port.

The desktop process is also the only process that links CPython: it hosts one generation
service and one interpreter, and MCP-started jobs and panel-started jobs go through it
together. The MCP server never embeds a second interpreter, and generated geometry never
travels through MCP: the app publishes a revision identity, and the viewer fetches exactly
those bytes as binary data.

## Tools

| Tool | What it does |
| --- | --- |
| `edit_batch` | apply several edit steps as **one transaction**: all of them commit as a single new revision or nothing changes. `dry_run` reports a preview (affected counts, before/after bounds, memory estimate) without committing; `preview_id` commits that candidate later and is refused if the document moved on. `operation_id` makes a retry after a lost response safe: an identical resend replays the recorded receipt, different content under the same id is refused. Undo/redo and the history are shared with the window |
| `edit_history` | `status` reports undo/redo availability and the retained steps of the displayed document; `undo` and `redo` commit a **new** revision each, restoring geometry and component membership. A new edit clears the redo stack |
| `splat_components` | named components and stable selections: `list`, `create`, `rename`, `remove`, `transform` (declares an explicit local frame; anisotropic gaussians are transformed through their covariance, and singular or reflecting frames are refused), `members` (bind a selection to a component), `apply_transform` (transform those members as a committed edit) and `select` (a revision-bound handle with count, bounds and a bounded sample) |
| `create_splat` | build a splat from a shape (`sphere`, `cube`, `plane`, `line`, `shell`, `ring`, `grid`) or explicit points, optionally write a `.ply`, and show it |
| `edit_splat` | apply ordered edit steps (`translate`, `rotate`, `scale`, `set_radius`, `adjust_color`, `set_color`, `set_opacity`, `duplicate`, `remove`, `merge`) with an optional box, sphere, attribute, **component**, point-id or saved-selection target. The displayed document is edited through the same transaction as `edit_batch` - stable component/point ids, one revision, components and undo history preserved, `operation_id` makes a retry safe - while a `.ply` or `new` source is a detached buffer that refuses document-only targets instead of ignoring them |
| `load_splat` | display an existing `.ply` and frame it; the import is strict, so a file that needs repair is refused with indexed diagnostics unless `repair: true` accepts it and the reply reports every change |
| `splat_info` | document id, revision, point count, bounds, mean colour, opacity range, scale/colour distributions, contract diagnostics and buffer sizes; reads the displayed document as bounded metadata, and shows the first *n* gaussians when asked |
| `set_camera` | move the camera: `fit`, an explicit position, or orbit values |
| `get_camera` | report position, target and field of view |
| `get_screenshot` | render the window and return the frame as an image, optionally after moving the camera |
| `splatmcp_status` | server version, bridge protocol, and whether an app is attached |
| `python_runtime_info` | Python readiness, interpreter and package versions, and the budgets jobs are held to |
| `run_python_splat` | run a NumPy recipe in the app's embedded interpreter and show the result; returns a job id immediately |
| `get_python_job` | state, progress, timings, revision, point count, bounds, export, structured error and logs of a job |
| `cancel_python_job` | ask a job to stop; says whether execution is still unwinding |

Every reply is compact JSON with three decimals, and a test keeps the listing within its
context budget (`the_tool_listing_stays_within_its_context_budget`).

The Python tools take a *recipe*, never geometry: a 500 000-Gaussian job is a few hundred
bytes of request. See [docs/design/python-generation.md](docs/design/python-generation.md)
for the job states and cancellation semantics. The array contract and coordinate
conventions it uses are the shared [Gaussian contract](docs/design/gaussian-contract.md).

## Prerequisites

- **Rust** (stable) with Cargo. The workspace uses `edition = "2024"` and `resolver = "3"`, so you need a recent stable toolchain (`rustup update stable`).
- **Tauri 2 CLI** — `cargo install tauri-cli --version "^2" --locked` (needed to build the installer or to run the desktop app through the Tauri CLI).
- **Platform build dependencies for Tauri 2** (see the [Tauri prerequisites guide](https://v2.tauri.app/start/prerequisites/)):
  - Windows: Microsoft C++ Build Tools (MSVC linker) + WebView2 runtime.
  - macOS: Xcode command line tools.
  - Linux: `webkit2gtk-4.1`, `libayatana-appindicator`, and the usual `build-essential`/pkg-config set.
- **Python 3** — for the scripted clients in `tools/`, and the private generation runtime.
  The app does not use whatever `python` is on `PATH`: run `tools\provision_python.cmd` once
  to assemble `.python-runtime` (pinned CPython + NumPy + SciPy + Pillow), or point
  `SPLATMCP_PYTHON_HOME` at a tested interpreter. The runtime manifest records the
  interpreter's own `sys.path`, and the app installs exactly that, so a recipe cannot
  import a package the app never shipped - including the invoking user's own
  `site-packages`. Without a runtime, the viewer, open, save and edit tools still work and
  Python tools report `python_runtime_unavailable`.
- **Node.js is not required.** `ui/` is static; `tauri.conf.json` points `build.frontendDist` at `../ui` directly.

## Build

```sh
cargo build                                 # whole workspace
cargo build -p splatmcp-mcp                 # MCP server -> target/debug/splatmcp-mcp(.exe)
cargo build --release                       # everything, for harness use
tools\test_workspace.cmd                    # tests, with the MSVC toolchain and Python runtime set up
```

On this machine `cargo` needs the Visual Studio toolchain on the path and `PYO3_PYTHON`
pointing at the private runtime; `tools\cargo_env.cmd` sets both, and `tools\build.cmd`,
`tools\test_workspace.cmd` and `tools\clippy.cmd` delegate to it.

## Installer

```sh
tools\build_installer.cmd
```

Produces `target\release\bundle\nsis\SplatMCP_<version>_x64-setup.exe`, a per-user NSIS
installer (no administrator prompt) that contains:

| Installed | Contents |
| --- | --- |
| `splatmcp.exe` | the desktop app and the PlayCanvas viewer |
| `mcp\splatmcp-mcp.exe` | the MCP server, so a client can point at the installed copy |
| `python-runtime\` | a self-contained CPython 3.13 with NumPy, SciPy and Pillow |
| `python-runtime\THIRD-PARTY-LICENSES\` | the licences of the redistributed packages |

The bundling is reproducible from a clean checkout: `tools\stage_python_runtime.py`
downloads the pinned Windows *embeddable* package, installs the pinned wheels into it,
prunes test suites and byte-code caches, writes the runtime's module search path and
`runtime-manifest.json`, and then **verifies** that the assembled interpreter can import
every pinned package from inside its own directory and reaches nothing outside it.

An installed app therefore needs no Python on the machine. It resolves its runtime in this
order, and never from `PATH`:

1. `SPLATMCP_PYTHON_HOME`, when set (development)
2. the runtime the installer placed in the application's resources
3. a runtime provisioned into the app data directory

```sh
python tools\installer_check.py        # install silently and verify the installed copy
```

`tools\installer_check.py` installs the built installer into `.tmp\installed`, runs the
installed interpreter with `SPLATMCP_PYTHON_HOME` **unset**, and drives the installed app
over the real MCP stdio path: it must report `source: bundled_resource`, generate 250 000
Gaussians, commit a revision, render it and capture a frame. Everything it checks is what a
user would do, so a green run is evidence the installer is usable on a machine without
Python.

## Run

```sh
./target/debug/splatmcp        # the desktop app (Windows: target\debug\splatmcp.exe)
```

Start the app first if you want to watch the edits happen; the MCP server starts it for
you otherwise. Then point an MCP client at the server:

```sh
./target/debug/splatmcp-mcp    # speaks MCP over stdin/stdout
```

Note that `ui/` is embedded into the binary at build time: after editing the frontend you
must rebuild the app, not just restart it.

## Tests

```sh
tools\test_workspace.cmd         # workspace tests with the MSVC + runtime environment set up
cargo test                      # the same, when your shell already has both
cargo test -p splatmcp-core     # model, PLY, authoring, edit operations
cargo test -p splatmcp-bridge   # protocol, framing, server/client round trips
cargo test -p splatmcp-mcp      # tool surface, schemas, MCP-side link
cargo test -p splatmcp          # app: document state, revisions, viewer dispatch, settings.json
cargo test -p splatmcp-python   # array contract, job states, geometry, embedded generation
tools\python_check.cmd           # the embedded generation tests against the private runtime
cargo fmt --check
cargo clippy --all-targets
```

The generation tests skip themselves with a printed message when no private runtime is
present, so the workspace still tests on a machine without Python.

### Live checks

The commands below drive the real app; they set `SPLATMCP_DATA_DIR` to a folder inside
`.tmp/` so they never touch your real settings.

```sh
tools\live_check.cmd                     # bridge: load a PLY, move the camera, capture a frame
tools\settings_check.cmd                 # settings.json: move, resize, close, restore
python tools\bridge_client.py status     # one bridge request by hand
python tools\mcp_session.py --list       # tool listing and its context size
python tools\mcp_session.py --call create_splat "{\"shape\":\"sphere\",\"count\":2000}"
tools\e2e_check.cmd                      # the whole story, end to end
tools\python_check.cmd                   # generation: array contract and embedded executor
tools\build_installer.cmd                # build the NSIS installer
python tools\installer_check.py          # install it silently and verify the installed copy

python tools\mcp_session.py --call python_runtime_info "{}"
python tools\mcp_session.py --call run_python_splat ^
  "{\"request_id\":\"demo-1\",\"script_path\":\"examples/height_field.py\",\"params\":{\"size\":64},\"seed\":3}"
python tools\mcp_session.py --call get_python_job "{\"job_id\":1}"
```

## MCP setup in an agent harness

- **Server name:** `splatmcp`
- **Transport:** `stdio`
- **Command:** absolute path to the built binary — `…/SplatMCP/target/release/splatmcp-mcp.exe` on Windows, `…/SplatMCP/target/release/splatmcp-mcp` on macOS/Linux
- **Args:** none
- **Env:** none required (`SPLATMCP_DATA_DIR` and `SPLATMCP_APP` exist for unusual setups)

```json
{
  "mcpServers": {
    "splatmcp": {
      "command": "C:\\src\\SplatMCP\\target\\release\\splatmcp-mcp.exe",
      "args": []
    }
  }
}
```

| Harness | Location / key |
| --- | --- |
| Claude Desktop | `claude_desktop_config.json` (Settings → Developer → Edit Config), `mcpServers` |
| Codex CLI | `~/.codex/config.toml`, as `[mcp_servers.splatmcp]` with `command = "…"` and `args = []` |
| Cursor | `.cursor/mcp.json` or Settings → MCP, `mcpServers` |
| VS Code / Copilot | `.vscode/mcp.json`, `servers` |
| Gemini CLI | `.gemini/settings.json`, `mcpServers` |

### Verify it manually

```sh
python tools\mcp_session.py --call splatmcp_status "{}"
```

Expected: `{"app":{"attached":false},...}` before an app is running, and
`{"app":{"attached":true,"pid":…,"version":"0.1.0"},…}` once one is.

## App data files

| File | Contents |
| --- | --- |
| `bridge.json` | port, token, pid, protocol version and executable of the running app; written at start-up, removed on exit |
| `settings.json` | window geometry (`bounds`, `mode`, schema `version`), restored on start and tracked while you move or resize the window |
| `python-runtime/` | the application private CPython runtime and its `runtime-manifest.json`; never taken from `PATH` |

## Status

Milestones 1-4 are done: the workspace builds, the viewer works, the bridge connects the
MCP server to the live window, the eight M4 tools work end to end, and window geometry is
persisted.

The Gaussian contract is versioned and enforced: raw values are validated before any
clamping constructor at every boundary, PLY import is strict by default and only repairs a
file when the caller asks for it, reporting every changed value and every dropped attribute
in the reply, and inspecting a 500 000-Gaussian document returns bounded metadata without
transferring geometry. See [docs/design/gaussian-contract.md](docs/design/gaussian-contract.md).

The embedded Python generation milestone is implemented: the app hosts one CPython
interpreter and one generation service, the four `python_*` tools and the desktop panel
submit the same jobs, a compact NumPy recipe produces 500 000 Gaussians in about two
seconds on the development machine, commits are revision checked and atomic, cancellation
is cooperative and reported honestly, and generated geometry reaches the viewer as
revision-addressed binary bytes. Verified by the workspace test suite plus
`tools\python_check.cmd`; see
[docs/design/python-generation.md](docs/design/python-generation.md) for what is measured
and what is deliberately not promised.

The Windows installer is built and verified: `tools\build_installer.cmd` produces a per-user
NSIS installer with the app, the MCP server and a self-contained CPython 3.13 (NumPy, SciPy,
Pillow), and `tools\installer_check.py` installs it silently and proves the installed copy
generates and renders geometry with no Python on the machine.

Still open:

- **Peak memory is not yet measured.** The generation doc records what has been measured and
  what has not, rather than inventing guarantees.
- **The bridge protocol version is still 1.** The Python methods are additive, and a test
  pins that they cannot need a viewer, so both halves keep talking to each other without a
  version bump; capability negotiation for future shape changes is not implemented.

- **The installer is Windows-only and unsigned.** NSIS on Windows x64 is the target; there
  is no code-signing step, so Windows SmartScreen warns on first run.
- **SPZ is out of scope** by an explicit earlier decision: the only splat file format is
  `.ply`. No SPZ codec exists in this repository.
- The bridge is a plain loopback socket protected by a token in the user's app data
  directory; any local process that can read that file can drive the viewer.
- One document at a time: the app displays a single splat, and `viewer_load_ply` replaces
  it.
- **Document identity and revision are process local.** The app mints a session-stamped
  document id, advances one monotonic revision per accepted change, resolves exact revisions
  through bounded retention with explicit pin/release, and commits every change under a
  compare-and-swap check - so a stale handle fails with `snapshot_expired` and a stale edit
  with `document_conflict` instead of overwriting newer work. Identities are not durable
  across restarts.
- **Edit transactions are process local.** Edit batches commit atomically through a shared
  transaction service (`crates/splatmcp-core/src/transaction.rs`): a candidate is built from an
  exact snapshot, validated, then swapped in once under compare-and-swap, so a failure at any
  step leaves the document unchanged. Undo/redo commit new revisions from a bounded per-document
  history (8 steps / 256 MiB, oldest evicted first) and are only offered for revisions the
  service itself produced. Idempotency receipts are bounded in memory (32 receipts, 15 minutes),
  so after a restart an old operation id is *unknown*: the tool says so instead of replaying a
  destructive edit.
- **A commit says `published`, not `done`, until the window renders it.** The app announces the
  exact revision over `splat://edit-revision`, with a monotonic publication token, and the window
  fetches *those* bytes by document id and revision. Order is decided by token rather than arrival,
  so a slow fetch or slow stage for an older revision can neither replace newer geometry nor be
  acknowledged or labelled as displayed; only the viewer's acknowledgement turns the receipt's
  display outcome into `done`. A retry replays the recorded receipt - its document,
  revision, point count and side effects - rather than reporting whatever is displayed now, and a
  preview commit is retry-safe once it carries an `operation_id`.
- **Component metadata and point identities are process local too.** `AuthoringSet` holds opaque
  component ids, stable point ids and membership beside the gaussian buffer. A revision produced
  outside the transaction service (a file replace, a Python job) rebuilds that layer, which is
  reported as `rebuilt` so a caller learns why its ids changed. A save writes a versioned
  `.authoring.json` sidecar next to the PLY carrying document id, revision, the artifact checksum,
  each component's frame, its members (as identities *and* rows) and the **exported revision's**
  gaussian count - never the file size. Native Save writes both files from one snapshot, and a load
  carries the absolute source path beside the display name, because a basename is a label and not a
  location. Reopening a file restores the
  metadata whose checksum and gaussian count match those bytes, rebuilding every component with
  fresh ids at the recorded rows; a sidecar that does not match is refused **with a reason the
  reply carries**, never attached by file name. A plain PLY export keeps its "geometry only"
  guarantee. A resolved selection is published to the window (`splat://selection`) and drawn as a
  marker layer over the document, so the sidebar, a tool call and the viewport show the same
  gaussians.

## License

Apache-2.0 — see [LICENSE](LICENSE). The vendored PlayCanvas build under `ui/vendor/playcanvas/` is covered by `ui/vendor/playcanvas/LICENSE.playcanvas`.
