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

## Tokens are per document

A publication token counts within **one document's** sequence: document A reaching token 4 says
nothing about document B, whose first publication is token 1. Any comparison of tokens (or of
revisions) must therefore carry the document identity with it, in both the app and the window:

- `PublicationTracker` keys every entry by document id and mints `next_token` per entry;
- `ui/publication-order.js` tracks a `{ newest, displayed }` pair *per document*, and
  `isSuperseded(incoming, displayed)` compares `{ documentId, token }` pairs, treating two
  different documents as unrelated;
- `ui/components.js` and `ui/python-panel.js` pass the event's `document_id` to every ordering
  call, and the selection highlight is scoped by `(document, revision)` rather than by revision
  alone.

Getting this wrong is not subtle: a global "highest token seen" made a new document's early
publications look stale, so the window kept showing the previous model and the app's requests
timed out until the new document's token happened to exceed the old document's count.

## One document owns the screen

A request for a document that has been replaced can never be displayed, so it is not left
in flight: acknowledging another document's publication drops it as `skipped` rather than
leaving a pending request that can only expire.

## Outcomes are published, not frozen

Every request produces exactly one `PublicationNotice` — `displayed`, `failed`, `timed_out` or
`superseded` — which the app reads and applies to the records that announced a display (a job's
`display` field). Notices are bounded, drained on read, and are what make a stored promise
converge instead of staying `pending`. A preview acknowledgement raises no notice: a dry run of
revision N does not mean revision N's geometry appeared.

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

## Every commit is recorded, displayed or not

`committed_revision` moves for **every** commit the store accepts, including one nobody asked to
display (`display:false` on an edit or a patch, a job that commits quietly). A hidden commit
therefore reads as:

```json
{"committed_revision": 3, "displayed_revision": 2, "is_current": false, "display_lagging": true}
```

instead of leaving the status on revision 2 and claiming the display was current. The commit is
recorded even when the publication itself fails, and the pointer only ever moves forward: a late
report of an older revision cannot make the document look older than it is.

## A pending publication expires

The viewer is not obliged to answer, and a request that never settles must not read as "pending"
forever. Every read of the status (and every new publication) applies the acknowledgement timeout
(`ACK_TIMEOUT_MS`, published in `renderer_capabilities`): a request that has waited longer is
recorded as `timed_out` on the next read, so a stalled viewer shows up as a stall. The renderer
applies its own bound too (`LOAD_TIMEOUT_MS` in `ui/viewer.js`), so a parse or GPU preparation that
never completes is reported as a display failure rather than waited on.

## One document is on screen at a time

Switching documents is not a per-document affair: acknowledging a publication of document B makes
B the active document, and A stops reporting a displayed revision — because it is not on screen.
A status read for a document that is committed but has never been published, while another document
owns the screen, says so explicitly (`displayed_elsewhere: true`, "another document is displayed"),
which is what distinguishes "not displayed yet" from "displayed elsewhere".

That, together with loading through this seam (below), is what stops a document switch from leaving
the previous model on screen: the switch is itself a publication - the app announces the newly
committed revision and the viewer fetches it by `(document, revision)`.

## A load is a publication

Opening or importing a document used to hand the raw request to the viewer, which meant an
asset-backed load mutated the document and then failed with `ply_base64 is required`, and it meant
a committed revision could never be displayed because the display step had already failed. A load
now records its commit and announces it through the same publication seam as an edit or a job
(`src-tauri::publication::announce`), so:

- nothing large travels to the viewer and no legacy field is required,
- the reply is the viewer's own facts plus the resolved identity, and
- a load that nobody displays is still reported as *committed*, not as current.

`viewer.load_ply` remains available for a client that sends bytes inline; it is no longer the path
any SplatMCP tool uses.

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
  (`tauri_binary_response`), `revision_addressed`, `displayed_revision`, `displayed_point_count`
  and `ack_timeout_ms`. A caller never infers readiness from a fixed delay, and camera capture
  consumes this readiness contract.

  Its identity comes from the **app's own records**, not from the renderer: the displayed revision
  is the one the tracker acknowledged, and the point count is read from the store's metadata for
  that exact revision. A viewer reports only what only a renderer knows (`viewer_ready`,
  `has_splat`), because it has no document identity of its own — reading identity from it is how
  capabilities and status came to disagree about which revision was displayed.

## What is deliberately not here

- **No base64 PLY events.** Identity events plus a binary response replace them; the seam was
  extended, not replaced.
- **No partial GPU mutation.** Full revision publication is the reliable case; reusing unchanged
  component buffers or applying validated binary deltas would need PlayCanvas APIs that are not
  guaranteed, and the design says so instead of inventing them.
- **No inferred readiness.** A metadata event is not proof that geometry rendered, and the
  tracker records announcements and acknowledgements rather than asserting a render.
