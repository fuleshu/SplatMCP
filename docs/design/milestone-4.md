# Milestone 4 design — an MCP client drives the SplatMCP desktop app

Status: in progress (per-task sections at the end).
Owner: agent run 2026-09-15.

> Note: the project design store (Adashi `adashi_design save`) rejected every write
> during this run (`call outcome is uncertain`, revision never advanced), so the formal
> design for this milestone is kept here as well. The Adashi task list mirrors this plan
> (tasks 1-9).

## 1. Why the bridge exists

An MCP client spawns `splatmcp-mcp.exe` and talks MCP over stdio. The desktop app is a
separate, long-lived GUI process the user is looking at. A stdio server can only be
reached by whoever spawned it, so the app cannot host the tool surface, and the tool
surface cannot reach the window directly.

The app therefore hosts a small request/response service on `127.0.0.1`, publishes its
port and a random token in `bridge.json` inside the app data directory, and the MCP
server connects to it on demand. The document of record stays in the app (`AppState`),
so there is never a second, divergent copy of the splat.

```
MCP client --stdio--> splatmcp-mcp --loopback TCP--> SplatMCP app --webview event--> viewer
                            |                              |
                            +-- splatmcp-core (authoring, PLY)
                                                           +-- bridge.json in app data dir
```

Start-up order:

1. The MCP client spawns `splatmcp-mcp.exe` (stdio).
2. On the first tool call that needs the app, the MCP server reads `bridge.json`. If it
   is missing or stale it launches `SplatMCP.exe` and waits for a fresh descriptor.
3. The app binds the bridge at start-up, publishes `bridge.json`, and retires it on exit.

## 2. Bridge protocol (v1)

One JSON object per line over a loopback `TcpStream`; frames are capped at 64 MiB so a
captured PNG fits but runaway input does not. Every frame carries the token; the first
frame on a connection must be `hello`.

| method | params | result |
| --- | --- | --- |
| `hello` | `{client, protocol}` | `{app_version, protocol, pid}` |
| `app_ping` | - | `{app_version, pid, uptime_ms}` |
| `viewer_status` | - | `{viewer_ready, loaded, point_count, canvas_width, canvas_height, camera}` |
| `viewer_get_camera` | - | `CameraState` |
| `viewer_set_camera` | `CameraRequest` | `CameraState` |
| `viewer_capture` | `{width?, height?, format?, quality?, camera?}` | `{mime_type, data_base64, width, height, camera}` |
| `viewer_load_ply` | `{ply_base64, file_name?, frame?}` | `ViewerStatus` |
| `document_get_ply` | - | `{ply_base64, file_name?, point_count}` |

`CameraRequest` accepts either an explicit `position`/`target`, or orbit values
(`azimuth`, `elevation`, `distance`), or `fit: true` to frame the whole splat.

Failure shape: `{id, ok:false, error:"..."}`. The client maps three cases to actionable
tool errors: no descriptor (`AppNotRunning`), rejected token (`Unauthorized`), wrong
protocol version (`UnsupportedProtocol`).

## 3. MCP tool surface (target, milestone 4)

| tool | purpose |
| --- | --- |
| `create_splat` | build a splat from parameters or explicit points; optionally write a `.ply` and display it |
| `edit_splat` | apply ordered edit ops to the displayed splat or a `.ply`; optionally save and display |
| `load_splat` | display an existing `.ply` in the app |
| `splat_info` | compact stats (count, bounds, opacity range, mean colour) for the displayed splat or a path |
| `set_camera` | move the viewer camera (position/target, orbit, or fit) |
| `get_screenshot` | render the current camera and return the frame as image content |

Descriptions stay under ~240 characters, properties are terse, and enums replace free
text so a tool listing stays cheap in a context.

## 4. Task designs

### T1 - `splatmcp-bridge` crate (done)

Files: `crates/splatmcp-bridge/src/{lib,paths,protocol,wire,server,client}.rs`, tests in
`tests/round_trip.rs`.

- `paths`: `app_data_dir()`, `bridge_descriptor_path()`, `settings_path()`,
  `ensure_app_data_dir()`. Resolves `LOCALAPPDATA` / `XDG_DATA_HOME` / `HOME` plus
  `com.splatmcp.app`; `SPLATMCP_DATA_DIR` overrides it so tests and scripted runs stay out
  of the user profile. Both processes use this module, so no Tauri path API is needed on
  the MCP side.
