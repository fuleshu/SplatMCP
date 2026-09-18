# Authoring transactions, components and selections

Prose for the two design artifacts `authoring-edit-transactions` and
`authoring-components-selections`. The code is the authority: `splatmcp-core::transaction` and
`splatmcp-core::components` carry the same rules in their module docs, and their unit tests pin
them.

## What problem this answers

An edit sequence used to be a loop: resolve indices, apply, validate, repeat. Three things went
wrong in that shape.

1. **A failure halfway through left half an edit applied.** `apply_all` stops at the first
   failure, but the steps before it had already changed the document.
2. **A retried request applied twice.** A lost response is indistinguishable from a lost edit, so
   a caller that retried a `duplicate` or a `merge` created the geometry again.
3. **An index was treated as an identity.** Deleting a point shifted every later row, so a
   selection saved before the delete referred to different gaussians after it.

## The transaction boundary

```
request (operation id + canonical hash)
  -> replay a recorded receipt, or refuse different content under the same id
  -> resolve the source revision (an exact snapshot)
  -> resolve every step's targets against that snapshot, as stable point ids
  -> apply the whole operation list to a detached candidate
  -> validate the candidate
  -> commit once, under compare-and-swap, as exactly one new revision
  -> record an undo step and a receipt
```

Everything before the commit is reversible by dropping the candidate: the authoritative document
is only ever asked to swap in a validated one. `Expected` is resolved at request receipt, so a
stale request gets `document_conflict` with the current handle rather than overwriting newer work.

**Target resolution.** `TargetResolution::Stable` (the default) resolves each step's selection
once, against the source snapshot, and keeps it as point ids; the ids are re-mapped to rows before
each step, so a removal or a merge cannot redirect a later operation through shifted rows. If a
step's target no longer exists, the batch is refused rather than applied to a neighbour.
`TargetResolution::Sequential` re-evaluates component, box and attribute selections against the
candidate after each step - the documented opt-in for "act on what is there now".

**Preview.** A dry run runs the same pipeline and retains the candidate behind a bounded handle
(4 candidates / 256 MiB). Previewing never touches the displayed document. Committing a preview
requires the caller's expectation to still resolve to the exact revision the preview was built
from, so a stale preview cannot overwrite newer work - it comes back as `preview_conflict` naming
both revisions.

**Idempotency.** `operation_id` plus the canonical request hash identifies one mutation. An
identical retry returns the recorded receipt with `replayed: true`; the same id with different
content is `operation_conflict`; an id whose receipt is no longer retained is `unknown_outcome`,
because replaying a destructive operation on a guess is worse than telling the caller to look.

**Undo and redo.** Each document keeps a bounded history (8 steps, 256 MiB, oldest evicted first).
Undo and redo commit **new** revisions - the revision counter never rewinds - and a new edit clears
the redo stack. Undo/redo are only offered for revisions this service produced: a revision that
arrived from elsewhere (a file replace, a Python job) clears the history instead of being stepped
over.

**Receipts are honest about side effects.** Commit, file export and viewer presentation are three
separate outcomes. A committed edit whose export or display failed is still a recorded commit, and
its receipt says which part failed. Atomicity applies to the document; there is no promise of an
all-or-nothing filesystem or GPU transaction.

**A receipt is replayed, not recomputed.** It carries its own copy of what it produced - document,
revision, point count, file name, per-step counts and the side-effect outcomes - so a retry reports
the outcome of the request it repeats even after the document has moved several revisions on or the
source revision has been evicted. Replay is resolved *before* any source snapshot is required, and a
replayed receipt never re-exports a file or re-announces a revision. The app records the export and
display outcomes back onto the receipt once they are known, addressed by the revision they concern.

**`published` is not `done`.** An event that left the app proves only that it left the app, so a
commit reports `published` and the window's acknowledgement (`edit_note_displayed`, or
`edit_note_display_failed`) turns it into `done` or `failed`. A tool reply therefore never claims a
picture appeared when all that happened is that the revision was announced with its document id and
revision, so the window can fetch exactly those bytes.

