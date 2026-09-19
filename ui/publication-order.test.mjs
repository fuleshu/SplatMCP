// Checks the publication ordering rule, because the defect it fixes is a *comparison* bug and a
// syntax check cannot see it: tokens are minted per document, so a cross-document comparison
// silently dropped a new document's early publications.
//
// Run with: node ui/publication-order.test.mjs
import { isSuperseded, PublicationOrder } from "./publication-order.js";

let failures = 0;
function check(label, condition) {
  if (condition) {
    console.log(`ok   ${label}`);
  } else {
    failures += 1;
    console.log(`FAIL ${label}`);
  }
}

// --- The reported defect: a second document's first publication ---------------------------------
// Document A reached token 4 and is displayed.
const order = new PublicationOrder();
order.observe("doc-a", 1);
order.observe("doc-a", 2);
order.observe("doc-a", 3);
order.observe("doc-a", 4);
order.markDisplayed("doc-a", 4);

// Document B's first publication is token 1: it is a different sequence and must not be stale.
check("B token 1 is not stale while A is at 4", !order.stale("doc-b", 1));
check("B token 1 may display", order.mayDisplay("doc-b", 1));
check("viewer accepts B@1 over A's displayed token 4", !isSuperseded(
  { documentId: "doc-b", token: 1 },
  { documentId: "doc-a", token: 4 },
));
// And the whole B sequence must be displayable without extra edits.
for (const token of [1, 2, 3, 4, 5]) {
  check(`B token ${token} may display`, order.mayDisplay("doc-b", token));
}

// --- Within one document the rule still holds --------------------------------------------------
check("A token 3 is stale while A is at 4", order.stale("doc-a", 3));
check("A token 5 is not stale", !order.stale("doc-a", 5));
check("viewer refuses the same document's older token", isSuperseded(
  { documentId: "doc-a", token: 3 },
  { documentId: "doc-a", token: 4 },
));
check("viewer accepts the same document's newer token", !isSuperseded(
  { documentId: "doc-a", token: 5 },
  { documentId: "doc-a", token: 4 },
));
check("a token equal to the displayed one is refused", isSuperseded(
  { documentId: "doc-a", token: 4 },
  { documentId: "doc-a", token: 4 },
));

// --- Missing identities keep the previous behaviour ---------------------------------------------
check("no request is never superseded", !isSuperseded(null, { documentId: "doc-a", token: 4 }));
check("nothing displayed is never superseded", !isSuperseded(
  { documentId: "doc-a", token: 1 },
  null,
));
check("a tokenless publication is never superseded", !isSuperseded(
  { documentId: "doc-a", token: null },
  { documentId: "doc-a", token: 4 },
));
check("an unknown document id keeps its own sequence", !order.stale(undefined, 1));

// --- Per-document maxima ------------------------------------------------------------------------
// Nothing was *observed* for B yet (the loop above only asked whether it may display), so its
// maximum is its own and still empty: A's publications did not set it.
check("A's maximum is its own", order.newestFor("doc-a") === 4);
check("B's maximum is still unset", order.newestFor("doc-b") === null);
order.observe("doc-b", 7);
check("observing B does not lower or raise A's maximum", order.newestFor("doc-a") === 4);
check("B's maximum is its own", order.newestFor("doc-b") === 7);
check("displayed is per document", order.displayedFor("doc-a") === 4);
order.markDisplayed("doc-b", 7);
check("marking B displayed does not change A's displayed token", order.displayedFor("doc-a") === 4);
order.forget("doc-b");
check("forgetting a document clears only that sequence", order.newestFor("doc-b") === null);

console.log(failures === 0 ? "\npublication-order: ok" : `\npublication-order: ${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