- `protocol`: `BridgeDescriptor` (atomic write, `retire` that only removes its own
  descriptor, protocol version check), `Method`, `Request`, `Response`, `CameraState`,
  `CameraRequest`, `CaptureRequest`, `CaptureResult`, `LoadPlyRequest`, `ViewerStatus`,
  `Hello{Request,Result}`, `PingResult`.
- `wire`: `write_message`, `read_message`, `read_line_limited` with an explicit byte cap
  enforced while filling the buffer.
- `server`: `BridgeServer::bind()` (loopback, OS-assigned port, 256-bit token derived from
  `RandomState`), `serve(handler, timeout)` returning `BridgeService`, a `Handler` trait,
  handshake validation, one thread per connection with a 16-connection cap, and a
  shutdown that unblocks `accept`.
- `client`: `BridgeClient::connect` (handshake), `call`, `call_typed`, `ping`, `call_once`
  with per-method timeouts; `BridgeError::is_app_missing` tells callers when to start the
  app.

Verification: `cargo test -p splatmcp-bridge` - 17 unit + 8 integration tests covering
round trips, wrong token, unknown method, malformed frames, oversized frames, handler
errors, slow-handler timeouts, wrong protocol version, concurrent clients and a stopped
server.

### T2 - the app hosts the bridge (done)

Files: `src-tauri/src/{main,bridge,viewer,document,paths}.rs`.

- `document.rs`: the single document of record (`Document { path, splat }` plus
  `AppState`). Bytes are parsed and validated before they replace the displayed splat, so
  a malformed push never clears the window.
- `viewer.rs`: request id allocation, the pending-request table, the
  `splat://bridge-request` event and the timeout that turns a missing answer into an
  actionable error. 15 s for state requests, 45 s for a capture.
- `bridge.rs`: `AppBridge` implements `splatmcp_bridge::Handler`. `app_ping` and document
  methods are answered in-process; viewer methods are forwarded. `viewer_load_ply`
  parses the payload, stores the document and then asks the viewer to display the very
  same bytes. `bridge::start` binds, publishes and serves; `bridge::retire` removes the
  descriptor on exit.
- `paths.rs`: `documents_dir()` for splats that arrived over the bridge, plus the
  settings and descriptor paths.
- `main.rs`: manages `AppState`, `ViewerState` and `BridgeHostState`; adds the
  `bridge_respond` command; uses `tauri::RunEvent::Exit` to shut the bridge down and
  retire the descriptor. The app no longer spawns `splatmcp-mcp.exe`: a stdio server
  spawned by the app has no client attached, so the MCP client spawns it instead.

Behaviour under a stripped environment: this sandbox runs commands with no environment
variables at all, so `app_data_dir()` also consults `HOMEDRIVE`+`HOMEPATH` and the app
reports a missing data directory instead of refusing to start. Live checks set
`SPLATMCP_DATA_DIR`.

Verification: `cargo test --workspace` (9 app tests among 49) plus a live run:
`app_ping` answered over the real bridge (`{app_version, pid, uptime_ms}`), and
`viewer_status` failed with the intended message while the viewer side was still missing.

### T3 - viewer bridge (done)

Files: `ui/{bridge,camera,capture}.js`, changes in `ui/{viewer,main}.js`.

- `camera.js`: `cameraState(viewer)` (position, target from the controls' focus point,
  fov) and `applyCamera(viewer, request)` handling `fit`, explicit `position`, orbit
  (`azimuth`/`elevation`/`distance`) and target-only moves, with fov clamped to 10-120 and
  the fov actually applied to the camera component so a capture reflects it. Pure array
  vectors, so the module has no engine dependency; `viewer.placeCamera` converts them.
- `capture.js`: `captureImage(viewer, {width, height, format, quality})` renders a frame,
  resizes the canvas when a frame size is requested (both edges or one edge, keeping the
  aspect), reads it back with `toDataURL`, and restores the canvas plus re-renders in a
  `finally`, mirroring the reference app's `captureThumbnail`.
- `bridge.js`: `ViewerBridge` listens for `splat://bridge-request`, executes
  `viewer_status`, `viewer_get_camera`, `viewer_set_camera`, `viewer_capture`,
  `viewer_load_ply`, and answers through `bridge_respond`. Requests are serialised so a
  slow capture cannot overtake an earlier camera move.
- `viewer.js`: `open({frame})` can now load without re-framing, `hasSplat()`,
  `pointCount()` (from the PLY header, no engine internals) and `canvasSize()`.
- `main.js`: the viewer is created and the bridge started at page load, so a tool can ask
  about readiness before the user has opened anything.

