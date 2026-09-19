# The capture contract

A capture answers one question: *what does this exact revision look like from this exact camera?*
Two tasks depend on that answer being trustworthy - #18 (one atomic frame) and #19 (a set of views
plus diagnostics) - and both are implemented from one contract, in
`crates/splatmcp-core/src/capture/`.

Timings are deliberately absent from the contract. `capture::session` states which completion
evidence a capture waits for, and "render twice" or "sleep 200 ms" is not among them.

## The three states, kept apart

| state | type | who produces it |
| --- | --- | --- |
| requested | `CameraSpec`, `CaptureSpec` | the caller (MCP tool or the window) |
| applied | `ResolvedCamera`, `AppliedCamera` | the viewer, reported back verbatim |
| rendered | `FrameMetadata` | the app, which mints the frame identity |

A reply never conflates them: `requested_viewport` and the achieved `viewport` are separate fields,
and a `capped` frame carries a `note` saying what changed. A capture that could not apply what was
asked fails, rather than returning a frame of something else.

## Camera

* World axes are the scene's: `+Y` up, `+Z` towards the viewer. `front` is `+Z`, `back` is `-Z`,
  `left` is `-X`, `right` is `+X`, `top` looks down with `-Z` up, `bottom` looks up with `+Z` up,
  `three_quarter` is yaw 45°, pitch 30°.
* Angles are degrees; lengths are world metres; the field of view is the **vertical** one and is
  limited to 10–120°.
* Exactly one of `pose`, `orbit`, `preset` or `fit` may be given. An orbit's pitch is limited to
  ±89.5°; the poles are reached through the `top`/`bottom` presets, which carry the up vector that
  makes them well defined.
* `fit` frames the document, a component, a selection or explicit bounds, with a `padding`
  fraction. The framing distance is `radius / sin(fov/2) * 0.95 * (1 + padding)`: the same 0.95
  margin the window's own framer uses, so an MCP fit and a UI frame agree.
* Projection is perspective (from the field of view) or orthographic (from an explicit world height).
  Asking for both is refused, as are an empty frustum, `far <= near` and a degenerate pose
  (position == target, or an up vector parallel to the view direction).
* A component or selection is framed from bounds the **app** resolves: the core refuses to guess at
  geometry it cannot see, and names the missing bounds instead.

`ui/camera-matrices.js` reproduces the core's `look_at`, `perspective` and `orthographic` functions
exactly - column-major, `-1..1` depth, f32 arithmetic - so the matrices in a reply are the ones the
core would compute, not a second implementation that agrees approximately.

## One frame, one owner

`CaptureGate` admits exactly one capture at a time. A second capture is **refused** with the holder's
name rather than queued: a caller waiting behind an unknown render cannot tell how stale its request
has become, and an explicit "busy" is what keeps two cameras from being interleaved. The gate lives
in the app process (`src-tauri/src/capture.rs`), so the window's captures and an MCP caller's are
admitted by the same rule.

A capture pins one revision before anything renders:

* `pin_for_capture` refuses a named document without `expected_revision`, a revision that is not the
  displayed one, and a different document than the one on screen.
* The app applies that rule *before* the request reaches the renderer, so a stale capture costs
  nothing.
* A document replaced while a set runs fails that view and marks the remaining views `skipped` - a
  mixed set is never returned as a partial success.

## Completion evidence (what a frame must satisfy before it is reported)

A capture reports success only when three conditions hold, and the failure names the one that did
not. None of them is a timing assumption:

1. **the pinned revision is the one the viewer displays** - while a newer revision is staging, the
   screen still shows the previous scene, so a capture pinned to the new one waits, and a capture
   pinned to a revision that never arrives fails with "showing revision X while revision Y was
   pinned";
2. **the engine completed a frame after the content changed** - `contentReadiness().upload_pending`
   is cleared by PlayCanvas' own `postrender`, which is the earliest moment new content can be on
   the GPU. Reading before it is how a capture returned a blank image with `status: "ok"`;
