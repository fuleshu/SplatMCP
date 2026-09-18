# Publishing exact revisions to the viewer

Implemented in `splatmcp-core::publish` (the per-document publication state machine) and
`src-tauri::publication` (the seam to the window: request minting, binary payload addressing,
status and capabilities). The front end consumes it in `ui/viewer.js` (staged swap),
`ui/components.js` and `ui/python-panel.js` (acknowledgement) and `ui/display.js` (the badge).

The rule this design exists to keep: **a commit is not a display**. The document store commits
a revision when an edit, an import or a job succeeds. The viewer presents a revision when a
frame has actually drawn it. Those are two facts, and every layer reports them separately
instead of collapsing them into one "loaded" flag.

| Concept | Where it lives | Meaning |
| --- | --- | --- |
| `committed_revision` | the store, mirrored on the tracker | what the authoritative document holds |
| `displayed_revision` | the tracker, set only by an acknowledgement | what a frame has presented |
| `PublicationRequest { document, revision, token, source, frame }` | minted by the app before the event is sent | one piece of publication work |
| `PublicationOutcome` | the tracker | `pending`, `displayed`, `failed`, `skipped`, `timed_out` |

## Why a token and not just a revision

Two publications of the *same* revision are still two different pieces of work. The viewer
fetches bytes for `(document, revision)`, and it may finish a slow load after a newer revision
was already shown. If a revision were the only check, the delayed load could acknowledge and
be recorded as the picture.

So the app mints a **monotonic token per document** before emitting the event, the event
carries it, and the acknowledgement must quote `(document, revision, token)`. Anything else is
refused with `stale_acknowledgement`, recorded in the tracker's failure history, and **cannot
become the displayed revision**. The viewer applies the same rule from its own side: every
load carries the request it belongs to, and a load a newer request superseded is abandoned
before the swap.

## Coalescing, not queueing

A newer publication for the same document supersedes the one in flight. The superseded request
is recorded as `skipped` with the revision that replaced it, never as displayed. That is what
makes rapidly published revisions A/B/C end on C while still reporting that B was never drawn:

```
begin(1) -> token 1        # A in flight
begin(2) -> token 2        # B in flight; A recorded skipped(superseded_by 2)
begin(3) -> token 3        # C in flight; B recorded skipped(superseded_by 3)
ack(1)   -> refused: stale  # A may not claim the screen
ack(3)   -> displayed revision 3
```

## Staging and the swap

`SplatViewer::publish` does the work in this order, and the order is the guarantee:

1. the candidate PLY is loaded into an asset that is **not attached to the entity tree**, so the
   previous model keeps rendering;
2. if a newer request arrived meanwhile, the prepared asset is disposed, the object URL revoked,
   and the call returns `null` - nothing was swapped, so nothing changed on screen;
3. only once the candidate is ready is the previous entity destroyed and the new one attached
   (`swapIn`), which is also where the old asset, its GPU resources and its object URL are
   released;
4. a failure at any point (parse, load, GPU preparation) disposes only what this attempt
   created and leaves the previous model visible, then reports `edit_note_display_failed`.

A failed publication therefore never blanks the canvas, and it never overwrites
`displayed_revision`: the badge shows the last revision that really appeared next to the one the
document committed.

## The bytes never travel as JSON

The event payload and the acknowledgement carry identity only:

```json
{ "contract_version": 1, "document_id": "doc-4f2a-1", "revision": 7, "token": 3,
  "source": "committed", "frame": false, "file_name": "scene.ply", "point_count": 200000 }
```

The geometry is fetched through the binary Tauri response `splat_bytes_for_revision`
(`splat_bytes_for_handle` for an explicit handle), addressed by document *and* revision, so a
500 000 gaussian revision never becomes a JSON payload and the store's lock is not held while
the bytes are serialised.

`source: "preview"` names a retained preview candidate. A preview is a frame, not a revision:
acknowledging one leaves `displayed_revision` where it was, because a dry run does not produce
a revision and must never be reported as one.

## Status and capabilities

- `publication.status` answers from the **tracker**, not from the renderer, so it works while
  the window is busy or closed. It reports `committed_revision`, `displayed_revision`,
  `is_current`, `display_lagging`, the request still in flight, what happened to the most recent
  one, which revisions were skipped and which failed. `src-tauri` exposes it as
  `publication_status`, the bridge as `publication.status`, and the MCP tool `splat_display`
  makes it readable to a caller.
- `publication.capabilities` reports what the renderer can do: `transport`
  (`tauri_binary_response`), `revision_addressed`, the viewer's own reported
  `displayed_revision` and `displayed_point_count`, and `ack_timeout_ms`. A caller never infers
  readiness from a fixed delay, and camera capture consumes this readiness contract.

## What is deliberately not here

- **No base64 PLY events.** Identity events plus a binary response replace them; the seam was
  extended, not replaced.
- **No partial GPU mutation.** Full revision publication is the reliable case; reusing unchanged
  component buffers or applying validated binary deltas would need PlayCanvas APIs that are not
  guaranteed, and the design says so instead of inventing them.
- **No inferred readiness.** A metadata event is not proof that geometry rendered, and the
  tracker records announcements and acknowledgements rather than asserting a render.
