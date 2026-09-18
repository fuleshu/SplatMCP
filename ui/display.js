// Display badge: the app's own answer to "which revision is on screen?".
//
// A commit and a display are two different facts. The document store commits a revision the
// moment an edit or a job succeeds; the viewer presents one when a frame has actually drawn it.
// This badge shows both, so a lagging display is visible rather than smoothed over:
//
// - `revision 7` when the picture matches the newest commit;
// - `revision 7 (not shown yet)` when the app committed something the viewer has not drawn;
// - the last publication failure when a revision could not be prepared at all.
//
// It never infers anything from a timer: every value comes from the app's publication status,
// which is the same state a tool call reads.

/** How often the badge refreshes while the window is open. */
const POLL_MS = 1000;
/** A failure stays visible for a moment, so it can be read before it scrolls away. */
const FAILURE_LINGER_MS = 8000;

export class DisplayBadge {
  constructor(node, invoke) {
    this.node = node;
    this.invoke = invoke;
    this.timer = null;
    this.busy = false;
    this.lastFailureAt = 0;
    this.lastFailure = "";
  }

  /** Starts refreshing; the badge hides itself while nothing is loaded. */
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

  /** Reads the app's publication status and renders it. */
  async refresh() {
    if (this.busy) {
      return;
    }
    this.busy = true;
    try {
      const status = await this.invoke("publication_status", {});
      this.render(status);
    } catch (error) {
      // No document, or no app: the badge simply reports nothing rather than guessing.
      this.render(null);
    } finally {
      this.busy = false;
    }
  }

  /** Draws one status reply. */
  render(status) {
    if (!status || status.committed_revision === null || status.committed_revision === undefined) {
      this.node.hidden = true;
      this.node.textContent = "";
      this.node.removeAttribute("title");
      return;
    }
    const committed = status.committed_revision;
    const displayed = status.displayed_revision;
    let text;
    if (displayed === null || displayed === undefined) {
      text = `committed r${committed} (nothing shown yet)`;
    } else if (status.is_current) {
      text = `showing r${displayed}`;
    } else {
      text = `showing r${displayed}, committed r${committed} (not shown yet)`;
    }
    if (status.pending) {
      text += ` · loading r${status.pending.revision}`;
    }

    // A failure is the most important thing on this line: it is shown for longer and wins over
    // a plain lagging display, because it means a revision will not appear on its own.
    const failure = Array.isArray(status.failures) ? status.failures[status.failures.length - 1] : null;
    if (failure) {
      const described = `revision ${failure.revision} could not be shown: ${failure.reason}`;
      if (described !== this.lastFailure || Date.now() - this.lastFailureAt > FAILURE_LINGER_MS) {
        this.lastFailure = described;
        this.lastFailureAt = Date.now();
      }
      text += ` · ${described}`;
    } else if (this.lastFailure && Date.now() - this.lastFailureAt < FAILURE_LINGER_MS) {
      text += ` · ${this.lastFailure}`;
    }

    this.node.hidden = false;
    this.node.textContent = text;
    this.node.title = status.summary || "";
    this.node.dataset.lagging = status.display_lagging ? "true" : "false";
  }
}