3. **the frame holds the document** - while this content token has not been seen to render, a frame
   that is a single flat colour for a document with gaussians is an upload frame, not a picture, and
   the capture keeps waiting (then fails with "the frame is a single flat colour although the pinned
   revision has N gaussians"). The check is skipped for the *steady state* of a content token that
   has already been seen to draw, for an empty document, and for a host that cannot read pixels -
   so a legitimate empty view is not looped until its timeout.

`ui/capture-readiness.js` owns the decision (pure and unit-tested); `ui/capture.js` owns the loop,
the size handling and the readback.

## Camera exactness

An applied pose is the pose that is rendered *and* reported:

* the transform is set directly (position, and a look-at that honours the requested up vector);
* the **interactive controls are suspended for the duration of the capture** - both `enabled` and a
  neutered `update`, so no engine build can ease the entity mid-frame. `beginDeterministicCamera` /
  `endDeterministicCamera` bracket a capture on every path, and the controls are re-attached to
  where the camera actually is when they come back;
* outside a capture, `placeCamera` **synchronises** the controls to the new pose (`reset` plus
  `focusPoint`, which also records the current zoom distance). Without that, the focus controller
  eases the camera along its view axis over the following frames, which made a requested pose arrive
  late and a reported camera disagree with the rendered frame;
* the reported camera is read from the **entity**, never from the controls' internal pose. `target`
  is the point on the camera's forward ray at the reported `distance`; the distance is stated
  separately because a transform has a ray, not a target.

`get_camera` and `viewer_status` therefore report the applied state: position, target, up, fov,
projection, near, far, distance, viewport and both matrices.

## Accepted input shapes

The published schema is authoritative, so the contract accepts what it documents, in both layers:
a format as `"png"`/`"jpeg"` (with `quality` beside it), a background as `"transparent"`, `"viewer"`
or `{kind: "solid", color}`, a projection as `"perspective"`, a fit target as `"document"`, a
diagnostic pass as `"rgb"`/`"alpha"`/…, and a camera that is omitted, `null` or `{}` to mean "keep
the current one". The tagged forms remain available for the values that carry arguments.

## Completion evidence

`ui/capture.js` waits for the renderer's own readiness (`renderReady`: the splat entity's uploaded
resource with a valid bounding box), scheduled through `postrender`, and fails with a reason if the
renderer never becomes ready. Reads happen only after that evidence, and the pixels come from the
same readback as the image, so a diagnostic pass costs no second render.

## Restoration

Capture-specific camera and viewport changes are temporary by default (`restore_previous`):

* `CameraGeneration` is bumped by every camera or viewport change made *outside* a capture in flight;
  a capture records the generation it found and may only restore that state while the number is
  unchanged.
* The decision is reported as `kept`, `restored`, `skipped_newer_navigation` or `nothing_to_restore`,
  on the success path and on the error path alike.
* `keep_camera` is the explicit opt-in for a caller that wants the capture camera to stay.
* A renderer whose controls settle their own up vector has not been navigated: `cameraMoved`
  compares position and view direction, so that settle does not refuse a restore.

## Diagnostics (#19)

A gaussian splat composites, so no pass may borrow a word from surface rendering:

* **rgb** always exists.
* **alpha** is the frame's own alpha channel: coverage is `1 - Π(1 - aᵢ)`, clamped to `0..=1`. It is
  only meaningful on a `transparent` capture, and a pass on an opaque one reports that instead of a
  white rectangle.
* **depth** is **not** available: the PlayCanvas splat renderer exposes no compositing depth
  readback. The definition is still stated for the day a renderer provides it - the transmittance
  weighted mean depth `Σ(Tᵢaᵢzᵢ)/Σ(Tᵢaᵢ)` in world metres along the camera forward axis, with
  background below the declared coverage threshold having *no* depth value - and asking for it fails
  by name with the reason.
