# The MCP contract: typed results, structured errors, capabilities and real limits

Task #20. Three things a caller must be able to do without reading English: know what a call
returned, know why it failed, and know what this build supports.

## Typed results

`crates/splatmcp-mcp/src/contract/envelope.rs` defines the result envelope:

```
{
  "status": "ok" | "error",
  "correlation": { "operation_id", "request_id", "job_id", "document_id", "revision", "displayed" },
  "payload": { ...the tool's own fields, unchanged... },
  "failure": { ...present exactly when status is "error"... }
}
```

* A reply carries the same data twice: the JSON `structuredContent` a client reads by field, and one
  compact line of text a model reads. Nothing structured is stringified into a lone text block.
* The correlation block is read from the payload the tool already produced, so a legacy reply that
  names its document is correlated without being rewritten. An empty block is omitted rather than
  filled with placeholders.
* The envelope wraps the operations that report state - `splatmcp_capabilities`, `capture_view`,
  `capture_views` - rather than every legacy reply. Wrapping everything would rename existing fields
  for no gain; the documented schema version is the bridge's `PROTOCOL_VERSION`, and a future
  incompatible change is a new version rather than a silent reshuffle.
* Large artifacts travel as resource references or checksums, never as the only machine-readable
  field: `metadata_only` on a capture returns identity, pose and checksum without the image, and a
  capture set's manifest carries checksums rather than the frames.

## Structured errors

`crates/splatmcp-mcp/src/contract/error.rs` is the vocabulary:

| field | meaning |
| --- | --- |
| `code` | a stable snake_case code, never renamed and never reused |
| `category` | the grouping a client branches on (input, identity, validation, capability, budget, cancellation, timeout, runtime, renderer, transport, internal) |
| `layer` | where it came from: `mcp`, `bridge`, `app`, `document`, `renderer`, `asset`, `job`, `python`, `filesystem` |
| `message` | what happened, as the layer reported it |
| `retryable` | whether an identical retry may succeed |
| `outcome` | `committed`, `not_committed`, `unknown` or `not_applicable` |
| `hint` | what to do next, one line |
| `details` | bounded, machine-readable context |

Codes: `invalid_input`, `no_document`, `unknown_document`, `stale_revision`, `validation_failed`,
`unsupported_capability`, `asset_failure`, `budget_exhausted`, `cancelled`, `timeout`,
`runtime_unavailable`, `renderer_failure`, `transport_disconnect`, `internal_error`.

Two rules are load-bearing:

* **A timeout after a mutation stays `unknown`.** A dropped connection or a deadline says nothing
  about the store, and reporting it as a rollback would invite the caller to repeat work that already
  landed. `Failure::inferred` therefore starts at `unknown` for every layer except `mcp` itself, and a
  layer says `committed`/`not_committed` only when it knows.
* **A refusal is a tool execution error, not a protocol error.** MCP separates the two: a protocol
  error means the call never reached the tool. A stale revision, an unsupported pass or a
  validation refusal travelled to the tool and stayed in the conversation, so it is returned as an
  error *tool result* with structured content. Unclassified failures are reported as
  `internal_error` rather than guessed at.

`tool_error` in the server classifies a layer's message instead of wrapping it as `internal_error`,
which is what made validation failures, stale revisions and transport failures indistinguishable.

## Capabilities and real limits

`src-tauri/src/capabilities.rs` assembles the report and `splatmcp_capabilities` serves it. Every
number is read from the component that enforces it: the capture gate's `CaptureLimits`, the job
service's `JobLimits`, the asset registry's budgets, the document store's retention limits, the
Gaussian contract's point limit, and the renderer's own pass list.

* `app_attached: false` is a valid answer, not a failure: the contract's own limits are reported so a
  caller can start working, and the reply says they are defaults rather than a negotiation.
* `unsupported` names what this build does not implement, with the reason - today,
  `diagnostics.depth` (no compositing depth readback) and `diagnostics.normals` (a splat has no
  well-defined surface orientation).
* `external_boundaries` names the limits that are **not** this app's: the historical 200 000-byte
  review limit belonged to a host approval layer, and "the selected model is at capacity" was a
  client-side message. Neither is a SplatMCP renderer or Gaussian limit, and neither is reported as
  one.
* `client_budget_hint_bytes` is how a caller supplies its *own* budget: the reply labels it as
  enforced by the client, so it can never be mistaken for an app limit.
* Nothing claims to discover an arbitrary client's limits, and no safety annotation was relaxed: the
  capture tools are marked `read_only` because they change only the camera and the viewport, and a
  capture that *keeps* its camera says so through `keep_camera`.

## Annotations

| tool | read_only | idempotent | open_world |
| --- | --- | --- | --- |
| `splatmcp_capabilities` | yes | yes | no |
| `capture_view` | yes | no | no |
| `capture_views` | yes | no | no |

`capture_view` is read only with respect to the *document*: it renders a frame and restores the
capture camera by default. It is **not** idempotent, because each capture mints a new frame id and
therefore a new artifact - which is exactly why the reply carries one.

## Workflow

`splatmcp_capabilities` → `create_splat` / `load_splat` / `register_asset` → `splat_info` for the
identity and revision → `edit_batch` with `expected_revision` and `operation_id` → `capture_view` or
`capture_views` at that revision → `splat_display` to confirm what is on screen → `save_splat`.

## Evidence

* `cargo test -p splatmcp-mcp`: code/category stability, classification of real layer messages,
  the unknown-outcome rule, envelope correlation, and the cap on the tool listing.
* The listing budget test still holds with 22 tools: descriptions are trimmed rather than the budget
  raised.
