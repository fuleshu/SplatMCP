// Checks the readiness rule that stops a capture reporting a blank frame as a success.
//
// The live failure this covers: right after an edit, one frame had been rendered, the pinned
// revision was already the displayed one, and the readback was empty - so the capture answered
// "ok" with a dark image.
//
// Run with: node ui/capture-readiness.test.mjs

import assert from "node:assert/strict";
import { READINESS, expired, frameIsFlat, readinessVerdict } from "./capture-readiness.js";

let checks = 0;
const check = (name, fn) => {
  fn();
  checks += 1;
  process.stdout.write(`ok ${checks} - ${name}\n`);
};

/** A frame of one colour per pixel, as the drawing buffer reports it. */
const flat = (width, height, [r, g, b, a] = [3, 4, 5, 255]) =>
  Uint8ClampedArray.from({ length: width * height * 4 }, (_, index) =>
    index % 4 === 0 ? r : index % 4 === 1 ? g : index % 4 === 2 ? b : a,
  );
const drawn = (width, height) => {
  const pixels = flat(width, height);
  pixels[0] = 200;
  pixels[1] = 40;
  pixels[2] = 10;
  return pixels;
};

check("a single flat frame is recognised, a drawn one is not", () => {
  assert.equal(frameIsFlat(flat(4, 4)), true);
  assert.equal(frameIsFlat(drawn(4, 4)), false);
  assert.equal(frameIsFlat(new Uint8ClampedArray(0)), true, "no pixels is not a picture");
});

check("a revision that is still coming is waited for, not captured", () => {
  // An edit immediately followed by a capture looks like this: the app pinned the new revision and
  // the viewer has not swapped to it yet. Waiting is what makes that workflow work.
  const verdict = readinessVerdict({
    expectedDocumentId: "doc-1-1",
    expectedRevision: 5,
    displayedDocumentId: "doc-1-1",
    displayedRevision: 4,
    stagedRevision: 5,
  });
  assert.equal(verdict.status, READINESS.Waiting);
  assert.match(verdict.reason, /still being prepared/);

  // No staging state reported, but the pinned revision is ahead: still a wait, because a
  // publication may be in flight.
  const other = readinessVerdict({
    expectedRevision: 5,
    displayedRevision: 4,
    stagedRevision: null,
  });
  assert.equal(other.status, READINESS.Waiting);
  assert.match(other.reason, /showing revision 4 while revision 5 was pinned/);
});

check("a revision the document has moved past is refused at once", () => {
  // The mirror image: the caller pinned an older revision, which can never become current again.
  // Waiting for it would waste the whole timeout before saying the same thing.
  const verdict = readinessVerdict({
    expectedDocumentId: "doc-1-1",
    expectedRevision: 3,
    displayedDocumentId: "doc-1-1",
    displayedRevision: 5,
  });
  assert.equal(verdict.status, READINESS.Failed);
  assert.match(verdict.reason, /the document moved on: expected revision 3, current 5/);
});

check("a different document is a failure, not a wait", () => {
  const verdict = readinessVerdict({
    expectedDocumentId: "doc-1-1",
    expectedRevision: 5,
    displayedDocumentId: "doc-9-9",
    displayedRevision: 5,
  });
  assert.equal(verdict.status, READINESS.Failed);
  assert.match(verdict.reason, /showing document doc-9-9/);
});

check("a swap that has not completed a frame is waited for", () => {
  const verdict = readinessVerdict({
    expectedRevision: 5,
    displayedRevision: 5,
    uploadPending: true,
    pointCount: 5,
    pixels: drawn(4, 4),
  });
  assert.equal(verdict.status, READINESS.Waiting);
  assert.match(verdict.reason, /has not completed a frame/);
});

check("the blank-frame case is waited for, and succeeds once content appears", () => {
  const base = {
    expectedDocumentId: "doc-1-1",
    expectedRevision: 5,
    displayedDocumentId: "doc-1-1",
    displayedRevision: 5,
    uploadPending: false,
    contentToken: 7,
    provenToken: -1,
    pointCount: 5,
  };
  // Content token 7 has never been seen to render, and the frame is an upload frame.
  const blank = readinessVerdict({ ...base, pixels: flat(4, 4) });
  assert.equal(blank.status, READINESS.Waiting);
  assert.match(blank.reason, /single flat colour although the pinned revision has 5 gaussians/);

  // The same content, once drawn: ready.
  assert.equal(readinessVerdict({ ...base, pixels: drawn(4, 4) }).status, READINESS.Ready);
});

check("an empty document is allowed to render flat", () => {
  const verdict = readinessVerdict({
    expectedRevision: 5,
    displayedRevision: 5,
    pointCount: 0,
    contentToken: 7,
    provenToken: -1,
    pixels: flat(4, 4),
  });
  assert.equal(verdict.status, READINESS.Ready, "a document with no gaussians has no content to prove");
});

check("content already proven does not have to prove itself again", () => {
  // A steady-state capture of a view that legitimately shows nothing must not loop until its
  // timeout: once this content has been seen to render, a flat frame is accepted.
  const verdict = readinessVerdict({
    expectedRevision: 5,
    displayedRevision: 5,
    contentToken: 7,
    provenToken: 7,
    pointCount: 5,
    pixels: flat(4, 4),
  });
  assert.equal(verdict.status, READINESS.Ready);
});

check("a host that cannot read pixels falls back to the renderer's own evidence", () => {
  const verdict = readinessVerdict({
    expectedRevision: 5,
    displayedRevision: 5,
    contentToken: 8,
    provenToken: -1,
    pointCount: 5,
    pixels: null,
  });
  assert.equal(verdict.status, READINESS.Ready, "no pixels means no content check, not a wait");
});

check("a wait that runs out of time is a failure, not a success", () => {
  assert.equal(expired(1000, 1500, 400), true);
  assert.equal(expired(1000, 1200, 400), false);
});

process.stdout.write(`\ncapture-readiness: ${checks} checks passed\n`);
