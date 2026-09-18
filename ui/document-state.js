// Authoritative document state for the window.
//
// The controls in the toolbar act on the document the *app* holds, not on whatever the viewer
// happens to be showing. A splat created by a tool call, an import job or a script therefore has
// to enable Save the same way a manually opened file does - which is why this module polls the
// app's own document record instead of relying on a local flag that only the Open button sets.
//
// One poller, one merged state, several readers:
//
// - `loaded` and `revision` come from `document_info` (identity, counts, retention),
// - `committed_revision`, `displayed_revision` and `display_lagging` come from
//   `publication_status` (the picture, which can lag the document),
// - the two are reported together, never collapsed, so "Save is enabled" and "the window is
//   showing revision 7" can both be true while "the window is showing revision 9" is not.

/** How often the state is refreshed. Cheap: metadata only, no geometry. */
const POLL_MS = 800;

export class DocumentState {
  constructor(invoke) {
    this.invoke = invoke;
    this.timer = null;
    this.busy = false;
    this.listeners = new Set();
    this.state = {
      loaded: false,
      documentId: null,
      revision: null,
      pointCount: 0,
      fileName: null,
      committedRevision: null,
      displayedRevision: null,
      displayLagging: false,
      displayFailure: null,
    };
  }

  /** Starts polling and resolves with the first state. */
  start() {
    if (this.timer === null) {
      this.timer = setInterval(() => this.refresh(), POLL_MS);
    }
    return this.refresh();
  }

  stop() {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  /** Subscribes to state changes; returns an unsubscribe function. */
  subscribe(listener) {
    this.listeners.add(listener);
    listener(this.state);
    return () => this.listeners.delete(listener);
  }

  /** The last known state, for a reader that just needs the current answer. */
  current() {
    return this.state;
  }

  /** Reads the app's document and publication records and publishes the merged state. */
  async refresh() {
    if (this.busy) {
      return this.state;
    }
    this.busy = true;
    try {
      const info = await this.invoke("document_info");
      // Publication status is asked for the displayed document only: it is the one whose
      // display state the toolbar describes.
      let publication = null;
      if (info && info.loaded !== false) {
        publication = await this.invoke("publication_status", {});
      }
      this.update(info, publication);
    } catch (error) {
      // No app, or no document: nothing is loaded, and the toolbar must say so rather than keep
      // a stale "Save" enabled.
      this.update(null, null);
    } finally {
      this.busy = false;
    }
    return this.state;
  }

  /** Merges one pair of replies into the state and notifies readers when it changed. */
  update(info, publication) {
    const loaded = Boolean(info) && info.loaded !== false;
    const next = {
      loaded,
      documentId: info?.document_id ?? null,
      revision: info?.revision ?? null,
      pointCount: info?.point_count ?? 0,
      fileName: info?.file_name ?? null,
      committedRevision: publication?.committed_revision ?? null,
      displayedRevision: publication?.displayed_revision ?? null,
      displayLagging: Boolean(publication?.display_lagging),
      displayFailure:
        Array.isArray(publication?.failures) && publication.failures.length > 0
          ? `${publication.failures[0].reason} (revision ${publication.failures[0].revision})`
          : null,
    };
    if (sameState(this.state, next)) {
      return;
    }
    this.state = next;
    for (const listener of this.listeners) {
      listener(next);
    }
  }
}

/** True when two states describe the same document, revision and display. */
function sameState(a, b) {
  return (
    a.loaded === b.loaded &&
    a.documentId === b.documentId &&
    a.revision === b.revision &&
    a.pointCount === b.pointCount &&
    a.fileName === b.fileName &&
    a.committedRevision === b.committedRevision &&
    a.displayedRevision === b.displayedRevision &&
    a.displayLagging === b.displayLagging &&
    a.displayFailure === b.displayFailure
  );
}
