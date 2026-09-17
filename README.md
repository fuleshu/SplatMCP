# SplatMCP

A 3D Gaussian Splat (3DGS) editor with an MCP server attached.

- **Viewer / editor shell** — a Tauri 2 desktop app that opens `.ply` Gaussian splats, renders them with PlayCanvas, and re-exports them.
- **MCP server** — a standalone `stdio` binary (`splatmcp-mcp`) built on the official [Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk) (`rmcp`), so any MCP client / agent harness can create, edit, look at and capture splats.
- **Bridge** — `splatmcp-bridge`, a loopback request/response service the app hosts and the MCP server calls. It is what lets a stdio server move the camera of a window it did not start.
- **Core library** — `splatmcp-core`, the in-memory splat model, PLY import/export, splat authoring and edit operations (fixed colour, SH degree 0 only; higher SH bands are dropped, never stored).

Everything is one Cargo workspace; there is no npm/Node build step (the frontend is plain ES modules with a vendored PlayCanvas build).

## Repository layout

```
Cargo.toml                    workspace root (members: crates/*, src-tauri)
crates/splatmcp-core/         splat model, PLY read/write, authoring, edit ops
crates/splatmcp-bridge/       loopback protocol, server, client, app-data paths
crates/splatmcp-mcp/          MCP server (lib: splatmcp_mcp, bin: splatmcp-mcp)
src-tauri/                    desktop app (bin: splatmcp), tauri.conf.json, capabilities
ui/                           frontend: index.html, main.js, viewer/camera/capture/bridge.js
docs/design/                  the formal design of each milestone
tools/                        scripted clients and live checks (Python + cmd)
```

| Crate | Package | Targets |
| --- | --- | --- |
| `crates/splatmcp-core` | `splatmcp-core` | library `splatmcp_core`, integration test `interop` |
| `crates/splatmcp-bridge` | `splatmcp-bridge` | library `splatmcp_bridge`, integration test `round_trip` |
| `crates/splatmcp-mcp` | `splatmcp-mcp` | library `splatmcp_mcp`, binary `splatmcp-mcp`, integration test `app_link` |
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

## Tools

| Tool | What it does |
| --- | --- |
| `create_splat` | build a splat from a shape (`sphere`, `cube`, `plane`, `line`, `shell`, `ring`, `grid`) or explicit points, optionally write a `.ply`, and show it |
| `edit_splat` | apply ordered edit steps (`translate`, `rotate`, `scale`, `set_radius`, `adjust_color`, `set_color`, `set_opacity`, `duplicate`, `remove`, `merge`) to the displayed splat, a `.ply`, or a new empty one, each with an optional box/attribute selection |
| `load_splat` | display an existing `.ply` and frame it |
| `splat_info` | point count, bounds, mean colour, opacity range, optionally the first *n* gaussians |
| `set_camera` | move the camera: `fit`, an explicit position, or orbit values |
| `get_camera` | report position, target and field of view |
| `get_screenshot` | render the window and return the frame as an image, optionally after moving the camera |
| `splatmcp_status` | server version, bridge protocol, and whether an app is attached |

Every reply is compact JSON with three decimals, and the whole listing stays under a
16 KB budget that a test enforces (`the_tool_listing_stays_within_its_context_budget`).

## Prerequisites

- **Rust** (stable) with Cargo. The workspace uses `edition = "2024"` and `resolver = "3"`, so you need a recent stable toolchain (`rustup update stable`).
- **Tauri 2 CLI** — `cargo install tauri-cli --version "^2" --locked` (only needed to run/package the desktop app).
- **Platform build dependencies for Tauri 2** (see the [Tauri prerequisites guide](https://v2.tauri.app/start/prerequisites/)):
  - Windows: Microsoft C++ Build Tools (MSVC linker) + WebView2 runtime.
  - macOS: Xcode command line tools.
  - Linux: `webkit2gtk-4.1`, `libayatana-appindicator`, and the usual `build-essential`/pkg-config set.
- **Python 3** — only for the scripted clients in `tools/`.
- **Node.js is not required.** `ui/` is static; `tauri.conf.json` points `build.frontendDist` at `../ui` directly.

## Build

```sh
cargo build                                 # whole workspace
cargo build -p splatmcp-mcp                 # MCP server -> target/debug/splatmcp-mcp(.exe)
cargo build --release                       # everything, for harness use
cargo tauri build                           # desktop app through the Tauri CLI
```

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
cargo test                      # workspace: 131 tests
cargo test -p splatmcp-core     # model, PLY, authoring, edit operations
cargo test -p splatmcp-bridge   # protocol, framing, server/client round trips
cargo test -p splatmcp-mcp      # tool surface, schemas, MCP-side link
cargo test -p splatmcp          # app: document state, viewer dispatch, settings.json
cargo fmt --check
cargo clippy --all-targets
```

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

## Status

Milestones 1-4 are done: the workspace builds, the viewer works, the bridge connects the
MCP server to the live window, all eight tools work end to end, and window geometry is
persisted. Verified by 131 automated tests plus the live checks above.

Still open:

- **Bundling is disabled** (`bundle.active = false` in `src-tauri/tauri.conf.json`), so
  `cargo tauri build` produces executables rather than installers.
- **SPZ is out of scope** by an explicit earlier decision: the only splat file format is
  `.ply`. No SPZ codec exists in this repository.
- The bridge is a plain loopback socket protected by a token in the user's app data
  directory; any local process that can read that file can drive the viewer.
- One document at a time: the app displays a single splat, and `viewer_load_ply` replaces
  it.

## License

Apache-2.0 — see [LICENSE](LICENSE). The vendored PlayCanvas build under `ui/vendor/playcanvas/` is covered by `ui/vendor/playcanvas/LICENSE.playcanvas`.