* **component** highlights the members of one component or the current selection through the
  existing marker layer, and reports the bounded marker count. It marks membership; it is not a
  per-pixel id buffer.
* **scale_orientation** is computed from the displayed gaussians (mean and largest radius, dominant
  ellipsoid axis, share of elongated gaussians), so an oversized or elongated splat is visible
  without a render feature.
* **Normals are absent on purpose.** A splat has no well-defined surface orientation, and offering a
  covariance-derived "normal" would present a guess as geometry.

`PassCapability::ensure` refuses an unsupported pass before a frame is rendered, and
`pass_capabilities(depth_readback, component_ids)` reports what a build can actually produce, with
its limitations attached - which is also what the capabilities tool publishes.

## Sets (#19)

* One pinned revision for the whole batch; the sheet is planned before anything renders
  (columns = `ceil(sqrt(views))` when unspecified, thumbnails 4:3, sheet edge ≤ 4096).
* Unique non-empty labels, ≤ 8 views, ≤ 5 passes per view, shared and per-view passes merged without
  duplicates.
* A failed view is marked `failed` with its reason and **never** replaced by an earlier frame; an
  unreached view is `skipped`; the manifest refuses itself if a frame names another revision or a
  "captured" view carries no checksum.
* The contact sheet is composed in the window that owns the canvas (`ui/contact-sheet.js`), with each
  view's label drawn under its thumbnail, and a cell that cannot be decoded is marked as failed.
* `output_dir` writes the originals where the caller asked; the app writes them, and the manifest
  stays the artifact.

## Reference comparison

The renderer never opens a path: the app reads the reference image it was given (bounded at 32 MiB,
unreadable or oversized refused with the path in the message) and forwards the bytes with the
request. The renderer decodes them, applies the caller's alignment through a canvas transform
(crop, rotate, scale, translate, resize in one sampling), builds both intensity planes, compares
them over the declared mask and, when asked, draws the difference image. So a comparison reports
metrics and an artifact rather than a note about a missing decoder.

## Reference comparison

Alignment is explicit: scale, offset, rotation, crop, colour space and resize-to-capture. There is no
auto-alignment, because an unaligned difference reports the alignment error as image disagreement.
Metrics are named and bounded - mean absolute, RMS, maximum absolute, mean signed (capture minus
reference) and the share of compared pixels above the threshold - always over a declared mask
(region ∩ coverage). The result carries a disclaimer, and a pixel difference is never presented as a
likeness, identity or correctness judgement. Reference files are read, never written.

## Where it lives

| file | responsibility |
| --- | --- |
| `crates/splatmcp-core/src/capture/{mod,camera,session,diagnostics,set}.rs` | the contract itself |
| `crates/splatmcp-bridge/src/protocol.rs` | the wire shapes and the request builders |
| `src-tauri/src/capture.rs` | pin, frame identity, budgets, one capture at a time |
| `src-tauri/src/capabilities.rs` | what this build supports, with real limits |
| `ui/camera.js`, `ui/camera-matrices.js` | resolution and matrices in the renderer |
| `ui/capture.js` | render evidence and readback |
| `ui/capture-session.js` | one capture at a time, pin, restore |
| `ui/capture-set.js`, `ui/contact-sheet.js` | sets, manifests, the sheet |
| `ui/diagnostics.js`, `ui/reference.js` | passes and the honest comparison |
| `ui/inspection-panel.js` | the panel that shows a set and compares a reference |

## Evidence

* `cargo test -p splatmcp-core`: camera resolution, presets, degeneracy, matrices, the gate, the
  restore rules, coverage and depth semantics, the set rules, the metric definitions.
* `node ui/camera-spec.test.mjs`, `node ui/capture-session.test.mjs`, `node ui/capture-set.test.mjs`,
  `node ui/reference.test.mjs`: the same rules on the renderer side, against faked viewers, plus the
  FNV-1a vectors the checksums must match.
