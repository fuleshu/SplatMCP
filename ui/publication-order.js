// Publication ordering, in one dependency-free place.
//
// A revision is published by the app with a monotonically increasing token, and the window then
// fetches and displays it. Two things can arrive out of order:
//
// - a *newer* publication while an older one is still fetching, and
// - an older publication whose fetch resolves *after* a newer one is already on screen.
//
// The second case is the dangerous one: the late older load would become the newest request the
// viewer saw and replace newer geometry with stale pixels. Comparing tokens, rather than arrival
// order, is what makes the rule independent of scheduling.
//
// **Tokens are minted per document**, so a comparison is only meaningful inside one document.
// Document B's first publication is token 1 whatever document A reached, and treating A's token 4
// as a bar for B would drop B's early publications silently - the window would keep showing A and
// a caller would see "pending" until B's token happened to exceed A's. Every entry point here
// therefore takes the document id alongside the token, and a token from a different document is
// never treated as older: it is a different sequence.

/**
 * True when `incoming` must not replace `displayed`.
 *
 * Both are `{ documentId, token }`. A token that is missing on either side means the order cannot
 * be established, and the caller keeps its behaviour rather than inventing a comparison: an app
 * build that does not send tokens still works, it just cannot be protected by this rule. A
 * *different* document is never superseded - its token belongs to another sequence.
 */
export function isSuperseded(incoming, displayed) {
  if (
    !incoming ||
    !displayed ||
    typeof incoming.token !== "number" ||
    typeof displayed.token !== "number"
  ) {
    return false;
  }
  if (
    typeof incoming.documentId === "string" &&
    typeof displayed.documentId === "string" &&
    incoming.documentId !== displayed.documentId
  ) {
    return false;
  }
  return incoming.token <= displayed.token;
}

/** The key one document's sequence is tracked under; an unknown id has its own sequence. */
function sequenceKey(documentId) {
  return typeof documentId === "string" && documentId.length > 0 ? documentId : "";
}

/** Tracks the newest publication token a window has seen, per document. */
export class PublicationOrder {
  constructor() {
    // documentId -> { newest, displayed }
    this.sequences = new Map();
  }

  sequence(documentId) {
    const key = sequenceKey(documentId);
    let entry = this.sequences.get(key);
    if (!entry) {
      entry = { newest: null, displayed: null };
      this.sequences.set(key, entry);
    }
    return entry;
  }

  /** Records a publication the app announced for a document. */
  observe(documentId, token) {
    if (typeof token !== "number") {
      return true;
    }
    const entry = this.sequence(documentId);
    if (entry.newest === null || token > entry.newest) {
      entry.newest = token;
      return true;
    }
    return token === entry.newest;
  }

  /** True when this token has been overtaken by a newer publication *of the same document*. */
  stale(documentId, token) {
    const entry = this.sequence(documentId);
    return entry.newest !== null && typeof token === "number" && token < entry.newest;
  }

  /** Records the token of a document whose geometry is on screen. */
  markDisplayed(documentId, token) {
    if (typeof token === "number") {
      this.sequence(documentId).displayed = token;
    }
  }

  /** True when this load may replace what is on screen, for its own document. */
  mayDisplay(documentId, token) {
    if (typeof token !== "number") {
      return true;
    }
    const entry = this.sequence(documentId);
    return !isSuperseded(
      { documentId, token },
      entry.displayed === null ? null : { documentId, token: entry.displayed },
    );
  }

  /** The newest token seen for a document, or `null` when none was. */
  newestFor(documentId) {
    return this.sequence(documentId).newest;
  }

  /** The token displayed for a document, or `null` when none was. */
  displayedFor(documentId) {
    return this.sequence(documentId).displayed;
  }

  /** Forgets a document that is no longer open. */
  forget(documentId) {
    this.sequences.delete(sequenceKey(documentId));
  }
}