Verification: `tools/live_check.cmd` against the real app. Results: status before load
`{viewer_ready: true, loaded: false, point_count: 0, canvas 1600x947}`; `viewer_load_ply`
of `external_grid.ply` returned `loaded: true, point_count: 189` with a framed camera;
`set_camera --fit` restored that frame exactly after an orbit; `set-camera --azimuth 35
--elevation 18 --distance 4` moved the eye as expected; `viewer_capture --width 800`
returned a 175 KB PNG (device pixels 1000x592 at a 1.25 pixel ratio), and the captured
frame was inspected and shows the rendered splat.

### T4 - the MCP side: link and launcher (done)

Files: `crates/splatmcp-mcp/src/{lib,bridge,app_launch}.rs`, `tools/`, `tests/app_link.rs`.

- `bridge.rs`: `AppLink` caches one connection, attaches on first use, retries exactly once
  when a cached connection went stale (the user restarted the app), and rewrites every
  failure into instructions: missing app, stale token, protocol mismatch, timeout, and a
  viewer refusal passed through untouched. A descriptor from a different protocol version
  is reported instead of being treated as a missing app.
- `app_launch.rs`: resolves the desktop executable from `SPLATMCP_APP`, then the last
  published descriptor, then next to the MCP binary, and starts it detached. Without a
  candidate the caller is told to start the app manually.
- `lib.rs`: a single tool router holds `splatmcp_status` (server version, bridge protocol,
  whether an app is attached) so a listing stays at a few hundred bytes.
- `tools/mod.rs`: shared reply helpers (`round3`, `SplatSummary`, `point_json`).

Verification: `cargo test -p splatmcp-mcp` - 11 unit tests and one integration test that
drives `AppLink` against a real `BridgeServer` (attach, reuse, typed decode, viewer
refusal, stale token, missing app, protocol mismatch, corrupt descriptor, launcher
timeout). Live: `tools/mcp_session.py` performed a real MCP handshake over stdio and
called `splatmcp_status` against a running app; the whole `tools/list` payload is 280
bytes for one tool.

### T5 - viewer tools: set_camera, get_camera, get_screenshot (done)

Files: `crates/splatmcp-mcp/src/tools/viewer.rs`, tool entry points in `lib.rs`.

- `CameraInput` is one optional-field schema shared by `set_camera` and
  `get_screenshot.camera`, so the orbit/explicit/fit vocabulary exists once in a listing.
- `CaptureInput` carries `width`, `height`, `format`, `quality` and the optional camera.
- `set_camera`/`get_camera`/`screenshot` translate the input into bridge requests
  (`camera_params`, `capture_params`) and decode typed replies, so the translation and the
  reply shape are unit tested without an app or an MCP session.
- `get_screenshot` answers with an MCP image block plus a one-line summary (frame size,
  mime type, byte count, camera used).
- Reply size: every tool answers with typed serialisation (`tool_json`) rather than a
  `serde_json::Value`, because widening an `f32` through `Value` prints
  `0.009999999776482582`; `round3` also folds negative zero.

Verification: `cargo test -p splatmcp-mcp` (18 unit tests, including exact JSON replies
for camera and capture summaries). Live over a real MCP stdio session with the app
running and `external_grid.ply` displayed: `get_camera` returned
`{"position":[0.0,0.0,-0.01],...,"fov":60.0}`, `set_camera {"fit":true}` returned the framed
camera, an orbit request moved the eye as expected, and `get_screenshot` returned an image
block: `{"width":320,"height":200,...}` and `{"width":900,"height":532,...}` - the requested
frame size is honoured exactly - with the PNG inspected and showing the rendered splat.

### T6 - splat authoring core and `create_splat` (done)

Files: `crates/splatmcp-core/src/authoring.rs`, `crates/splatmcp-mcp/src/tools/author.rs`,
`create_splat` in `lib.rs`.

- `authoring.rs`: a seeded SplitMix64 `Rng` (with its own tests) and `SplatParams` +
  `Shape` builders for sphere, cube, plane, line, shell, ring and grid. Sphere sampling
  scales a uniform direction by `cbrt(u)` so it needs exactly three samples and stays
  uniform; shell, jitter, colour variation and random orientation are all applied per
  point. `validate()` rejects anything that cannot be built, and `MAX_POINTS` (2,000,000)
  bounds one call.
- `author.rs`: `CreateInput` (flat, every field optional), `PointInput` for explicit
  points, shape-name parsing with aliases and a message that lists the valid names, the
  mapping onto `SplatParams`, file writing that requires an extension, and
  `display_splat`, which sends PLY bytes over the bridge.
