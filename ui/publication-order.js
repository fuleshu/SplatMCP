// Publication ordering, in one dependency-free place.
//
// A revision is published by the app with a monotonically increasing token, and the window then
// fetches and displays it. Two things can arrive out of order:
//
// - a *newer* publication while an older one is still fetching, and
// - an older publication whose fetch resolves *after* a newer one is already on screen.
//
// The first case resolves itself (the newer load wins), but the second is the dangerous one: the
// late older load would become the newest request the viewer saw and replace newer geometry with
// stale pixels. Comparing tokens, rather than arrival order, is what makes the rule independent
// of scheduling.

/**
 * True when `incoming` must not replace `displayed`.
 *
 * Both are publication tokens. A token that is missing on either side means the order cannot be
 * established, and the caller keeps its previous behaviour rather than inventing a comparison:
 * an app build that does not send tokens still works, it just cannot be protected by this rule.
 */
export function isSuperseded(incoming, displayed) {
  if (typeof incoming !== "number" || typeof displayed !== "number") {
    return false;
  }
  return incoming <= displayed;
}

/**
 * Tracks the newest publication token a window has seen.
 *
 * The window observes publications in arbitrary order, so "newest" is a maximum, not "the last
 * one I heard about": an older event must not lower the bar and let its own load through.
 */
export class PublicationOrder {
  constructor() {
    this.newest = null;
    this.displayed = null;
  }

  /** Records a publication the app announced, and returns true when it is the newest so far. */
  observe(token) {
    if (typeof token !== "number") {
      return true;
    }
    if (this.newest === null || token > this.newest) {
      this.newest = token;
      return true;
    }
    return token === this.newest;
  }

  /** True when this token has been overtaken by a newer publication. */
  stale(token) {
    return this.newest !== null && typeof token === "number" && token < this.newest;
  }

  /** Records the token whose geometry is on screen. */
  markDisplayed(token) {
    if (typeof token === "number") {
      this.displayed = token;
    }
  }

  /** True when this load may replace what is on screen. */
  mayDisplay(token) {
    if (typeof token !== "number") {
      return true;
    }
    return !isSuperseded(token, this.displayed);
  }
}
