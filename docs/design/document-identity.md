# Document identity, revisions and snapshots

Implemented in `splatmcp-core::document` (identity, revisions, compare-and-swap, snapshots,
retention) and `src-tauri::document` (the app-side adapter: parsing, serialising, files and
export). This is the shared foundation every producer and consumer of geometry uses: MCP
tools, the viewer, the desktop panel and the Python generation service.

## Why identity is not a path

The app used to identify the displayed splat mainly by the file it came from, which made two
different things look the same: *the document I am editing* and *the file I opened*. It also
meant `C:/scenes/house.ply` opened twice was one document, and a save under a new name was a
different one - neither is true.

Identity is now minted and opaque:

| Concept | Type | Meaning |
| --- | --- | --- |
| Document identity | `DocumentId` | `doc-<session>-<n>`, minted by the store for the session that owns it |
| Exact revision | `DocumentHandle { document_id, revision }` | what every later request quotes |
| Path | `Provenance::source_path` | where geometry came from; never identity |
| Export path | `Provenance::exports[].path` | where a revision was written; never identity |
| Artifact checksum | `ArtifactChecksum` (`fnv1a64`) | identifies the *bytes of one encoded file*, not the scene |

The **session stamp** is what makes a handle from an earlier run fail loudly: `doc-4f2a-3` can
only be minted by the session whose stamp is `4f2a`, so a stale handle cannot silently name
whatever document happens to hold index `3` this time. Handles are valid for the life of the
process; a versioned project format would have to restore both the stamp and the documents
deliberately, which is out of scope here (no persistence exists yet).

## Revisions

| Event | Revision |
| --- | --- |
| Open a file, import a buffer, create a splat | new document, revision **1** |
| Load that is an explicit replacement of a named document | same identity, revision **+1** |
| Edit steps (`edit_splat`, `merge`), reload of the source | +1 |
| Component metadata change | +1 (geometry is shared, not copied) |
| Python generation job committing a candidate | +1 |
| Export / save, capture, inspect, read | **unchanged**: only provenance is recorded |

The revision never moves backwards and is never reused. Undo, when it lands, commits a *new*
revision for the same reason: rewinding the counter would silently invalidate every handle a
caller is holding.

Every accepted change appends a `RevisionRecord { revision, kind, operation, at_ms }` to the
document's history (bounded to the newest 8), so a reply can say *who* produced a revision -
`edit_splat`, `job 12`, `reload`, `open` - and *when*.

## Compare and swap

A mutation states what it expects, and the check happens in the same critical section as the
swap:

```text
Expected::Any                 the displayed document, or a new one when nothing is displayed
Expected::Revision(r)         the displayed document, which must still be at revision r
Expected::Handle(handle)      that exact document at that exact revision
```

Consequences that are tested:

- Two candidates built from the same revision produce **one commit and one explicit
  `document_conflict`**, with the current handle in the error. Never last-writer-wins.
- A named document must also name its revision. A request that says only *which* document
  cannot overwrite work that landed in between.
- A mutation that names a document which is no longer displayed fails with
  `unknown_document` instead of silently editing whatever is on screen now. `Expected::Any` is
  the only form that resolves "whatever is displayed", it resolves **once at request receipt**,
  and the reply names the handle it resolved.
- A stale/expired/unknown handle leaves the displayed document exactly as it was.

Error codes, stable for structured replies: `no_document`, `unknown_document`,
`document_conflict`, `snapshot_expired` (plus `invalid_request` from the app adapter when the
input itself is wrong).

## Snapshots, locks and retention

Reading yields a `Snapshot`: an immutable `Arc<Splat>` of one exact revision plus the
provenance and metadata that describe it. Content is held behind `Arc` and mutated with
copy-on-write, so

- a snapshot stays readable and unchanged while the editor moves on - a slow operation on
  document A finishes correctly after document B was opened;
- holding a snapshot costs nothing until the content actually changes: nothing is copied while
  a reader exists, and one clone when the last reader leaves;
- the store's mutex protects *identity, provenance and the `Arc` pointers* only.

**Parsing, serialising, exporting and inspecting all happen on a snapshot, after the lock is
released.** A 500 000 gaussian document is read, written or inspected without blocking another
request, and `document.inspect` (which `splat_info` uses) never serialises a PLY to the MCP
server at all.

Retention is bounded (`RetentionLimits`: 8 revisions or 512 MiB of gaussian data by default,
oldest first):

| Operation | Behaviour |
| --- | --- |
| `DocumentStore::resolve(handle)` | exact revision, or `snapshot_expired` when it was evicted |
| `DocumentStore::pin(handle)` | keeps one revision resolvable for as long as a caller needs it |
| `DocumentStore::release(pin)` | drops the pin; retention catches up on the next change |
| `retention()` / `RetentionSummary` | documents, revisions, pins, bytes, and whether pins are holding more than the budget |

A pin wins over the budget rather than breaking a promise: `over_budget` reports the temporary
overshoot instead. Pins are used where a handle is resolved *later* - export pins the revision
while it serialises and writes, and the viewer's revision fetch pins it while it produces
bytes. A Python job's source is an owned, detached copy taken at request receipt, so it stays
readable for the life of the job without a pin.