- `SplatReply` (flattened summary + `path` + `displayed`) serialises the reply as a typed
  struct, so `f32` fields keep short forms and the payload stays small.
- The viewer now settles after a load (three rendered frames) before reporting success,
  which is what makes "create then screenshot" show the splat; captures also render twice.

Verification: `cargo test -p splatmcp-core -p splatmcp-mcp` (27 + 26 tests) plus live MCP
sessions: `create_splat` of a shell rendered as a blue shell, a grid rendered as a yellow
sheet, two identical calls written to different files compared byte-for-byte with `fc /b`
(identical, so generation is deterministic and seed-sensitive), and `display:false`
returning `"displayed":false` without touching the window.

### T7 - edit operations and `edit_splat` / `load_splat` / `splat_info` (done)

Files: `crates/splatmcp-core/src/edit.rs`, `crates/splatmcp-mcp/src/tools/edit.rs`, tool
entry points in `lib.rs`.

- `edit.rs`: `Selection` (box in/out, colour range, opacity floor, radius ceiling, first N)
  and `EditOp` for translate, rotate, scale, set_radius, adjust_color, set_color,
  set_opacity, duplicate, remove and merge. `apply` returns an `OpReport`
  (`affected`, `remaining`), validates the operation before touching anything, and refuses
  a selection that matches nothing instead of silently doing nothing. Rotation rotates
  positions *and* orientations around a centre; scale multiplies positions relative to a
  centre and the radii per axis. `apply_all` stops at the first failure.
- `edit.rs` (MCP side): `EditInput`/`EditOpInput` with one `Factor` type that accepts a
  number or `[x, y, z]`, `within`/`outside` boxes as 6 numbers (or 2 for a line), the
  `user-friendly` errors that name the op index and the missing field, and
  `resolve_source`, which reads the splat from the app (`viewer`, via the new
  `document_get_ply` request read back through the bridge), a `.ply` path, or `new`.
- `edit_splat` deliberately loads without re-framing, so editing keeps the caller's
  current view; `load_splat` frames the splat; `splat_info` can return the first `n`
  points for detailed inspection.
- Replies are typed (`EditReply`, `StepReport`, `InfoReply`, `PointOut`), which keeps
  floats short: a two-point sample prints `"scale":[0.06,0.06,0.02]`, not
  `0.06000000238418579`.

Verification: `cargo test -p splatmcp-core -p splatmcp-mcp` (44 + 41 tests). Live MCP
sessions: a plane recoloured inside a box selection (1200 of 4000 points) and translated,
with the captured frame showing the red band exactly where the box was; and the full
round trip `load_splat` (189 points) → `splat_info` → a four-step `edit_splat`
(set_color, duplicate, set_opacity on the first 189, set_radius) → `.tmp/edited.ply` →
`splat_info` of that file, which reported the same 378 points, centre `[0, 0.768, 0]` and
mean colour as the in-memory result, with the captured frame showing the duplicate grid
above the recoloured original.

### T8 - settings.json window geometry (done)

Files: `src-tauri/src/settings.rs`, bootstrap in `main.rs`; live check in
`tools/settings_check.cmd`.

- `settings.json` holds a schema `version`, `bounds` (x, y, width, height) and `mode`
  (`normal`, `maximized`, `fullscreen`), written pretty-printed next to `bridge.json`.
- `Settings::sanitized` replaces unusable bounds (too small, non-finite, or absurdly far
  off-screen) with the configured default and, in that case, forces `normal` mode: a
  window restored off-screen, or fullscreen on bounds that cannot be placed, would look
  like the app failed to start.
- `normal_bounds` captures the position and size the way Tauri applies them
  (`set_position` takes the outer corner, `set_size` the inner size), which is what makes
  the restored window land exactly where it was left.
- `WindowSettings` throttles writes to one per 400 ms while the user drags or resizes, and
  forces a write on `CloseRequested`; a failed write is reported and never fatal.

Verification: `cargo test -p splatmcp` (14 tests, including round trip, broken/missing
file, old schema version, implausible bounds and the throttle). Live with
`tools/settings_check.cmd`: the app started at its default geometry, the window was moved
to (140, 90) and resized to 900x640 through the Win32 API, the app was closed, and the
stored file read
`{"version":1,"bounds":{"x":140.0,"y":90.4,"width":885.6,"height":602.4},"mode":"normal"}`;
on the next start the same window rect (x=140, y=90, w=900, h=640) was restored.







