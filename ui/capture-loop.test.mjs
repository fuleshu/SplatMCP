// Checks the frame-capture loop: it waits for the renderer's evidence, and its failure says which
// condition never became true.
//
// Run with: node ui/capture-loop.test.mjs

import assert from "node:assert/strict";
import { awaitRenderEvidence, capturePinnedFrame, nextRenderedFrame } from "./capture.js";

let checks = 0;
const check = async (name, fn) => {
  await fn();
  checks += 1;
  process.stdout.write(`ok ${checks} - ${name}\n`);
};

/**
 * A viewer whose readiness changes as frames are rendered.
 *
 * `scenario` is the list of readiness states the engine passes through; each render takes the next
 * one, which is how the real failure looked: the swap was in, one frame had been drawn, and the
 * frame was still empty.
 */
function fakeViewer(scenario, { pixels = null } = {}) {
  const state = {
    ready: scenario[0],
    frame: 0,
    renders: 0,
    listeners: new Set(),
  };
  const app = {
    render() {
      state.renders += 1;
      const next = scenario[Math.min(state.frame + 1, scenario.length - 1)];
      state.frame += 1;
      state.ready = next;
      for (const listener of [...state.listeners]) {
        listener();
      }
    },
    once(event, listener) {
      if (event !== "postrender") {
        return;
      }
      state.listeners.add(listener);
    },
    off(event, listener) {
      state.listeners.delete(listener);
    },
    graphicsDevice: { maxPixelRatio: 1 },
    resizeCanvas() {},
  };
  const canvas = { width: 64, height: 48, getContext: () => null, toDataURL: () => "data:image/png;base64,ZnJhbWU=" };
  return {
    app,
    canvas,
    state,
    contentReadiness: () => state.ready,
    handleResize() {},
    worldBounds: () => ({ min: [-1, -1, -1], max: [1, 1, 1] }),
  };
}

const staged = {
  content_token: 4,
  upload_pending: true,
  staged_revision: 7,
  staged_document_id: "doc-1-1",
  displayed_revision: 6,
  displayed_document_id: "doc-1-1",
  point_count: 5,
};
const swappedNotDrawn = { ...staged, upload_pending: false, staged_revision: null, displayed_revision: 7 };
const drawn = { ...swappedNotDrawn, content_token: 5, point_count: 5 };

const flat = new Uint8ClampedArray(64 * 48 * 4).fill(9);
const content = new Uint8ClampedArray(64 * 48 * 4).fill(9);
content[0] = 250;

await check("the loop waits through staging, swap and upload, then reads", async () => {
  const viewer = fakeViewer([staged, swappedNotDrawn, swappedNotDrawn, drawn]);
  // The readback reports an empty frame until the content is drawn, exactly as a real upload frame
  // does.
  let reads = 0;
  const frame = await awaitRenderEvidence(viewer, {
    expectedDocumentId: "doc-1-1",
    expectedRevision: 7,
    timeoutMs: 2000,
    provenToken: -1,
    readback: () => {
      reads += 1;
      const pixels = viewer.state.ready === drawn ? content : flat;
      return { pixels, width: 64, height: 48 };
    },
  });
  assert.equal(reads >= 3, true, `the loop kept reading until the frame held the document (${reads})`);
  assert.equal(frame.attempts >= 3, true);
  assert.equal(viewer.state.upload_pending, undefined);
});

await check("a renderer that never shows the pinned revision fails with the reason", async () => {
  const stuck = { ...staged, staged_revision: null, displayed_revision: 6, upload_pending: false };
  const viewer = fakeViewer([stuck]);
  const error = await awaitRenderEvidence(viewer, {
    expectedRevision: 7,
    timeoutMs: 30,
    readback: () => ({ pixels: flat, width: 64, height: 48 }),
  }).catch((thrown) => thrown);
  assert.match(error.message, /no frame of revision 7 could be captured/);
  assert.match(error.message, /showing revision 6 while revision 7 was pinned/);
});

await check("a blank frame for a full document times out as a failure, never a success", async () => {
  const viewer = fakeViewer([swappedNotDrawn, drawn]);
  const error = await awaitRenderEvidence(viewer, {
    expectedRevision: 7,
    timeoutMs: 30,
    provenToken: -1,
    readback: () => ({ pixels: flat, width: 64, height: 48 }),
  }).catch((thrown) => thrown);
  assert.match(error.message, /single flat colour although the pinned revision has 5 gaussians/);
});

await check("once content is proven, a flat frame is read straight away", async () => {
  const viewer = fakeViewer([drawn]);
  const frame = await awaitRenderEvidence(viewer, {
    expectedRevision: 7,
    timeoutMs: 100,
    provenToken: 5,
    readback: () => ({ pixels: flat, width: 64, height: 48 }),
  });
  assert.equal(frame.attempts, 1);
});

await check("a pinned frame applies the size, reads it and reports what it produced", async () => {
  const resized = [];
  const viewer = fakeViewer([drawn]);
  viewer.app.resizeCanvas = (width, height) => resized.push([width, height]);
  const frame = await capturePinnedFrame(viewer, {
    viewport: { width: 320, height: 240 },
    expectedRevision: 7,
    timeoutMs: 100,
  });
  assert.deepEqual(resized[0], [320, 240], "the requested size is applied before the read");
  assert.equal(frame.width, 64, "the produced size is what the canvas actually has");
  assert.equal(frame.capped, true, "a size the renderer did not honour is reported as capped");
  assert.equal(frame.mime_type, "image/png");
  assert.ok(frame.data_base64.length > 0);
  // The window is put back the way it was.
  assert.equal(resized.length > 1, true);
});

await check("nextRenderedFrame resolves when the engine finishes a frame", async () => {
  const viewer = fakeViewer([drawn]);
  let resolved = false;
  const pending = nextRenderedFrame(viewer.app).then(() => {
    resolved = true;
  });
  await pending;
  assert.equal(resolved, true);
  assert.equal(viewer.state.renders >= 1, true, "asking for a frame is what makes one happen");
});

process.stdout.write(`\ncapture-loop: ${checks} checks passed\n`);