## What the app exposes

| Surface | Behaviour |
| --- | --- |
| `open_splat` (Tauri) | opens a file as a **new document**, records the path as provenance |
| `reload_splat` (Tauri) | re-reads the source of the displayed document as its next revision |
| `current_splat_bytes` (Tauri) | bytes of the displayed revision, serialised outside the lock |
| `save_splat` (Tauri) | exports, records the export with its artifact checksum, returns path + identity + checksum; the revision does not move |
| `document_info` (Tauri) | flat identity, provenance, retained revisions and retention for the panel |
| `splat_bytes_for_revision` (Tauri) | the exact revision the viewer was told about, or a clear failure |
| `viewer.load_ply` (bridge) | no target: a new document; with `document_id` + `expected_revision`: an explicit replacement |
| `document.get_ply` (bridge) | bytes of the displayed revision, or of a named document/revision, with the identity they are of |
| `document.inspect` (bridge) | bounded metadata of a named or displayed revision; no geometry crosses the wire |
| `document.reload` (bridge) | re-reads the source, optionally against a stated revision |
| `document.set_component` (bridge) | component metadata change, advancing the revision |

Bridge protocol version stays **1**: every one of these is additive, and a client that never
sends them cannot notice them. Replies gained a `document` object
(`DocumentSummary`/`InspectionSummary`); a reply from an older app simply has no identity in it,
which callers already tolerate.

## MCP behaviour

- `create_splat` and `load_splat` produce a **new document** and report its identity.
- `edit_splat` on the displayed splat sends the identity it read back with the replacement, so
  the edit keeps the document and advances the revision by one. A stale edit fails with a
  conflict instead of overwriting. An edit whose source is a `.ply` produces a new document.
- `splat_info` reports the identity (id + revision) plus the bounded inspection, and reads the
  displayed document as metadata - no PLY transfer for a count.
- Every reply that produced or edited a splat carries `document: { document_id, revision }`, so
  a caller can quote the revision back instead of guessing which document it touched.

## Migration notes

1. **A file name is no longer identity.** Two opens of the same path are two documents; a save
   under a new name does not change identity. Previously a repeated load of the same path kept
   the identity and bumped the revision: use `document.reload` / `reload_splat` for that, which
   is explicit about re-reading the source.
2. **A load that replaces a document must name it** (`document_id` + `expected_revision`).
   Without a target a load is an open, which is what `create_splat`/`load_splat` do.
3. **`edit_splat` keeps the identity of the displayed document**; previously an edit effectively
   re-imported whatever file name it happened to use. Existing workflows that read `splat_info`
   and quote `expected_revision` for a Python job are unaffected.
4. **A Python job creating geometry now produces its own document** instead of replacing the
   displayed one's content under the old identity: the job target states either a document and
   revision (an edit) or nothing (a new document), and a new document gets a new identity.
5. **Snapshot handles expire.** A revision that has fallen out of the retention window fails
   with `snapshot_expired`; increase the window or pin the revision if a caller needs it longer.
6. **Saving reports the identity and an artifact checksum.** `save_splat` returns
   `{ path, document_id, revision, checksum, bytes }`; the window shows the path and revision.

Out of scope, and deliberately not promised: durable identities across restarts, a project
file, multi-document tabs, peer/remote sharing of identities, and undo (which will commit a new
revision on top of these primitives).

## Evidence

Recorded for task #12, run on the development machine:

```sh
tools\cargo_env.cmd test -p splatmcp-core      # 95 unit + 8 contract + 11 documents + 3 interop
tools\cargo_env.cmd test -p splatmcp-bridge    # 22 unit + 8 round trip
tools\cargo_env.cmd test -p splatmcp-mcp       # 56 unit + 1 app link
tools\cargo_env.cmd test -p splatmcp           # 22 app tests
tools\cargo_env.cmd clippy -p splatmcp-core -p splatmcp-bridge -p splatmcp-mcp
tools\cargo_env.cmd clippy -p splatmcp --all-targets
```

`crates/splatmcp-core/tests/documents.rs` holds the acceptance checks that need no window:

- two candidates from one revision, and four racing threads, produce one commit and explicit
  conflicts;
- a slow operation on document A cannot touch document B, and A stays readable meanwhile;
- every mutation kind advances the revision by exactly one, with one history;
- an export records provenance without moving the revision, and an artifact checksum is not a
  content identity;
- reopening the same path is a new document, a named revision is a replacement;
- stale, expired and foreign handles fail explicitly and change nothing;
- a pin keeps one revision readable while the document moves on, and retention reports the
  overshoot;
- metadata for a 200 000 gaussian document is the same fixed size as for two gaussians.

Not run in this task: an end-to-end MCP session against a live window (the acceptance items it
would confirm - one commit/one conflict, identity in tool replies, agreement between MCP and UI
- are covered by the store, adapter and tool tests above), the installer build, and Adashi QA
jobs.