## Components, point identities and selections

`splatmcp_core::components::AuthoringSet` holds the authoring layer of one document revision:
opaque component ids, editable names, explicit local frames, free-form metadata, and a stable
`PointId` per row. It sits **beside** the gaussian buffer, never inside it, so PLY import/export,
edits and the Python bindings keep working on plain geometry.

**`edit_splat` is the same transaction.** Editing the displayed document goes through
`document_edit_batch`, so the long-standing tool honours component, point-id, saved-selection,
frame and sphere targets, commits one revision, and preserves component membership and undo
history. A detached source (`new`, or a `.ply` path) cannot resolve any of that, so those target
fields are *refused* with the field names instead of being dropped - silently widening a
component-targeted edit to the whole document is the failure this rule exists to prevent.

Identity rules:

| event | identity |
|-------|----------|
| new points (merge, duplicate, recipe output) | new ids |
| surviving points | keep their ids |
| removed points | retired, never re-issued |
| component replaced | replacement points are new; other components untouched |
| cross-document import | every id re-minted |
| revision produced outside the service | the layer is rebuilt and reported as `rebuilt` |

A selection composes, in a fixed order: component membership, explicit point ids, spatial
predicates (box inside/outside, sphere) in a chosen frame, attribute filters, then `first`. Rows
come back ascending; box and sphere boundaries are inclusive. A resolved selection is retained as
a handle bound to a document and revision, with count, bounds and a bounded id sample, so quoting
it later cannot silently match other gaussians.

**Local frames and anisotropic gaussians.** A `LocalTransform` is translation, rotation and
positive per-axis scale (`A = R·S`). Rotating or scaling a gaussian transforms its covariance
`C' = A·C·Aᵀ` and decomposes the result back into a valid scale/orientation pair, which is the
only way a rotated, anisotropic gaussian keeps its shape. Reflections (`det < 0`), singular
transforms and degenerate quaternions are refused with `unsupported_transform`, never repaired.

**Persistence and reopen.** Component metadata is written to a versioned `<file>.authoring.json`
sidecar carrying the document id, the revision, the artifact checksum of the PLY it belongs to,
each component's frame, and its members as *both* identities and the rows they occupy. Opening a
file always mints a new identity, so the association used on open is the content the record claims
to describe: the artifact checksum plus the gaussian count. A record that matches is **restored**
- every component rebuilt with fresh identities at the recorded rows - and a record that does not
is **refused with a reason the reply carries**, never attached by file name and never merely
printed. The restored-rows rule is what makes a restore possible at all: an identity belongs to
the session that minted it, a row belongs to the file. A plain PLY export keeps its documented
guarantee: geometry only.

**A selection is visible.** A resolved selection is published as `splat://selection` with its
handle, revision, document and count; the window asks the app for the marker geometry of exactly
those point ids (a bounded PLY of bright markers, authored in document space) and draws it as a
second layer over the document. The sidebar and a tool call therefore describe the same gaussians,
and a highlight that belongs to another revision is cleared rather than left pointing at geometry
that is no longer displayed.

## Surfaces

The same `TransactionService` instance is shared by the native window, the Python panel and every
stdio MCP connection, so previews, history, the idempotency ledger and component ids agree across
all of them. Thin actions:

- Tauri commands: `edit_batch`, `edit_preview`, `commit_preview`, `preview_splat_bytes`,
  `edit_history`, `edit_undo`, `edit_redo`, `edit_note_displayed`, `edit_note_display_failed`,
  `component_list`, `component_action`, `selection_highlight`, `splat_bytes_for_revision`
  (document id *and* revision, so the window fetches the revision it was told about rather than
  whatever is displayed).
- Bridge methods: `document_edit_batch`, `document_commit_preview`, `document_history`,
  `document_undo`, `document_redo`, `document_components`.
- MCP tools: `edit_batch`, `edit_history`, `splat_components`.

A committed revision is published as `splat://edit-revision` (identity only); the frontend fetches
that exact revision as binary, so a display failure cannot leave the viewer showing half an edit.
